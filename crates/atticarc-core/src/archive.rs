//! Archiving, verifying and the checks that guard deleting a folder.
//!
//! Engine choice follows the sevenz-bench results: sevenz-turbo, LZMA2,
//! one archive at a time, 32 MiB chunks at level 5. Thread count is a
//! setting (all cores by default).
//!
//! Unlike [`crate::scan`], the scan here fails on anything it can't read:
//! a partial archive would be unsafe to delete from.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use sevenz_turbo::encoder_options::Lzma2Options;
use sevenz_turbo::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod, Password,
    SourceReader,
};
use sha2::{Digest, Sha256};

use serde::{Deserialize, Serialize};

use crate::catalog::{ArchiveRecord, Catalog, DATA_DIR, EntryRecord, now_utc, path_key};
use crate::exclude::{Action, Exclusions, Rule};

/// The manifest's path inside every archive: what the archive holds, with
/// every file's hashes, so the record travels with the archive and the
/// catalogue can be rebuilt from it.
pub const MANIFEST_ENTRY: &str = ".atticarc/manifest.json";

/// What an archive's manifest holds. Written inside the archive before it
/// is finished, so `archive.archive_path` and `archive.archive_bytes` are
/// empty there; [`read_manifest`] fills them in.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub archive: ArchiveRecord,
    pub entries: Vec<EntryRecord>,
    /// Files the exclusions left out of this archive.
    #[serde(default)]
    pub excluded: Excluded,
}

/// Files left out of an archive by the exclusions, recorded so the folder
/// can still be checked before it is deleted.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Excluded {
    /// File name of the archive beside this one holding the set-aside files.
    pub set_aside_archive: Option<String>,
    pub set_aside: Vec<EntryRecord>,
    /// Files archived nowhere, deleted with the folder.
    pub discarded: Vec<EntryRecord>,
}

/// Files at least this big go in a block of their own; smaller files are
/// packed into solid blocks that stay under it. Same as sevenz-turbo's
/// own directory helper, which the benchmark used.
const MAX_BLOCK: u64 = 4 * 1024 * 1024 * 1024;

/// Extensions of formats that are already compressed. In store mode these
/// are copied into the archive without compressing them again.
const STORE_EXTS: &[&str] = &[
    "7z", "zip", "rar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "cab", "whl", "jar", "apk", "nupkg",
    "docx", "xlsx", "pptx", "odt", "epub", "jpg", "jpeg", "png", "gif", "webp", "heic", "avif", "mp3",
    "aac", "m4a", "ogg", "opus", "flac", "mp4", "m4v", "mkv", "mov", "avi", "webm", "wmv", "safetensors",
    "gguf", "pdf", "msi", "dmg",
];

#[derive(Clone, Debug)]
pub struct Settings {
    pub out_dir: PathBuf,
    pub level: u32,
    pub store_mode: bool,
    pub threads: u32,
    /// Folders the app never deletes from, whatever their state.
    pub protected: Vec<PathBuf>,
    /// Files to leave out of archives.
    pub exclusions: Vec<Rule>,
}

impl Settings {
    pub fn engine_label(&self) -> String {
        format!("sevenz-turbo 0.29.3, LZMA2 level {}", self.level)
    }
}

pub fn default_threads() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4)
}

/// Shared between a worker and the window, so the window can show progress
/// and ask the worker to stop.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    pub phase: Mutex<String>,
}

impl Progress {
    pub fn set_phase(&self, phase: &str, total: u64) {
        *self.phase.lock().unwrap() = phase.to_string();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 { 0.0 } else { (self.done.load(Ordering::Relaxed) as f64 / t as f64).min(1.0) as f32 }
    }

    fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::other("cancelled"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub abs: PathBuf,
    pub rel: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub files: Vec<ScannedFile>,
    /// Folders below the root, root excluded.
    pub dirs: Vec<(PathBuf, String)>,
    pub total_bytes: u64,
    /// Links and junctions, which are not followed or archived.
    pub skipped_links: Vec<String>,
    /// Files the exclusions set aside for the second archive.
    pub set_aside: Vec<ScannedFile>,
    /// Files the exclusions discard.
    pub discarded: Vec<ScannedFile>,
}

/// Walks `root` without following links or junctions. Any folder or file
/// that can't be listed is an error: a partial archive would be unsafe to
/// delete from.
pub fn scan(root: &Path) -> Result<Scan> {
    scan_excluding(root, &Exclusions::default())
}

/// [`scan`], leaving out the files `exclusions` match.
pub fn scan_excluding(root: &Path, exclusions: &Exclusions) -> Result<Scan> {
    let mut s = Scan::default();
    // A top-level .atticarc is a manifest left by extracting an archive; the
    // new archive gets its own instead.
    let walk = walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !(e.depth() == 1 && e.file_name() == DATA_DIR));
    for item in walk {
        let item = item.with_context(|| format!("can't read inside {}", root.display()))?;
        if item.depth() == 0 {
            continue;
        }
        let rel = rel_name(root, item.path())?;
        let ft = item.file_type();
        if ft.is_symlink() {
            s.skipped_links.push(rel);
        } else if ft.is_dir() {
            s.dirs.push((item.path().to_path_buf(), rel));
        } else if ft.is_file() {
            let meta = item.metadata().with_context(|| format!("can't read {}", item.path().display()))?;
            let action = exclusions.action(&rel);
            let f = ScannedFile {
                abs: item.path().to_path_buf(),
                rel,
                size: meta.len(),
                modified: meta.modified().ok(),
                created: meta.created().ok(),
            };
            match action {
                None => {
                    s.total_bytes += f.size;
                    s.files.push(f);
                }
                Some(Action::SetAside) => s.set_aside.push(f),
                Some(Action::Discard) => s.discarded.push(f),
            }
        }
    }
    Ok(s)
}

/// Total size and file count only, for the folder list.
pub fn quick_size(root: &Path) -> (u64, u64) {
    let mut bytes = 0;
    let mut files = 0;
    for item in walkdir::WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if item.file_type().is_file() {
            files += 1;
            bytes += item.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (bytes, files)
}

fn rel_name(root: &Path, p: &Path) -> Result<String> {
    let rel = p.strip_prefix(root)?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(parts.join("/"))
}

fn ext_of(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map(|(_, x)| x.to_lowercase()).unwrap_or_default()
}

fn utc(t: Option<SystemTime>) -> Option<String> {
    t.map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[derive(Clone, Debug)]
struct FileHash {
    blake3: String,
    sha256: String,
    bytes: u64,
}

/// Opens its file on first read and hashes every byte it hands to the
/// compressor, so each source byte is read from disk once.
struct HashReader {
    path: PathBuf,
    file: Option<File>,
    slot: usize,
    b3: blake3::Hasher,
    sha: Sha256,
    bytes: u64,
    finished: bool,
    out: Arc<Mutex<Vec<Option<FileHash>>>>,
    progress: Arc<Progress>,
}

impl Read for HashReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.progress.check_cancel()?;
        if self.file.is_none() {
            let f = File::open(&self.path)
                .map_err(|e| io::Error::new(e.kind(), format!("can't open {}: {e}", self.path.display())))?;
            self.file = Some(f);
        }
        let n = self.file.as_mut().unwrap().read(buf)?;
        if n == 0 {
            if !self.finished {
                self.finished = true;
                let hash = FileHash {
                    blake3: std::mem::take(&mut self.b3).finalize().to_hex().to_string(),
                    sha256: hex(&std::mem::take(&mut self.sha).finalize()),
                    bytes: self.bytes,
                };
                self.out.lock().unwrap()[self.slot] = Some(hash);
                self.file = None;
            }
            return Ok(0);
        }
        self.b3.update(&buf[..n]);
        self.sha.update(&buf[..n]);
        self.bytes += n as u64;
        self.progress.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn lzma2_config(level: u32, threads: u32) -> EncoderConfiguration {
    // Same profiles as the benchmark: level 5 with 32 MiB chunks, level 9
    // with a 64 MiB dictionary and 256 MiB chunks.
    let mut opts = if threads <= 1 {
        Lzma2Options::from_level(level)
    } else if level >= 9 {
        Lzma2Options::from_level_mt(level, threads, 256 << 20)
    } else {
        Lzma2Options::from_level_mt(level, threads, 32 << 20)
    };
    if level >= 9 {
        opts.set_dictionary_size(64 << 20);
    }
    opts.into()
}

/// A written archive's entries and size.
type Written = (Vec<EntryRecord>, u64);

pub struct ArchiveOutcome {
    pub record: ArchiveRecord,
    pub skipped_links: Vec<String>,
    /// What the exclusions left out, as recorded in the manifest.
    pub excluded: Excluded,
}

/// Archives `source` into `settings.out_dir`, verifies the archive by
/// decoding it, and only then records it in the catalog. Files the
/// exclusions set aside go into a second archive, `<name>.excluded.7z`,
/// verified the same way; discarded files are hashed and listed in the
/// main archive's manifest.
pub fn archive_folder(source: &Path, settings: &Settings, progress: &Arc<Progress>) -> Result<ArchiveOutcome> {
    let started = now_utc();
    let exclusions = Exclusions::new(&settings.exclusions).context("an exclusion can't be used")?;
    let source = std::fs::canonicalize(source).with_context(|| format!("can't open {}", source.display()))?;
    // Check before creating the output folder, so a protected folder never
    // gains one, and again after, once the path can be fully resolved.
    let refuse = || anyhow!("the output folder {} is a protected folder; choose another one", settings.out_dir.display());
    let wanted = std::path::absolute(&settings.out_dir).unwrap_or_else(|_| settings.out_dir.clone());
    if is_protected(&wanted, settings) {
        return Err(refuse());
    }
    std::fs::create_dir_all(&settings.out_dir)?;
    if is_protected(&settings.out_dir, settings) {
        return Err(refuse());
    }
    let out_dir = std::fs::canonicalize(&settings.out_dir)?;
    if out_dir.starts_with(&source) {
        bail!("the output folder is inside {}; choose another output folder", source.display());
    }
    let name = source
        .file_name()
        .ok_or_else(|| anyhow!("can't archive a drive root"))?
        .to_string_lossy()
        .into_owned();
    let final_path = unique_archive_path(&out_dir, &name);
    let partial = PathBuf::from(format!("{}.partial", final_path.display()));

    progress.set_phase("Scanning", 0);
    let scan = scan_excluding(&source, &exclusions)?;

    let source_display = crate::settings::strip_verbatim(&source).to_string_lossy().into_owned();
    let record_for = |name: String, source_path: String, scan: &Scan| ArchiveRecord {
        name,
        archive_path: String::new(),
        source_path,
        source_display: source_display.clone(),
        created_utc: now_utc(),
        engine: settings.engine_label(),
        level: settings.level,
        threads: settings.threads,
        input_bytes: scan.total_bytes,
        archive_bytes: 0,
        file_count: scan.files.len() as u64,
        dir_count: scan.dirs.len() as u64,
    };
    let mut record = record_for(name.clone(), path_key(&source), &scan);

    // The set-aside files, as a scan of their own for the second archive.
    let aside_scan = Scan {
        total_bytes: scan.set_aside.iter().map(|f| f.size).sum(),
        files: scan.set_aside.clone(),
        ..Scan::default()
    };
    let aside_path = (!aside_scan.files.is_empty()).then(|| unique_archive_path(&out_dir, &format!("{name}.excluded")));
    let aside_partial = aside_path.as_ref().map(|p| PathBuf::from(format!("{}.partial", p.display())));
    // Kept apart from the main archive's key, so the folder list and the
    // delete flow only ever find the main archive.
    let mut aside_record = record_for(format!("{name} (excluded)"), format!("{}|excluded", path_key(&source)), &aside_scan);

    let result = (|| -> Result<(Written, Excluded, Option<Written>)> {
        let mut excluded = Excluded::default();
        if !scan.discarded.is_empty() {
            progress.set_phase("Hashing discarded files", scan.discarded.iter().map(|f| f.size).sum());
            excluded.discarded = scan.discarded.iter().map(|f| hash_record(f, progress)).collect::<Result<_>>()?;
        }
        let mut aside = None;
        if let (Some(path), Some(partial)) = (&aside_path, &aside_partial) {
            progress.set_phase("Compressing set-aside files", aside_scan.total_bytes);
            let entries = write_archive(&aside_scan, partial, settings, progress, &aside_record, &Excluded::default())?;
            progress.set_phase("Verifying set-aside files", aside_scan.total_bytes);
            verify_archive(partial, &entries, settings.threads, progress)?;
            excluded.set_aside_archive = path.file_name().map(|n| n.to_string_lossy().into_owned());
            excluded.set_aside = entries.clone();
            aside = Some((entries, std::fs::metadata(partial)?.len()));
        }
        progress.set_phase("Compressing", scan.total_bytes);
        let entries = write_archive(&scan, &partial, settings, progress, &record, &excluded)?;
        progress.set_phase("Verifying", scan.total_bytes);
        verify_archive(&partial, &entries, settings.threads, progress)?;
        let archive_bytes = std::fs::metadata(&partial)?.len();
        Ok(((entries, archive_bytes), excluded, aside))
    })();

    let ((entries, archive_bytes), excluded, aside) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            if let Some(p) = &aside_partial {
                let _ = std::fs::remove_file(p);
            }
            if let Ok(cat) = Catalog::open(&out_dir) {
                let _ = cat.log_run(None, "archive", &source, &started, "failed", &format!("{e:#}"));
            }
            return Err(e);
        }
    };

    let mut cat = Catalog::open(&out_dir)?;
    if let (Some(path), Some(partial), Some((aside_entries, bytes))) = (&aside_path, &aside_partial, aside) {
        std::fs::rename(partial, path).with_context(|| format!("can't rename {} to {}", partial.display(), path.display()))?;
        aside_record.archive_path = crate::settings::strip_verbatim(path).to_string_lossy().into_owned();
        aside_record.archive_bytes = bytes;
        cat.insert_archive(&aside_record, &aside_entries)?;
    }
    std::fs::rename(&partial, &final_path)
        .with_context(|| format!("can't rename {} to {}", partial.display(), final_path.display()))?;
    record.archive_path = crate::settings::strip_verbatim(&final_path).to_string_lossy().into_owned();
    record.archive_bytes = archive_bytes;

    let archive_id = cat.insert_archive(&record, &entries)?;
    cat.insert_excluded(archive_id, &excluded)?;
    let mut detail = Vec::new();
    if !scan.skipped_links.is_empty() {
        detail.push(format!("links not followed: {}", scan.skipped_links.join(", ")));
    }
    let names = |v: &[EntryRecord]| v.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>().join(", ");
    if let Some(a) = &excluded.set_aside_archive {
        detail.push(format!("set aside in {a}: {}", names(&excluded.set_aside)));
    }
    if !excluded.discarded.is_empty() {
        detail.push(format!("discarded: {}", names(&excluded.discarded)));
    }
    let detail = detail.join("; ");
    cat.log_run(Some(archive_id), "archive", &source, &started, "verified", &detail)?;
    Ok(ArchiveOutcome { record, skipped_links: scan.skipped_links, excluded })
}

/// Hashes one file that isn't archived, for the manifest.
fn hash_record(f: &ScannedFile, progress: &Arc<Progress>) -> Result<EntryRecord> {
    let mut b3 = blake3::Hasher::new();
    let mut sha = Sha256::new();
    let mut file = File::open(&f.abs).with_context(|| format!("can't open {}", f.abs.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        progress.check_cancel()?;
        let k = file.read(&mut buf)?;
        if k == 0 {
            break;
        }
        b3.update(&buf[..k]);
        sha.update(&buf[..k]);
        n += k as u64;
        progress.done.fetch_add(k as u64, Ordering::Relaxed);
    }
    if n != f.size {
        bail!("{} changed while it was being read; try again", f.rel);
    }
    Ok(EntryRecord {
        rel_path: f.rel.clone(),
        is_dir: false,
        size: f.size,
        modified_utc: utc(f.modified),
        created_utc: utc(f.created),
        blake3: Some(b3.finalize().to_hex().to_string()),
        sha256: Some(hex(&sha.finalize())),
        method: "discard".into(),
    })
}

/// Where earlier versions wrote an archive's manifest: a file beside it.
fn old_manifest_path(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.manifest.json", archive.display()))
}

/// Reads the manifest inside `archive`, or for archives made before
/// manifests went inside, the `.manifest.json` file beside it. Only the
/// manifest's own small block is decoded.
pub fn read_manifest(archive: &Path) -> Result<Manifest> {
    let mut reader = ArchiveReader::open(archive, Password::empty()).with_context(|| format!("can't open {}", archive.display()))?;
    let bytes = match reader.read_file(MANIFEST_ENTRY) {
        Ok(b) => b,
        Err(_) => {
            // Beside the archive, or moved into the .atticarc folder with the catalogue.
            let beside = old_manifest_path(archive);
            let moved = archive.parent().zip(beside.file_name()).map(|(d, n)| d.join(DATA_DIR).join(n));
            std::fs::read(&beside)
                .or_else(|e| moved.map_or(Err(e), std::fs::read))
                .map_err(|_| anyhow!("{} has no AtticARC manifest", archive.display()))?
        }
    };
    let mut m: Manifest = serde_json::from_slice(&bytes).with_context(|| format!("the manifest in {} can't be read", archive.display()))?;
    m.archive.archive_path = crate::settings::strip_verbatim(archive).to_string_lossy().into_owned();
    m.archive.archive_bytes = std::fs::metadata(archive)?.len();
    Ok(m)
}

/// What rebuilding a catalogue found.
#[derive(Debug, Default)]
pub struct Rebuilt {
    pub added: u64,
    pub already_there: u64,
    /// Archives that couldn't be added, with the reason.
    pub failed: Vec<String>,
}

/// Adds every .7z in `out_dir` that isn't in its catalogue yet, from the
/// manifests the archives carry. Archives already catalogued are left alone.
pub fn rebuild_catalog(out_dir: &Path) -> Result<Rebuilt> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .with_context(|| format!("can't read {}", out_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("7z")))
        .collect();
    found.sort();
    let mut out = Rebuilt::default();
    if found.is_empty() {
        return Ok(out);
    }
    let mut cat = Catalog::open(out_dir)?;
    for path in found {
        let key = crate::settings::strip_verbatim(&path).to_string_lossy().into_owned();
        if cat.has_archive_path(&key)? {
            out.already_there += 1;
            continue;
        }
        let added = read_manifest(&path).and_then(|m| {
            let id = cat.insert_archive(&m.archive, &m.entries)?;
            cat.insert_excluded(id, &m.excluded)
        });
        match added {
            Ok(_) => out.added += 1,
            Err(e) => out.failed.push(format!("{}: {e:#}", path.file_name().unwrap_or_default().to_string_lossy())),
        }
    }
    Ok(out)
}

fn unique_archive_path(out_dir: &Path, name: &str) -> PathBuf {
    let plain = out_dir.join(format!("{name}.7z"));
    if !plain.exists() {
        return plain;
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S");
    out_dir.join(format!("{name} ({stamp}).7z"))
}

/// Writes the archive, with the manifest describing `record` and the files
/// last in a small block of its own so it can be read without decoding the
/// rest.
fn write_archive(
    scan: &Scan,
    dest: &Path,
    settings: &Settings,
    progress: &Arc<Progress>,
    record: &ArchiveRecord,
    excluded: &Excluded,
) -> Result<Vec<EntryRecord>> {
    let hashes: Arc<Mutex<Vec<Option<FileHash>>>> = Arc::new(Mutex::new(vec![None; scan.files.len()]));
    let mut w = ArchiveWriter::create(dest).with_context(|| format!("can't create {}", dest.display()))?;

    for (abs, rel) in &scan.dirs {
        w.push_archive_entry::<File>(ArchiveEntry::from_path(abs, rel.clone()), None)?;
    }

    let is_store = |f: &ScannedFile| settings.store_mode && STORE_EXTS.contains(&ext_of(&f.rel).as_str());
    let (store, pack): (Vec<usize>, Vec<usize>) = (0..scan.files.len()).partition(|&i| is_store(&scan.files[i]));

    let groups = [
        (pack, lzma2_config(settings.level, settings.threads)),
        (store, EncoderConfiguration::new(EncoderMethod::COPY)),
    ];
    for (indices, method) in groups {
        if indices.is_empty() {
            continue;
        }
        w.set_content_methods(vec![method]);
        let reader_for = |i: usize| HashReader {
            path: scan.files[i].abs.clone(),
            file: None,
            slot: i,
            b3: blake3::Hasher::new(),
            sha: Sha256::new(),
            bytes: 0,
            finished: false,
            out: hashes.clone(),
            progress: progress.clone(),
        };
        let entry_for = |i: usize| {
            let f = &scan.files[i];
            let mut e = ArchiveEntry::from_path(&f.abs, f.rel.clone());
            e.size = f.size;
            e
        };
        let mut entries = Vec::new();
        let mut readers: Vec<SourceReader<HashReader>> = Vec::new();
        let mut block_bytes = 0;
        for i in indices {
            let size = scan.files[i].size;
            if (size >= MAX_BLOCK || block_bytes + size >= MAX_BLOCK)
                && !entries.is_empty() {
                    w.push_archive_entries_send(std::mem::take(&mut entries), std::mem::take(&mut readers))?;
                    block_bytes = 0;
                }
            if size >= MAX_BLOCK {
                w.push_archive_entry_send(entry_for(i), Some(reader_for(i)))?;
                continue;
            }
            block_bytes += size;
            entries.push(entry_for(i));
            readers.push(reader_for(i).into());
        }
        if !entries.is_empty() {
            w.push_archive_entries_send(entries, readers)?;
        }
    }
    let records = file_records(scan, &hashes.lock().unwrap(), &is_store)?;
    let manifest = serde_json::to_vec_pretty(&Manifest {
        archive: record.clone(),
        entries: records.clone(),
        excluded: excluded.clone(),
    })?;
    // Marked as a folder and hidden, so it stays out of the way when the
    // archive is extracted on Windows too.
    const HIDDEN: u32 = 0x2;
    const DIRECTORY: u32 = 0x10;
    let now = sevenz_turbo::NtTime::now();
    let mut dir = ArchiveEntry::new_directory(DATA_DIR);
    dir.has_windows_attributes = true;
    dir.windows_attributes = DIRECTORY | HIDDEN;
    dir.has_last_modified_date = true;
    dir.last_modified_date = now;
    w.push_archive_entry::<File>(dir, None)?;
    let mut entry = ArchiveEntry::new_file(MANIFEST_ENTRY);
    entry.size = manifest.len() as u64;
    entry.has_last_modified_date = true;
    entry.last_modified_date = now;
    w.set_content_methods(vec![lzma2_config(5, 1)]);
    w.push_archive_entry(entry, Some(io::Cursor::new(manifest)))?;
    w.finish().with_context(|| format!("can't finish {}", dest.display()))?;
    Ok(records)
}

/// The catalogue records for everything in `scan`, once every file has
/// been read and hashed.
fn file_records(scan: &Scan, hashes: &[Option<FileHash>], is_store: &dyn Fn(&ScannedFile) -> bool) -> Result<Vec<EntryRecord>> {
    let mut records: Vec<EntryRecord> = scan
        .dirs
        .iter()
        .map(|(abs, rel)| {
            let meta = std::fs::metadata(abs).ok();
            EntryRecord {
                rel_path: rel.clone(),
                is_dir: true,
                size: 0,
                modified_utc: utc(meta.as_ref().and_then(|m| m.modified().ok())),
                created_utc: utc(meta.as_ref().and_then(|m| m.created().ok())),
                blake3: None,
                sha256: None,
                method: "none".into(),
            }
        })
        .collect();
    for (i, f) in scan.files.iter().enumerate() {
        let h = hashes[i].as_ref().ok_or_else(|| anyhow!("{} was not read in full", f.rel))?;
        let now = std::fs::metadata(&f.abs).ok();
        let changed = h.bytes != f.size
            || now.as_ref().map(|m| m.len()) != Some(f.size)
            || now.as_ref().and_then(|m| m.modified().ok()) != f.modified;
        if changed {
            bail!("{} changed while it was being archived; close whatever is writing to it and try again", f.rel);
        }
        records.push(EntryRecord {
            rel_path: f.rel.clone(),
            is_dir: false,
            size: f.size,
            modified_utc: utc(f.modified),
            created_utc: utc(f.created),
            blake3: Some(h.blake3.clone()),
            sha256: Some(h.sha256.clone()),
            method: if is_store(f) { "copy".into() } else { "lzma2".into() },
        });
    }
    records.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(records)
}

/// Decodes every file in `archive` and checks it against `expected`.
/// Nothing is written to disk.
pub fn verify_archive(archive: &Path, expected: &[EntryRecord], threads: u32, progress: &Arc<Progress>) -> Result<u64> {
    let want: HashMap<&str, &EntryRecord> =
        expected.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e)).collect();
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut problems: Vec<String> = Vec::new();
    let mut reader = ArchiveReader::open(archive, Password::empty())
        .with_context(|| format!("can't open {}", archive.display()))?
        .with_threads(threads.max(1));
    let mut buf = vec![0u8; 1 << 20];
    reader
        .for_each_entries(|entry, data| {
            if entry.is_directory() {
                return Ok(true);
            }
            if entry.name() == MANIFEST_ENTRY {
                let mut bytes = Vec::new();
                data.read_to_end(&mut bytes)?;
                match serde_json::from_slice::<Manifest>(&bytes) {
                    Ok(m) if m.entries.len() == expected.len() => {}
                    _ => problems.push("the manifest in the archive doesn't match the record".into()),
                }
                return Ok(true);
            }
            let mut h = blake3::Hasher::new();
            let mut n = 0u64;
            loop {
                progress.check_cancel()?;
                let k = data.read(&mut buf)?;
                if k == 0 {
                    break;
                }
                h.update(&buf[..k]);
                n += k as u64;
                progress.done.fetch_add(k as u64, Ordering::Relaxed);
            }
            let name = entry.name().to_string();
            match want.get(name.as_str()) {
                None => problems.push(format!("{name} is in the archive but not in the record")),
                Some(e) => {
                    if e.size != n || e.blake3.as_deref() != Some(h.finalize().to_hex().as_str()) {
                        problems.push(format!("{name} in the archive doesn't match its recorded hash"));
                    }
                }
            }
            seen.insert(name, ());
            Ok(true)
        })
        .map_err(|e| {
            if progress.cancel.load(Ordering::Relaxed) { anyhow!("cancelled") } else { anyhow!("archive can't be decoded: {e}") }
        })?;
    for (name, e) in &want {
        if !seen.contains_key(*name) && e.size > 0 {
            problems.push(format!("{name} is missing from the archive"));
        }
    }
    if !problems.is_empty() {
        problems.sort();
        bail!("archive check failed:\n{}", summarize(&problems));
    }
    Ok(want.len() as u64)
}

fn summarize(problems: &[String]) -> String {
    let mut s: Vec<String> = problems.iter().take(10).cloned().collect();
    if problems.len() > 10 {
        s.push(format!("and {} more", problems.len() - 10));
    }
    s.join("\n")
}

/// Extracts one file from `archive` into `dest_dir`, keeping only its file
/// name, and returns the path written. Refuses to overwrite an existing file.
pub fn extract_file(archive: &Path, rel_path: &str, dest_dir: &Path, threads: u32) -> Result<PathBuf> {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let dest = dest_dir.join(name);
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let partial = PathBuf::from(format!("{}.partial", dest.display()));
    let mut reader = ArchiveReader::open(archive, Password::empty())
        .with_context(|| format!("can't open {}", archive.display()))?
        .with_threads(threads.max(1));
    let mut found = false;
    let result = reader.for_each_entries(|entry, data| {
        if entry.is_directory() || entry.name() != rel_path {
            return Ok(true);
        }
        let mut out = File::create(&partial)?;
        io::copy(data, &mut out)?;
        found = true;
        Ok(false)
    });
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        bail!("can't extract {rel_path}: {e}");
    }
    if !found {
        bail!("{rel_path} is not in {}", archive.display());
    }
    std::fs::rename(&partial, &dest)?;
    Ok(dest)
}

/// What the check before a delete found.
pub struct DeleteCheck {
    pub files_checked: u64,
    pub bytes_checked: u64,
    /// Files that will be deleted without being kept anywhere, because a
    /// discard rule left them out when the folder was archived.
    pub discarded: Vec<EntryRecord>,
    /// The second archive holding the set-aside files, if there is one.
    pub set_aside_archive: Option<PathBuf>,
}

pub fn is_protected(path: &Path, settings: &Settings) -> bool {
    let key = path_key(path);
    settings.protected.iter().any(|p| {
        let pk = path_key(p);
        key == pk || key.starts_with(&format!("{pk}{}", std::path::MAIN_SEPARATOR))
    })
}

/// Before a folder may be deleted: the folder on disk must hold exactly the
/// files recorded for its archive, every one with the recorded hash, and
/// every file in the archive must decode to the recorded hash too. Files the
/// exclusions left out count only if the manifest lists them with the same
/// hash: set-aside ones must also decode from the second archive, and
/// discarded ones are returned so the person sees what goes for good.
pub fn check_before_delete(
    source: &Path,
    archive: &Path,
    recorded: &[EntryRecord],
    threads: u32,
    progress: &Arc<Progress>,
) -> Result<DeleteCheck> {
    progress.set_phase("Comparing the folder with the catalog", 0);
    let now = scan(source)?;
    // Archives made before exclusions, or before manifests went inside,
    // have nothing excluded.
    let excluded = read_manifest(archive).map(|m| m.excluded).unwrap_or_default();
    let aside_archive = excluded.set_aside_archive.as_ref().map(|n| archive.with_file_name(n));
    if let Some(p) = &aside_archive
        && !p.is_file()
    {
        bail!("{} holds this folder's set-aside files but isn't there", p.display());
    }
    let rec_files: BTreeMap<&str, &EntryRecord> = recorded
        .iter()
        .chain(&excluded.set_aside)
        .chain(&excluded.discarded)
        .filter(|e| !e.is_dir)
        .map(|e| (e.rel_path.as_str(), e))
        .collect();
    let mut problems = Vec::new();
    for f in &now.files {
        match rec_files.get(f.rel.as_str()) {
            None => problems.push(format!("{} isn't in the archive: it is new, or an exclusion left it out", f.rel)),
            Some(r) if r.size != f.size => problems.push(format!("{} has changed size since it was archived", f.rel)),
            _ => {}
        }
    }
    for l in &now.skipped_links {
        problems.push(format!("{l} is a link or junction, which archives don't hold"));
    }
    if !problems.is_empty() {
        bail!("the folder no longer matches its archive:\n{}", summarize(&problems));
    }

    // Hash the folder and decode the archive side by side.
    progress.set_phase("Checking hashes of the folder and the archive", now.total_bytes * 2);
    let (folder_result, archive_result) = std::thread::scope(|s| {
        let folder = s.spawn(|| hash_folder_against(&now, &rec_files, progress));
        let arch = s.spawn(|| {
            verify_archive(archive, recorded, threads, progress)?;
            match &aside_archive {
                Some(p) => verify_archive(p, &excluded.set_aside, threads, progress),
                None => Ok(0),
            }
        });
        (folder.join().unwrap(), arch.join().unwrap())
    });
    folder_result?;
    archive_result?;
    let discarded = excluded.discarded.iter().filter(|d| now.files.iter().any(|f| f.rel == d.rel_path)).cloned().collect();
    Ok(DeleteCheck {
        files_checked: now.files.len() as u64,
        bytes_checked: now.total_bytes,
        discarded,
        set_aside_archive: aside_archive,
    })
}

fn hash_folder_against(now: &Scan, rec: &BTreeMap<&str, &EntryRecord>, progress: &Arc<Progress>) -> Result<()> {
    let mut problems = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    for f in &now.files {
        let mut h = blake3::Hasher::new();
        let mut file = File::open(&f.abs).with_context(|| format!("can't open {}", f.abs.display()))?;
        loop {
            progress.check_cancel()?;
            let k = file.read(&mut buf)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            progress.done.fetch_add(k as u64, Ordering::Relaxed);
        }
        let want = rec.get(f.rel.as_str()).and_then(|r| r.blake3.as_deref());
        if want != Some(h.finalize().to_hex().as_str()) {
            problems.push(format!("{} on disk doesn't match its recorded hash", f.rel));
        }
    }
    if !problems.is_empty() {
        bail!("the folder no longer matches its archive:\n{}", summarize(&problems));
    }
    Ok(())
}

/// Deletes `source` for good. Read-only files are made writable first,
/// because Windows refuses to delete them otherwise.
pub fn delete_folder(source: &Path) -> Result<()> {
    for item in walkdir::WalkDir::new(source).follow_links(false).into_iter().flatten() {
        if let Ok(meta) = item.path().symlink_metadata() {
            let mut perms = meta.permissions();
            if perms.readonly() && !meta.file_type().is_symlink() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(item.path(), perms);
            }
        }
    }
    std::fs::remove_dir_all(source).with_context(|| format!("can't delete {}", source.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn settings(out: &Path, store: bool) -> Settings {
        Settings { out_dir: out.to_path_buf(), level: 5, store_mode: store, threads: 4, protected: vec![], exclusions: vec![] }
    }

    /// A tree with the awkward cases: Unicode names, an empty folder, an
    /// empty file, nested folders, compressible and incompressible files.
    fn make_tree(root: &Path) {
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(root.join("empty folder")).unwrap();
        std::fs::write(root.join("notes.txt"), "hello world\n".repeat(50_000)).unwrap();
        std::fs::write(root.join("sub/zero.bin"), b"").unwrap();
        std::fs::write(root.join("sub/deeper/caf\u{e9} \u{1f600}.txt"), "unicode name").unwrap();
        let mut noise = Vec::with_capacity(3 << 20);
        let mut x: u64 = 0x1234_5678;
        for _ in 0..(3 << 20) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            noise.push(x as u8);
        }
        std::fs::write(root.join("sub/already.zip"), &noise).unwrap();
        std::fs::write(root.join("sub/random.dat"), &noise[..1 << 20]).unwrap();
    }

    fn run(store: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("My Folder");
        let out = tmp.path().join("out");
        make_tree(&src);
        let p = Arc::new(Progress::default());
        let o = archive_folder(&src, &settings(&out, store), &p).unwrap();
        assert_eq!(o.record.file_count, 5);
        assert_eq!(o.record.dir_count, 3);
        (tmp, src, out)
    }

    fn recorded(out: &Path, src: &Path) -> (crate::catalog::ArchivedFolder, Vec<EntryRecord>) {
        let cat = Catalog::open(out).unwrap();
        let a = cat.archived_folder(src).unwrap().expect("archived");
        let e = cat.entries(a.archive_id).unwrap();
        (a, e)
    }

    #[test]
    fn reading_a_missing_catalogue_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("not here");
        assert!(Catalog::open_existing(&out).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn original_folder_keeps_its_spelling() {
        let (_tmp, src, out) = run(true);
        let cat = Catalog::open_existing(&out).unwrap();
        let shown = PathBuf::from(&cat.archives().unwrap()[0].source_path);
        assert_eq!(shown, crate::settings::strip_verbatim(&std::fs::canonicalize(&src).unwrap()));
        assert!(shown.ends_with("My Folder"), "{}", shown.display());
        let hit = &cat.search("notes", None, 5).unwrap()[0];
        assert!(hit.source_path.ends_with("My Folder"), "{}", hit.source_path);
    }

    #[test]
    fn clearing_the_catalogue_keeps_the_archives() {
        let (_tmp, src, out) = run(true);
        let (a, _) = recorded(&out, &src);
        let mut cat = Catalog::open_existing(&out).unwrap();
        assert_eq!(cat.clear().unwrap(), 1);
        assert!(cat.archives().unwrap().is_empty());
        assert!(cat.search("notes", None, 5).unwrap().is_empty());
        assert!(cat.archived_folder(&src).unwrap().is_none());
        assert!(a.archive_path.is_file());
        // Archiving again works and is searchable.
        archive_folder(&src, &settings(&out, true), &Arc::new(Progress::default())).unwrap();
        assert_eq!(Catalog::open_existing(&out).unwrap().search("notes", None, 5).unwrap().len(), 1);
    }

    #[test]
    fn rebuilding_the_catalogue_from_the_archives() {
        let (_tmp, src, out) = run(true);
        let (a, before) = recorded(&out, &src);
        Catalog::open_existing(&out).unwrap().clear().unwrap();
        // Something that isn't an AtticARC archive is reported, not added.
        std::fs::write(out.join("other.7z"), b"not an archive").unwrap();
        let r = rebuild_catalog(&out).unwrap();
        assert_eq!((r.added, r.already_there, r.failed.len()), (1, 0, 1), "{r:?}");
        let (b, after) = recorded(&out, &src);
        assert_eq!(after, before);
        assert_eq!(b.archive_path, a.archive_path);
        assert_eq!(b.archive_bytes, a.archive_bytes);
        let r = rebuild_catalog(&out).unwrap();
        assert_eq!((r.added, r.already_there), (0, 1));
    }

    #[test]
    fn old_catalogue_moves_into_its_folder() {
        let (_tmp, _src, out) = run(true);
        let db = crate::catalog::db_path(&out);
        std::fs::rename(&db, out.join("catalog.db")).unwrap();
        std::fs::remove_dir_all(out.join(".atticarc")).unwrap();
        assert!(crate::catalog::exists(&out));
        assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);
        assert!(db.is_file());
        assert!(!out.join("catalog.db").exists());
    }

    #[test]
    fn excluded_files_are_set_aside_or_discarded() {
        use crate::exclude::{Action, Kind};
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let out = tmp.path().join("out");
        make_tree(&src);
        std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
        let mut s = settings(&out, true);
        let rule = |pattern: &str, action| Rule { pattern: pattern.into(), action, ..Rule::default() };
        s.exclusions = vec![rule("*.lnk", Action::Discard), rule("*.dat", Action::SetAside)];
        let o = archive_folder(&src, &s, &Arc::new(Progress::default())).unwrap();
        assert_eq!(o.record.file_count, 4);
        assert_eq!(o.excluded.discarded.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/Shortcut.LNK"]);
        assert_eq!(o.excluded.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);
        assert_eq!(o.excluded.set_aside_archive.as_deref(), Some("src.excluded.7z"));
        let aside = out.join("src.excluded.7z");
        assert_eq!(std::fs::read(src.join("sub/random.dat")).unwrap().len() as u64, read_manifest(&aside).unwrap().entries[0].size);

        // The folder list finds the main archive, and the catalogue knows what was left out.
        let (a, entries) = recorded(&out, &src);
        assert!(a.archive_path.ends_with("src.7z"));
        assert!(!entries.iter().any(|e| e.rel_path.ends_with(".LNK") || e.rel_path.ends_with(".dat")));
        let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].aside_archive.as_deref(), Some(aside.as_path()));
        assert_eq!((folders[0].set_aside.len(), folders[0].discarded.len()), (1, 1));

        // Every file is accounted for, so the folder may go.
        let c = check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).unwrap();
        assert_eq!(c.discarded.len(), 1);
        assert_eq!(c.set_aside_archive.as_deref(), Some(aside.as_path()));
        // But not if a discarded file has changed since...
        std::fs::write(src.join("sub/Shortcut.LNK"), b"lunk").unwrap();
        assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).is_err());
        std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
        // ...or the set-aside archive is gone.
        std::fs::rename(&aside, out.join("moved.bin")).unwrap();
        let e = check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).err().unwrap();
        assert!(format!("{e:#}").contains("set-aside"), "{e:#}");
        std::fs::rename(out.join("moved.bin"), &aside).unwrap();

        // Rebuilding brings back both archives and what was left out.
        Catalog::open_existing(&out).unwrap().clear().unwrap();
        let r = rebuild_catalog(&out).unwrap();
        assert_eq!(r.added, 2, "{r:?}");
        let cat = Catalog::open_existing(&out).unwrap();
        assert_eq!(cat.excluded_folders().unwrap().len(), 1);
        assert!(cat.archived_folder(&src).unwrap().unwrap().archive_path.ends_with("src.7z"));

        // A rule that doesn't compile stops the job before anything is written.
        s.exclusions = vec![Rule { kind: Kind::Regex, pattern: "*.lnk".into(), ..Rule::default() }];
        assert!(archive_folder(&src, &s, &Arc::new(Progress::default())).is_err());
    }

    #[test]
    fn extracted_manifest_is_not_archived_again() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        make_tree(&src);
        std::fs::create_dir_all(src.join(".atticarc")).unwrap();
        std::fs::write(src.join(".atticarc/manifest.json"), b"{}").unwrap();
        std::fs::create_dir_all(src.join("sub/.atticarc")).unwrap();
        let s = scan(&src).unwrap();
        assert!(!s.files.iter().any(|f| f.rel.starts_with(".atticarc")));
        assert!(s.dirs.iter().any(|d| d.1 == "sub/.atticarc"), "only the top-level one is skipped");
        let o = archive_folder(&src, &settings(&tmp.path().join("out"), true), &Arc::new(Progress::default())).unwrap();
        assert_eq!(o.record.file_count, 5);
    }

    #[test]
    fn archive_path_is_stored_in_plain_form() {
        let (_tmp, src, out) = run(true);
        let (a, _) = recorded(&out, &src);
        assert!(!a.archive_path.to_string_lossy().starts_with(r"\\?\"), "{}", a.archive_path.display());
    }

    #[test]
    fn archive_verify_and_catalog() {
        for store in [false, true] {
            let (_tmp, src, out) = run(store);
            let (a, entries) = recorded(&out, &src);
            assert!(a.archive_path.ends_with("My Folder.7z"));
            // The manifest is inside the archive and only archives are left
            // in the output folder.
            let m = read_manifest(&a.archive_path).unwrap();
            assert_eq!(m.entries, entries);
            assert_eq!(m.archive.file_count, 5);
            let mut names: Vec<_> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            assert_eq!(names, [".atticarc", "My Folder.7z"]);
            assert!(crate::catalog::db_path(&out).is_file());
            assert!(!PathBuf::from(format!("{}.partial", a.archive_path.display())).exists());
            let zip = entries.iter().find(|e| e.rel_path == "sub/already.zip").unwrap();
            assert_eq!(zip.method, if store { "copy" } else { "lzma2" });
            let empty = entries.iter().find(|e| e.rel_path == "empty folder").unwrap();
            assert!(empty.is_dir);
            let txt = entries.iter().find(|e| e.rel_path == "notes.txt").unwrap();
            assert_eq!(txt.blake3.as_deref().unwrap(), blake3::hash(&std::fs::read(src.join("notes.txt")).unwrap()).to_hex().as_str());
            assert_eq!(txt.sha256.as_deref().unwrap(), hex(&Sha256::digest(std::fs::read(src.join("notes.txt")).unwrap())));
            // Extract with sevenz-turbo itself and compare every byte.
            let dest = out.join("x");
            ArchiveReader::open(&a.archive_path, Password::empty()).unwrap().with_threads(4).extract_to(&dest).unwrap();
            for e in entries.iter().filter(|e| !e.is_dir) {
                assert_eq!(std::fs::read(dest.join(&e.rel_path)).unwrap(), std::fs::read(src.join(&e.rel_path)).unwrap(), "{}", e.rel_path);
            }
            assert!(dest.join("empty folder").is_dir());
        }
    }

    #[test]
    fn second_archive_gets_a_new_name() {
        let (_tmp, src, out) = run(true);
        let p = Arc::new(Progress::default());
        archive_folder(&src, &settings(&out, true), &p).unwrap();
        let n = std::fs::read_dir(&out).unwrap().flatten().filter(|d| d.path().extension().is_some_and(|x| x == "7z")).count();
        assert_eq!(n, 2);
    }

    #[test]
    fn check_passes_then_delete() {
        let (_tmp, src, out) = run(true);
        let (a, entries) = recorded(&out, &src);
        let p = Arc::new(Progress::default());
        let c = check_before_delete(&src, &a.archive_path, &entries, 4, &p).unwrap();
        assert_eq!(c.files_checked, 5);
        delete_folder(&src).unwrap();
        assert!(!src.exists());
    }

    #[test]
    fn check_fails_when_a_file_changed() {
        let (_tmp, src, out) = run(true);
        let (a, entries) = recorded(&out, &src);
        // Same size, different content: only the hash can catch it.
        let mut f = std::fs::OpenOptions::new().write(true).open(src.join("notes.txt")).unwrap();
        f.write_all(b"HELLO").unwrap();
        drop(f);
        let p = Arc::new(Progress::default());
        let e = check_before_delete(&src, &a.archive_path, &entries, 4, &p).err().unwrap();
        assert!(format!("{e:#}").contains("notes.txt on disk doesn't match"), "{e:#}");
    }

    #[test]
    fn check_fails_when_a_file_was_added() {
        let (_tmp, src, out) = run(true);
        let (a, entries) = recorded(&out, &src);
        std::fs::write(src.join("sub/new.txt"), "new").unwrap();
        let p = Arc::new(Progress::default());
        let e = check_before_delete(&src, &a.archive_path, &entries, 4, &p).err().unwrap();
        assert!(format!("{e:#}").contains("sub/new.txt isn't in the archive: it is new"), "{e:#}");
    }

    #[test]
    fn check_fails_when_the_archive_is_damaged() {
        let (_tmp, src, out) = run(false);
        let (a, entries) = recorded(&out, &src);
        let mut bytes = std::fs::read(&a.archive_path).unwrap();
        let mid = bytes.len() / 3;
        bytes[mid] ^= 0xff;
        std::fs::write(&a.archive_path, bytes).unwrap();
        let p = Arc::new(Progress::default());
        assert!(check_before_delete(&src, &a.archive_path, &entries, 4, &p).is_err());
    }

    #[test]
    fn search_duplicates_and_extract() {
        let (_tmp, src, out) = run(true);
        // A second folder holding a copy of notes.txt.
        let other = src.parent().unwrap().join("Other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::copy(src.join("notes.txt"), other.join("notes copy.txt")).unwrap();
        archive_folder(&other, &settings(&out, true), &Arc::new(Progress::default())).unwrap();

        let cat = Catalog::open(&out).unwrap();
        assert_eq!(cat.archives().unwrap().len(), 2);
        let hits = cat.search("caf", None, 50).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].rel_path, "sub/deeper/caf\u{e9} \u{1f600}.txt");
        let notes = cat.search("notes", None, 50).unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().all(|f| f.copies == 2));
        assert!(cat.search("  ", None, 50).unwrap().is_empty());

        let groups = cat.duplicate_groups(10).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies.len(), 2);
        assert_eq!(groups[0].reclaimable(), std::fs::metadata(src.join("notes.txt")).unwrap().len());

        let a = cat.archived_folder(&src).unwrap().unwrap();
        let c = cat.contents(a.archive_id).unwrap();
        assert!(c.folders.iter().any(|d| d == "sub/deeper"), "{:?}", c.folders);
        assert!(c.files.iter().any(|f| f.rel_path == "sub/random.dat" && f.copies == 1));
        assert!(c.files.iter().any(|f| f.rel_path == "notes.txt" && f.copies == 2));
        let dest = out.join("restored");
        std::fs::create_dir_all(&dest).unwrap();
        let p = extract_file(&a.archive_path, "sub/random.dat", &dest, 2).unwrap();
        assert_eq!(std::fs::read(p).unwrap(), std::fs::read(src.join("sub/random.dat")).unwrap());
        assert!(extract_file(&a.archive_path, "sub/random.dat", &dest, 2).is_err(), "no overwrite");
        assert!(extract_file(&a.archive_path, "nope.txt", &dest, 2).is_err());
    }

    #[test]
    fn protected_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = settings(tmp.path(), true);
        s.protected = vec![tmp.path().join("keep")];
        std::fs::create_dir_all(tmp.path().join("keep/inner")).unwrap();
        std::fs::create_dir_all(tmp.path().join("keeper")).unwrap();
        assert!(is_protected(&tmp.path().join("keep"), &s));
        assert!(is_protected(&tmp.path().join("keep/inner"), &s));
        assert!(!is_protected(&tmp.path().join("keeper"), &s));
    }

    #[test]
    fn output_inside_protected_folder_is_refused_before_it_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        make_tree(&src);
        let keep = tmp.path().join("keep");
        std::fs::create_dir_all(&keep).unwrap();
        let mut s = settings(&keep.join("out"), true);
        s.protected = vec![keep.clone()];
        let e = archive_folder(&src, &s, &Arc::new(Progress::default())).err().unwrap();
        assert!(format!("{e:#}").contains("protected"), "{e:#}");
        assert!(!keep.join("out").exists());
    }

    #[test]
    fn refuses_output_inside_source() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        make_tree(&src);
        let p = Arc::new(Progress::default());
        assert!(archive_folder(&src, &settings(&src.join("out"), true), &p).is_err());
    }

    #[test]
    fn cancel_leaves_no_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let out = tmp.path().join("out");
        make_tree(&src);
        let p = Arc::new(Progress::default());
        p.cancel.store(true, Ordering::Relaxed);
        assert!(archive_folder(&src, &settings(&out, true), &p).is_err());
        let left: Vec<_> = std::fs::read_dir(&out).unwrap().flatten().map(|d| d.file_name()).collect();
        assert!(left.iter().all(|n| !n.to_string_lossy().contains(".7z")), "{left:?}");
    }
}
