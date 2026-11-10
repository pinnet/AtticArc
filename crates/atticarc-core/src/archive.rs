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

/// Where the archive of set-aside files goes inside the main archive, so
/// the destination holds one archive per folder.
pub const EXCLUDED_ENTRY: &str = ".atticarc/excluded.7z";

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
    /// File name of the archive beside this one holding the set-aside
    /// files. Only archives made before the set-aside files went inside
    /// have one.
    pub set_aside_archive: Option<String>,
    /// The archive of set-aside files kept inside this one, stored without
    /// compressing it again at [`EXCLUDED_ENTRY`], with its size and hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_aside_inside: Option<EntryRecord>,
    pub set_aside: Vec<EntryRecord>,
    /// Files archived nowhere, deleted with the folder.
    pub discarded: Vec<EntryRecord>,
    /// Files removed from this archive because another archive holds an
    /// identical copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duplicates: Vec<EntryRecord>,
    /// Duplicates replaced by a reference file pointing to their original.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<Replaced>,
}

/// The extension of the small reference file that stands in for a
/// duplicate: `IMG_1.jpg` becomes `IMG_1.jpg.atticarcdep`. Not a Windows
/// shortcut, so it isn't mistaken for one.
pub const DEP_EXT: &str = "atticarcdep";

/// Where the reference file for the file at `rel` goes.
pub fn dep_path(rel: &str) -> String {
    format!("{rel}.{DEP_EXT}")
}

/// A duplicate replaced by a reference file: the file as it was archived
/// here, and where the original is kept.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Replaced {
    /// The file as it was archived here.
    pub entry: EntryRecord,
    /// Path inside this archive of the reference file standing in for it.
    pub dep_path: String,
    /// File name of the archive that holds the original, in the same folder.
    pub original_archive: String,
    /// Path of the original inside that archive.
    pub original_path: String,
    pub replaced_utc: String,
}

/// What a `.atticarcdep` file holds: plain JSON, so it can be read in any
/// text editor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DepFile {
    /// Format version.
    pub atticarc_dep: u32,
    pub note: String,
    #[serde(flatten)]
    pub replaced: Replaced,
}

impl DepFile {
    pub fn new(replaced: Replaced) -> Self {
        let note = format!(
            "AtticARC replaced this file with a reference because an identical copy (same size and BLAKE3 hash) \
             is kept in {} at {}. AtticARC can extract the original or put the file back.",
            replaced.original_archive, replaced.original_path
        );
        DepFile { atticarc_dep: 1, note, replaced }
    }
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
/// exclusions set aside go into an archive of their own, verified the same
/// way and then stored inside the main archive at [`EXCLUDED_ENTRY`];
/// discarded files are hashed and listed in the main archive's manifest.
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

    // The set-aside files, as a scan of their own for the inner archive,
    // written beside the main archive's partial file until it goes inside.
    let aside_scan = Scan {
        total_bytes: scan.set_aside.iter().map(|f| f.size).sum(),
        files: scan.set_aside.clone(),
        ..Scan::default()
    };
    let aside_partial = (!aside_scan.files.is_empty()).then(|| PathBuf::from(format!("{}.excluded.partial", final_path.display())));
    let aside_record = record_for(format!("{name} (excluded)"), format!("{}|excluded", path_key(&source)), &aside_scan);

    let result = (|| -> Result<(Written, Excluded)> {
        // Must fit before anything is written, as if nothing compresses. The
        // set-aside files count twice: their own archive is written first,
        // then copied inside the main one before it is removed.
        let aside_worst = crate::disk::archive_worst_case(aside_scan.total_bytes);
        let needed = crate::disk::archive_worst_case(scan.total_bytes).saturating_add(aside_worst.saturating_mul(2));
        crate::disk::ensure_free(&out_dir, needed, &format!("archiving {name}"))?;
        let mut excluded = Excluded::default();
        if !scan.discarded.is_empty() {
            progress.set_phase("Hashing discarded files", scan.discarded.iter().map(|f| f.size).sum());
            excluded.discarded = scan.discarded.iter().map(|f| hash_record(f, progress)).collect::<Result<_>>()?;
        }
        if let Some(aside) = &aside_partial {
            progress.set_phase("Compressing set-aside files", aside_scan.total_bytes);
            let entries = write_archive(&aside_scan, aside, settings, progress, &aside_record, &Excluded::default(), None)?;
            progress.set_phase("Verifying set-aside files", aside_scan.total_bytes);
            verify_archive(aside, &entries, settings.threads, progress)?;
            excluded.set_aside = entries;
            excluded.set_aside_inside = Some(inner_record(aside)?);
        }
        progress.set_phase("Compressing", scan.total_bytes);
        let entries = write_archive(&scan, &partial, settings, progress, &record, &excluded, aside_partial.as_deref())?;
        progress.set_phase("Verifying", scan.total_bytes);
        verify_archive(&partial, &with_inner(&entries, &excluded), settings.threads, progress)?;
        let archive_bytes = std::fs::metadata(&partial)?.len();
        Ok(((entries, archive_bytes), excluded))
    })();
    if let Some(p) = &aside_partial {
        let _ = std::fs::remove_file(p);
    }

    let ((entries, archive_bytes), excluded) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            if let Ok(cat) = Catalog::open(&out_dir) {
                let _ = cat.log_run(None, "archive", &source, &started, "failed", &format!("{e:#}"));
            }
            return Err(e);
        }
    };

    let mut cat = Catalog::open(&out_dir)?;
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
    if !excluded.set_aside.is_empty() {
        detail.push(format!("set aside inside the archive: {}", names(&excluded.set_aside)));
    }
    if !excluded.discarded.is_empty() {
        detail.push(format!("discarded: {}", names(&excluded.discarded)));
    }
    let detail = detail.join("; ");
    cat.log_run(Some(archive_id), "archive", &source, &started, "verified", &detail)?;
    Ok(ArchiveOutcome { record, skipped_links: scan.skipped_links, excluded })
}

/// The record of an archive of set-aside files about to go inside the main
/// archive at [`EXCLUDED_ENTRY`]: its size and hashes, so the main archive's
/// check covers it byte for byte.
fn inner_record(path: &Path) -> Result<EntryRecord> {
    let meta = std::fs::metadata(path).with_context(|| format!("can't read {}", path.display()))?;
    let f = ScannedFile { abs: path.to_path_buf(), rel: EXCLUDED_ENTRY.into(), size: meta.len(), modified: meta.modified().ok(), created: None };
    let mut rec = hash_record(&f, &Arc::new(Progress::default()))?;
    rec.method = "copy".into();
    Ok(rec)
}

/// `entries` with the inner archive of set-aside files, if `excluded` has
/// one: everything a check of the main archive must find.
fn with_inner(entries: &[EntryRecord], excluded: &Excluded) -> Vec<EntryRecord> {
    entries.iter().cloned().chain(excluded.set_aside_inside.clone()).collect()
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
    /// Archives found under a new name and relinked: (old name, new name).
    pub relinked: Vec<(String, String)>,
    /// Files that are copies of an archive already in the catalogue under
    /// another name, so were not added twice.
    pub copies: Vec<String>,
}

/// Adds every .7z in `out_dir` that isn't in its catalogue yet, from the
/// manifests the archives carry. Archives already catalogued are left alone.
pub fn rebuild_catalog(out_dir: &Path) -> Result<Rebuilt> {
    rebuild_catalog_with(out_dir, &Progress::default())
}

/// [`rebuild_catalog`], reporting on `progress`: one step per archive file,
/// with the phase naming the file being read.
pub fn rebuild_catalog_with(out_dir: &Path, progress: &Progress) -> Result<Rebuilt> {
    let mut out = Rebuilt::default();
    if crate::catalog::exists(out_dir) {
        progress.set_phase("Looking for renamed archives", 0);
        // Archives renamed since they were catalogued are relinked, not added twice.
        out.relinked = relink_missing(out_dir)?.relinked;
    }
    let found = archive_files(out_dir)?;
    if found.is_empty() {
        return Ok(out);
    }
    progress.set_phase("Reading the manifests", found.len() as u64);
    let mut cat = Catalog::open(out_dir)?;
    // Fingerprints of the archives already catalogued, made when first needed.
    let mut known: Option<HashMap<String, String>> = None;
    let total = found.len();
    for (i, path) in found.into_iter().enumerate() {
        let file = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        *progress.phase.lock().unwrap() = format!("Reading {file} ({} of {total})", i + 1);
        progress.done.store(i as u64 + 1, Ordering::Relaxed);
        let key = crate::settings::strip_verbatim(&path).to_string_lossy().into_owned();
        if cat.has_archive_path(&key)? {
            out.already_there += 1;
            continue;
        }
        let added = read_manifest(&path).and_then(|m| {
            if known.is_none() {
                known = Some(catalogue_fingerprints(&cat)?);
            }
            let fp = crate::catalog::fingerprint(&m.archive.created_utc, &m.entries);
            if let Some(of) = known.as_ref().and_then(|k| k.get(&fp)) {
                return Ok(Some(of.clone()));
            }
            let id = cat.insert_archive(&m.archive, &m.entries)?;
            cat.insert_excluded(id, &m.excluded)?;
            Ok(None)
        });
        match added {
            Ok(None) => out.added += 1,
            Ok(Some(of)) => out.copies.push(format!("{file} is a copy of {of}")),
            Err(e) => out.failed.push(format!("{file}: {e:#}")),
        }
    }
    Ok(out)
}

/// The `.7z` files directly in `dir`, sorted.
fn archive_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("can't read {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("7z")))
        .collect();
    found.sort();
    Ok(found)
}

/// Fingerprint → archive file name, for every archive in the catalogue.
fn catalogue_fingerprints(cat: &Catalog) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for a in cat.archives()? {
        let fp = crate::catalog::fingerprint(&a.created_utc, &cat.entries(a.id)?);
        let name = Path::new(&a.archive_path).file_name().unwrap_or_default().to_string_lossy().into_owned();
        map.insert(fp, name);
    }
    Ok(map)
}

/// What [`relink_missing`] did.
#[derive(Debug, Default)]
pub struct Relinked {
    /// Archives found under a new name: (old file name, new file name).
    pub relinked: Vec<(String, String)>,
    /// Archives whose file is missing and wasn't found under another name.
    pub missing: Vec<i64>,
}

/// Finds catalogued archives whose file is gone from `out_dir`, and looks
/// for them among the `.7z` files there that the catalogue doesn't know,
/// by [`crate::catalog::fingerprint`]: so renaming an archive in Explorer doesn't
/// lose it. A match is relinked, keeping its old name as an alias so
/// reference files naming it still resolve. Only the small manifests of
/// unknown files are read, and only when something is missing.
pub fn relink_missing(out_dir: &Path) -> Result<Relinked> {
    let mut out = Relinked::default();
    let mut cat = Catalog::open_existing(out_dir)?;
    let missing: Vec<_> = cat.archives()?.into_iter().filter(|a| !Path::new(&a.archive_path).is_file()).collect();
    if missing.is_empty() {
        return Ok(out);
    }
    let mut unknown: HashMap<String, PathBuf> = HashMap::new();
    // If the folder can't be listed, nothing is relinked and the archives stay missing.
    for path in archive_files(out_dir).unwrap_or_default() {
        let key = crate::settings::strip_verbatim(&path).to_string_lossy().into_owned();
        if cat.has_archive_path(&key)? {
            continue;
        }
        if let Ok(m) = read_manifest(&path) {
            unknown.insert(crate::catalog::fingerprint(&m.archive.created_utc, &m.entries), path);
        }
    }
    for a in missing {
        let fp = crate::catalog::fingerprint(&a.created_utc, &cat.entries(a.id)?);
        match unknown.remove(&fp) {
            Some(path) => {
                let new = crate::settings::strip_verbatim(&path).to_string_lossy().into_owned();
                cat.relink(a.id, &new)?;
                let name = |p: &str| Path::new(p).file_name().unwrap_or_default().to_string_lossy().into_owned();
                out.relinked.push((name(&a.archive_path), name(&new)));
            }
            None => out.missing.push(a.id),
        }
    }
    Ok(out)
}

/// The archive file named `name` in `out_dir`, the way a reference file
/// names its original's archive. If it was renamed and relinked since,
/// its current path from the catalogue.
fn original_archive_file(out_dir: &Path, name: &str) -> PathBuf {
    let plain = out_dir.join(name);
    if plain.is_file() {
        return plain;
    }
    Catalog::open_existing(out_dir)
        .and_then(|c| c.path_for_alias(name))
        .ok()
        .flatten()
        .map(PathBuf::from)
        .unwrap_or(plain)
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
/// rest. `inner` is the archive of set-aside files to store, uncompressed,
/// at [`EXCLUDED_ENTRY`]; `excluded.set_aside_inside` must describe it.
fn write_archive(
    scan: &Scan,
    dest: &Path,
    settings: &Settings,
    progress: &Arc<Progress>,
    record: &ArchiveRecord,
    excluded: &Excluded,
    inner: Option<&Path>,
) -> Result<Vec<EntryRecord>> {
    if inner.is_some() != excluded.set_aside_inside.is_some() {
        bail!("the set-aside files' archive and its record don't agree");
    }
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
    let mut records = file_records(scan, &hashes.lock().unwrap(), &is_store)?;
    for r in &mut records {
        if excluded.deps.iter().any(|d| d.dep_path == r.rel_path) {
            r.method = "dep".into();
        }
    }
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
    if let (Some(path), Some(rec)) = (inner, &excluded.set_aside_inside) {
        let f = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
        if f.metadata()?.len() != rec.size {
            bail!("the set-aside files' archive changed size before it went inside");
        }
        let mut entry = ArchiveEntry::new_file(EXCLUDED_ENTRY);
        entry.size = rec.size;
        entry.has_last_modified_date = true;
        entry.last_modified_date = now;
        w.set_content_methods(vec![EncoderConfiguration::new(EncoderMethod::COPY)]);
        w.push_archive_entry(entry, Some(f))?;
    }
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
    verify_archive_saving(archive, expected, threads, progress, None)
}

/// [`verify_archive`], also writing the inner archive of set-aside files to
/// `save_inner` as it is decoded, so it can be checked next without
/// decoding the main archive again.
fn verify_archive_saving(
    archive: &Path,
    expected: &[EntryRecord],
    threads: u32,
    progress: &Arc<Progress>,
    save_inner: Option<&Path>,
) -> Result<u64> {
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
                    // The inner archive of set-aside files is checked like a file but isn't one of the entries.
                    Ok(m) if m.entries.len() + usize::from(m.excluded.set_aside_inside.is_some()) == expected.len() => {}
                    _ => problems.push("the manifest in the archive doesn't match the record".into()),
                }
                return Ok(true);
            }
            let mut h = blake3::Hasher::new();
            let mut n = 0u64;
            let mut save = match save_inner.filter(|_| entry.name() == EXCLUDED_ENTRY) {
                Some(p) => Some(File::create(p)?),
                None => None,
            };
            loop {
                progress.check_cancel()?;
                let k = data.read(&mut buf)?;
                if k == 0 {
                    break;
                }
                h.update(&buf[..k]);
                if let Some(f) = &mut save {
                    io::Write::write_all(f, &buf[..k])?;
                }
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
    extract_file_as(archive, rel_path, &dest_dir.join(name), threads)
}

/// Extracts the file `rel_path` in `archive` to `dest`, refusing to overwrite.
fn extract_file_as(archive: &Path, rel_path: &str, dest: &Path, threads: u32) -> Result<PathBuf> {
    let dest = dest.to_path_buf();
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

/// Extracts several files from `archive` in one pass, each (path inside the
/// archive, where to write it). Every file must be found.
fn extract_many(archive: &Path, files: &[(String, PathBuf)], threads: u32) -> Result<()> {
    let mut want: HashMap<&str, Vec<&Path>> = HashMap::new();
    for (rel, to) in files {
        want.entry(rel.as_str()).or_default().push(to.as_path());
    }
    let mut found = 0;
    let mut reader = ArchiveReader::open(archive, Password::empty())
        .with_context(|| format!("can't open {}", archive.display()))?
        .with_threads(threads.max(1));
    reader
        .for_each_entries(|entry, data| {
            if entry.is_directory() {
                return Ok(true);
            }
            if let Some(dests) = want.get(entry.name()) {
                let mut bytes = Vec::new();
                data.read_to_end(&mut bytes)?;
                for to in dests {
                    if let Some(parent) = to.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(to, &bytes)?;
                }
                found += 1;
            }
            Ok(found < want.len())
        })
        .map_err(|e| anyhow!("can't unpack from {}: {e}", archive.display()))?;
    if found < want.len() {
        let missing: Vec<&str> = want.keys().copied().collect();
        bail!("{} doesn't hold all of: {}", archive.display(), missing.join(", "));
    }
    Ok(())
}

fn parse_utc(t: Option<&str>) -> Option<SystemTime> {
    t.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(SystemTime::from)
}

/// What putting excluded files back into an archive did.
#[derive(Debug, Default)]
pub struct Restored {
    pub put_back: usize,
    /// Set-aside files moved to the discarded list.
    pub discarded: usize,
    /// Whether the set-aside archive was emptied and removed.
    pub aside_removed: bool,
    /// Whether the set-aside archive beside the main one, from an older
    /// version, was moved inside it.
    pub moved_inside: bool,
}

/// Puts files the exclusions left out of `archive` back into it. Set-aside
/// files come from the set-aside archive inside it (or, for older archives,
/// beside it); discarded ones from the source folder, which must still hold
/// them with their recorded hash. With `discard_rest`, the set-aside files
/// not kept move to the discarded list and the set-aside archive goes.
/// An older archive's set-aside archive beside it moves inside.
///
/// The archive is unpacked into a work folder in the catalogue's folder and
/// written again, then verified, and only then swapped in, so this needs
/// free space for the unpacked archive and its new copy.
pub fn restore_excluded(
    archive: &Path,
    keep: &[String],
    discard_rest: bool,
    settings: &Settings,
    progress: &Arc<Progress>,
) -> Result<Restored> {
    rewrite(archive, &Changes { keep, discard_rest, ..Changes::default() }, settings, progress)
}

/// Moves the set-aside archive beside an older `archive` inside it, so the
/// destination holds one archive per folder. Its files are checked by
/// decoding them first; the archive beside is deleted only once the new
/// main archive is verified.
pub fn move_aside_inside(archive: &Path, settings: &Settings, progress: &Arc<Progress>) -> Result<Restored> {
    let m = read_manifest(archive)?;
    match &m.excluded.set_aside_archive {
        None => bail!("{} has no set-aside archive beside it", archive.display()),
        Some(n) if !archive.with_file_name(n).is_file() => bail!("{n} isn't beside {}", archive.display()),
        Some(_) => rewrite(archive, &Changes::default(), settings, progress),
    }
}

/// What removing duplicates did.
#[derive(Debug, Default)]
pub struct RemovedDuplicates {
    pub files: usize,
    pub bytes: u64,
    pub archives: usize,
}

/// Removes the catalogued files `entry_ids` from their archives, as long as
/// every one has an identical copy (same size and hash) that stays in
/// another archive or another place in the same one. Each archive is
/// rewritten without them, verified and swapped in, and its manifest lists
/// them as duplicates, so its folder still passes the check before a delete.
pub fn remove_duplicates(out_dir: &Path, entry_ids: &[i64], settings: &Settings, progress: &Arc<Progress>) -> Result<RemovedDuplicates> {
    let cat = Catalog::open_existing(out_dir)?;
    let mut by_archive: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut out = RemovedDuplicates::default();
    for id in entry_ids {
        let f = cat.entry_location(*id)?.ok_or_else(|| anyhow!("a file picked is no longer in the catalogue; refresh and try again"))?;
        let hash = f.blake3.as_deref().ok_or_else(|| anyhow!("{} has no recorded hash", f.rel_path))?;
        if cat.is_dep_original(&f.archive_path, &f.rel_path)? {
            bail!("{} is the original that reference files point to, so it can't be removed", f.rel_path);
        }
        let kept = cat
            .copies_of(hash, f.size)?
            .into_iter()
            .any(|(other, path)| !entry_ids.contains(&other) && path.is_file());
        if !kept {
            bail!("{} would have no copy left in any archive; untick one of its copies", f.rel_path);
        }
        out.files += 1;
        out.bytes += f.size;
        by_archive.entry(f.archive_path).or_default().push(f.rel_path);
    }
    drop(cat);
    for (archive, rels) in &by_archive {
        rewrite(archive, &Changes { remove: rels, ..Changes::default() }, settings, progress)
            .with_context(|| format!("{} was left as it was", archive.file_name().unwrap_or_default().to_string_lossy()))?;
        out.archives += 1;
    }
    Ok(out)
}

/// What replacing duplicates by reference files did, or in a dry run, would do.
#[derive(Debug, Default)]
pub struct ReplaceReport {
    pub dry_run: bool,
    pub files: usize,
    pub bytes: u64,
    /// Each archive changed (or to change), oldest first, with its file
    /// count and bytes.
    pub archives: Vec<(String, usize, u64)>,
    /// Originals decoded from their archives and found to match their hash.
    pub originals_checked: usize,
}

/// Replaces each catalogued duplicate `entry_ids` inside its archive with a
/// small `.atticarcdep` reference file pointing to the original: the oldest
/// copy, in the oldest archive, which stays. Before anything changes, every
/// original is decoded from its archive and must match its recorded hash;
/// if any doesn't, nothing is changed. With `dry_run`, only those checks
/// run. Each archive is then unpacked, the duplicate checked against its
/// hash and swapped for its reference file, and the archive written again,
/// verified and swapped in, oldest archive first.
pub fn replace_duplicates(
    out_dir: &Path,
    entry_ids: &[i64],
    dry_run: bool,
    settings: &Settings,
    progress: &Arc<Progress>,
) -> Result<ReplaceReport> {
    let cat = Catalog::open_existing(out_dir)?;
    let mut report = ReplaceReport { dry_run, ..ReplaceReport::default() };
    // By archive, in catalogue order (oldest first).
    let mut by_archive: Vec<(PathBuf, String, Vec<Replaced>)> = Vec::new();
    let mut originals: BTreeMap<PathBuf, BTreeMap<String, (String, u64)>> = BTreeMap::new();
    let now = now_utc();
    let mut leaves = Vec::new();
    for id in entry_ids {
        leaves.push(cat.dedupe_leaf(*id)?.ok_or_else(|| anyhow!("a file picked is no longer a duplicate in the catalogue; refresh and try again"))?);
    }
    leaves.sort_by(|a, b| (&a.archive_created, a.archive_id, &a.entry.rel_path).cmp(&(&b.archive_created, b.archive_id, &b.entry.rel_path)));
    for leaf in leaves {
        if !leaf.original_archive_path.is_file() {
            bail!("{} holds the original of {} but isn't there; nothing was changed", leaf.original_archive_path.display(), leaf.entry.rel_path);
        }
        if !leaf.archive_path.is_file() {
            bail!("{} isn't there; nothing was changed", leaf.archive_path.display());
        }
        if leaf.original_archive_path.parent() != leaf.archive_path.parent() {
            bail!("{} and its original's archive aren't in the same folder", leaf.entry.rel_path);
        }
        let hash = leaf.entry.blake3.clone().ok_or_else(|| anyhow!("{} has no recorded hash", leaf.entry.rel_path))?;
        originals.entry(leaf.original_archive_path.clone()).or_default().insert(leaf.original_path.clone(), (hash, leaf.entry.size));
        let r = Replaced {
            dep_path: dep_path(&leaf.entry.rel_path),
            original_archive: leaf.original_archive_path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            original_path: leaf.original_path,
            replaced_utc: now.clone(),
            entry: leaf.entry,
        };
        report.files += 1;
        report.bytes += r.entry.size;
        match by_archive.iter_mut().find(|(p, ..)| *p == leaf.archive_path) {
            Some((.., v)) => v.push(r),
            None => by_archive.push((leaf.archive_path, leaf.archive_name, vec![r])),
        }
    }
    drop(cat);
    report.archives = by_archive.iter().map(|(_, n, v)| (n.clone(), v.len(), v.iter().map(|r| r.entry.size).sum())).collect();

    // Every original must decode with its hash before anything changes.
    let mut problems = Vec::new();
    for (archive, want) in &originals {
        let name = archive.file_name().unwrap_or_default().to_string_lossy().into_owned();
        progress.set_phase(&format!("Checking the originals in {name}"), want.values().map(|v| v.1).sum());
        let got = hash_entries(archive, &want.keys().cloned().collect::<Vec<_>>(), settings.threads, progress)?;
        for (rel, (hash, size)) in want {
            match got.get(rel) {
                Some((h, n)) if h == hash && n == size => report.originals_checked += 1,
                Some(_) => problems.push(format!("{rel} in {name} doesn't match its recorded hash")),
                None => problems.push(format!("{rel} is missing from {name}")),
            }
        }
    }
    if !problems.is_empty() {
        problems.sort();
        bail!("nothing was changed, because some originals failed their check:\n{}", summarize(&problems));
    }
    if dry_run {
        return Ok(report);
    }
    let total = by_archive.len();
    for (i, (archive, name, reps)) in by_archive.iter().enumerate() {
        rewrite(archive, &Changes { replace: reps, ..Changes::default() }, settings, progress).with_context(|| {
            let done = if i == 0 { "no archive was changed".to_string() } else { format!("the {i} archives before it were done") };
            format!("{name}.7z was left as it was (archive {} of {total}); {done}", i + 1)
        })?;
    }
    Ok(report)
}

/// Puts the files in `archive` that were replaced by reference files back,
/// from their originals, by their paths. The originals must match their
/// recorded hash; the archive is rewritten, verified and swapped in.
pub fn put_back_replaced(archive: &Path, rels: &[String], settings: &Settings, progress: &Arc<Progress>) -> Result<usize> {
    rewrite(archive, &Changes { put_back: rels, ..Changes::default() }, settings, progress)?;
    Ok(rels.len())
}

/// Follows a reference file: extracts the original it points to into
/// `dest_dir`, under the replaced file's name, and checks its hash.
pub fn extract_original(out_dir: &Path, dep: &Replaced, dest_dir: &Path, threads: u32) -> Result<PathBuf> {
    let from = original_archive_file(out_dir, &dep.original_archive);
    if !from.is_file() {
        bail!("{} holds the original but isn't there", from.display());
    }
    let name = dep.entry.rel_path.rsplit('/').next().unwrap_or(&dep.entry.rel_path);
    let to = extract_file_as(&from, &dep.original_path, &dest_dir.join(name), threads)?;
    if let Err(e) = check_hash(&to, &dep.entry) {
        let _ = std::fs::remove_file(&to);
        return Err(e.context("the original doesn't match the replaced file's hash, so it was not kept"));
    }
    set_times(&to, parse_utc(dep.entry.modified_utc.as_deref()), parse_utc(dep.entry.created_utc.as_deref()), false);
    Ok(to)
}

/// Decodes `archive` and returns the BLAKE3 hash and size of each of the
/// files `rels` found in it. Nothing is written to disk.
fn hash_entries(archive: &Path, rels: &[String], threads: u32, progress: &Arc<Progress>) -> Result<HashMap<String, (String, u64)>> {
    let want: std::collections::HashSet<&str> = rels.iter().map(String::as_str).collect();
    let mut out = HashMap::new();
    let mut reader = ArchiveReader::open(archive, Password::empty())
        .with_context(|| format!("can't open {}", archive.display()))?
        .with_threads(threads.max(1));
    let mut buf = vec![0u8; 1 << 20];
    reader
        .for_each_entries(|entry, data| {
            if entry.is_directory() || !want.contains(entry.name()) {
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
            out.insert(entry.name().to_string(), (h.finalize().to_hex().to_string(), n));
            Ok(out.len() < want.len())
        })
        .map_err(|e| {
            if progress.cancel.load(Ordering::Relaxed) { anyhow!("cancelled") } else { anyhow!("{} can't be decoded: {e}", archive.display()) }
        })?;
    Ok(out)
}

/// What to change when an archive is rewritten.
#[derive(Default)]
struct Changes<'a> {
    /// Left-out files to put back.
    keep: &'a [String],
    /// Move the set-aside files not kept to the discarded list.
    discard_rest: bool,
    /// Files to remove as duplicates.
    remove: &'a [String],
    /// Duplicates to replace by a reference file.
    replace: &'a [Replaced],
    /// Files replaced by a reference file to put back, by their path.
    put_back: &'a [String],
}

/// Rewrites `archive` with `ch`: puts back left-out files, optionally
/// discards the rest of the set-aside files, removes duplicates, replaces
/// duplicates by reference files, and puts replaced files back.
fn rewrite(archive: &Path, ch: &Changes<'_>, settings: &Settings, progress: &Arc<Progress>) -> Result<Restored> {
    let (keep, discard_rest, remove) = (ch.keep, ch.discard_rest, ch.remove);
    let m = read_manifest(archive)?;
    let changes_files = !remove.is_empty() || !ch.replace.is_empty();
    if changes_files && m.archive.source_path.ends_with("|excluded") {
        bail!("{} holds set-aside files; manage those on the Excluded page", archive.display());
    }
    let recorded = |rel: &str| m.entries.iter().find(|e| !e.is_dir && e.rel_path == rel);
    for rel in remove {
        if recorded(rel).is_none() {
            bail!("{rel} isn't in {}", archive.display());
        }
    }
    // Each replaced file as the manifest records it, which must match the copy the caller expects.
    let mut replace = Vec::new();
    for r in ch.replace {
        let rel = &r.entry.rel_path;
        let e = recorded(rel).ok_or_else(|| anyhow!("{rel} isn't in {}", archive.display()))?;
        if e.method == "dep" {
            bail!("{rel} is already a reference file");
        }
        if e.size != r.entry.size || e.blake3 != r.entry.blake3 {
            bail!("{rel} in {} isn't the copy the catalogue lists; rebuild the catalogue and try again", archive.display());
        }
        if m.entries.iter().any(|x| x.rel_path == r.dep_path) {
            bail!("{} already has a file called {}", archive.display(), r.dep_path);
        }
        replace.push(Replaced { entry: e.clone(), ..r.clone() });
    }
    for rel in ch.put_back {
        if !m.excluded.deps.iter().any(|d| &d.entry.rel_path == rel) {
            bail!("{rel} wasn't replaced by a reference file in {}", archive.display());
        }
    }
    let ex = &m.excluded;
    let find = |list: &[EntryRecord], rel: &str| list.iter().any(|e| e.rel_path == rel);
    for rel in keep {
        if !find(&ex.set_aside, rel) && !find(&ex.discarded, rel) {
            bail!("{rel} isn't one of the files left out of {}", archive.display());
        }
    }
    let out_dir = archive.parent().ok_or_else(|| anyhow!("{} has no folder", archive.display()))?;
    // Older archives keep their set-aside files in an archive beside them.
    // Whenever one is rewritten, those files move inside it, unless that
    // archive has gone missing and isn't needed.
    let aside_path = ex.set_aside_archive.as_ref().map(|n| archive.with_file_name(n));
    let migrate = aside_path.as_ref().is_some_and(|p| p.is_file());
    let source = PathBuf::from(if m.archive.source_display.is_empty() { &m.archive.source_path } else { &m.archive.source_display });
    let work = out_dir.join(DATA_DIR).join(format!("work-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let main_dir = work.join("main");
    let aside_dir = work.join("aside");

    let result = (|| -> Result<_> {
        // Room for the unpacked files (main and set-aside), a new set-aside
        // archive and the new main archive beside the old one, as if nothing
        // compresses.
        let aside: u64 = ex.set_aside.iter().map(|e| e.size).sum();
        let unpacked = m.archive.input_bytes.saturating_add(aside);
        let needed = unpacked
            .saturating_add(crate::disk::archive_worst_case(unpacked))
            .saturating_add(crate::disk::archive_worst_case(aside));
        crate::disk::ensure_free(out_dir, needed, &format!("rewriting {}", m.archive.name))?;
        progress.set_phase("Unpacking the archive", m.archive.input_bytes);
        // This unpacks the inner archive of set-aside files too, still packed.
        extract_all(archive, &main_dir, settings.threads, progress)?;
        let old_inner = main_dir.join(EXCLUDED_ENTRY);
        if let Some(rec) = &ex.set_aside_inside {
            check_hash(&old_inner, rec).context("the set-aside files' archive inside didn't unpack with its recorded hash")?;
        }
        let old_aside = if ex.set_aside_inside.is_some() { Some(old_inner.clone()) } else { aside_path.clone() };
        // Only needed when a set-aside file is kept; otherwise the set-aside
        // archive stays as it is or goes whole.
        let takes_aside = keep.iter().any(|r| find(&ex.set_aside, r));
        if let Some(p) = old_aside.as_ref().filter(|_| takes_aside) {
            progress.set_phase("Unpacking the set-aside files", ex.set_aside.iter().map(|e| e.size).sum());
            extract_all(p, &aside_dir, settings.threads, progress)?;
        }
        for rel in keep {
            let to = main_dir.join(rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if let Some(rec) = ex.set_aside.iter().find(|e| &e.rel_path == rel) {
                std::fs::rename(aside_dir.join(rel), &to).with_context(|| format!("{rel} isn't in the set-aside archive"))?;
                check_hash(&to, rec)?;
            } else {
                let rec = ex.discarded.iter().find(|e| &e.rel_path == rel).unwrap();
                let from = source.join(rel);
                if !from.is_file() {
                    bail!("{rel} was discarded and the folder no longer has it, so it can't be put back");
                }
                std::fs::copy(&from, &to).with_context(|| format!("can't copy {}", from.display()))?;
                check_hash(&to, rec).with_context(|| format!("{rel} has changed since it was archived"))?;
                set_times(&to, std::fs::metadata(&from)?.modified().ok(), std::fs::metadata(&from)?.created().ok(), false);
            }
        }
        // Duplicates out, reference files in: the copy unpacked must match its hash first.
        for r in &replace {
            let at = main_dir.join(&r.entry.rel_path);
            check_hash(&at, &r.entry).with_context(|| format!("{} didn't unpack with its recorded hash", r.entry.rel_path))?;
            std::fs::remove_file(&at).with_context(|| format!("can't remove {} from the work folder", r.entry.rel_path))?;
            let dep = main_dir.join(&r.dep_path);
            std::fs::write(&dep, serde_json::to_vec_pretty(&DepFile::new(r.clone()))?)
                .with_context(|| format!("can't write {}", r.dep_path))?;
            set_times(&dep, parse_utc(r.entry.modified_utc.as_deref()), parse_utc(r.entry.created_utc.as_deref()), false);
        }
        // Replaced files back from their originals, one pass per original archive.
        let mut wanted: BTreeMap<PathBuf, Vec<(String, PathBuf)>> = BTreeMap::new();
        for rel in ch.put_back {
            let d = ex.deps.iter().find(|d| &d.entry.rel_path == rel).unwrap();
            let from = original_archive_file(out_dir, &d.original_archive);
            if !from.is_file() {
                bail!("{} holds the original of {rel} but isn't there", from.display());
            }
            wanted.entry(from).or_default().push((d.original_path.clone(), main_dir.join(rel)));
        }
        for (from, files) in &wanted {
            progress.set_phase("Unpacking the originals", 0);
            extract_many(from, files, settings.threads)?;
        }
        for rel in ch.put_back {
            let d = ex.deps.iter().find(|d| &d.entry.rel_path == rel).unwrap();
            let to = main_dir.join(rel);
            check_hash(&to, &d.entry).with_context(|| format!("the original of {rel} doesn't match its recorded hash"))?;
            set_times(&to, parse_utc(d.entry.modified_utc.as_deref()), parse_utc(d.entry.created_utc.as_deref()), false);
            let dep = main_dir.join(&d.dep_path);
            if dep.is_file() {
                std::fs::remove_file(&dep).with_context(|| format!("can't remove {} from the work folder", d.dep_path))?;
            }
        }
        let mut deps: Vec<Replaced> = ex.deps.iter().filter(|d| !ch.put_back.contains(&d.entry.rel_path)).cloned().collect();
        deps.extend(replace.iter().cloned());
        deps.sort_by(|a, b| a.entry.rel_path.cmp(&b.entry.rel_path));

        let mut duplicates = ex.duplicates.clone();
        for rel in remove {
            std::fs::remove_file(main_dir.join(rel)).with_context(|| format!("can't remove {rel} from the work folder"))?;
            let mut e = m.entries.iter().find(|e| &e.rel_path == rel).unwrap().clone();
            e.method = "duplicate".into();
            duplicates.push(e);
        }
        duplicates.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

        // What is left out now.
        let mut left = Excluded {
            set_aside_archive: if migrate { None } else { ex.set_aside_archive.clone() },
            set_aside_inside: None,
            set_aside: ex.set_aside.iter().filter(|e| !keep.contains(&e.rel_path)).cloned().collect(),
            discarded: ex.discarded.iter().filter(|e| !keep.contains(&e.rel_path)).cloned().collect(),
            duplicates,
            deps,
        };
        let mut moved = 0;
        if discard_rest {
            moved = left.set_aside.len();
            for mut e in std::mem::take(&mut left.set_aside) {
                e.method = "discard".into();
                left.discarded.push(e);
            }
            left.discarded.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        }
        if left.set_aside.is_empty() {
            left.set_aside_archive = None;
        }

        // The set-aside files' archive to keep inside: the one there already
        // or beside, or written again without the files taken from it.
        let mut inner = None;
        let kept_aside = !left.set_aside.is_empty() && left.set_aside_archive.is_none();
        if let Some(old) = old_aside.as_ref().filter(|_| kept_aside) {
            if left.set_aside.len() == ex.set_aside.len() {
                if ex.set_aside_inside.is_none() {
                    progress.set_phase("Checking the set-aside files", ex.set_aside.iter().map(|e| e.size).sum());
                    verify_archive(old, &ex.set_aside, settings.threads, progress)?;
                }
                inner = Some(old.clone());
            } else {
                let am = read_manifest(old)?;
                // Files only, like the set-aside archive first written.
                let s = Scan { dirs: Vec::new(), ..scan(&aside_dir)? };
                let mut rec = am.archive.clone();
                (rec.archive_path, rec.archive_bytes) = (String::new(), 0);
                (rec.input_bytes, rec.file_count, rec.dir_count) = (s.total_bytes, s.files.len() as u64, s.dirs.len() as u64);
                let new_inner = work.join("excluded.7z");
                progress.set_phase("Compressing the set-aside files", s.total_bytes);
                let entries = write_archive(&s, &new_inner, settings, progress, &rec, &Excluded::default(), None)?;
                progress.set_phase("Verifying the set-aside files", s.total_bytes);
                verify_archive(&new_inner, &entries, settings.threads, progress)?;
                left.set_aside = entries;
                inner = Some(new_inner);
            }
        }
        if let Some(p) = &inner {
            left.set_aside_inside = Some(inner_record(p)?);
        }

        let s = scan(&main_dir)?;
        let mut rec = m.archive.clone();
        (rec.archive_path, rec.archive_bytes) = (String::new(), 0);
        (rec.input_bytes, rec.file_count, rec.dir_count) = (s.total_bytes, s.files.len() as u64, s.dirs.len() as u64);
        let partial = PathBuf::from(format!("{}.partial", archive.display()));
        progress.set_phase("Compressing", s.total_bytes);
        let entries = write_archive(&s, &partial, settings, progress, &rec, &left, inner.as_deref())?;
        progress.set_phase("Verifying", s.total_bytes);
        verify_archive(&partial, &with_inner(&entries, &left), settings.threads, progress)?;
        Ok((partial, rec, entries, left, moved))
    })();
    let _ = std::fs::remove_dir_all(&work);
    let (partial, mut rec, entries, left, moved) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(format!("{}.partial", archive.display()));
            return Err(e);
        }
    };

    // Everything is verified: swap the new archive in and update the catalogue.
    let mut cat = Catalog::open(out_dir)?;
    let main_id = cat.archive_id_for(archive)?;
    let aside_id = match &aside_path {
        Some(p) => cat.archive_id_for(p)?,
        None => None,
    };
    std::fs::rename(&partial, archive).with_context(|| format!("can't replace {}", archive.display()))?;
    rec.archive_bytes = std::fs::metadata(archive)?.len();
    if let Some(id) = main_id {
        cat.replace_contents(id, &rec, &entries, &left)?;
    }
    // The archive beside goes once its files are inside, or discarded.
    let had_aside = ex.set_aside_inside.is_some() || ex.set_aside_archive.is_some();
    let aside_removed = had_aside && left.set_aside.is_empty();
    let moved_inside = migrate && !left.set_aside.is_empty();
    if let Some(p) = aside_path.as_ref().filter(|_| left.set_aside_archive.is_none()) {
        if p.is_file() {
            std::fs::remove_file(p).with_context(|| format!("can't remove {}", p.display()))?;
        }
        if let Some(id) = aside_id {
            cat.remove_archive(id)?;
        }
    }
    if let Some(id) = main_id {
        let names = |v: Vec<&str>| v.join(", ");
        let (what, detail) = if !ch.replace.is_empty() {
            ("reference", format!("replaced by reference files: {}", names(ch.replace.iter().map(|r| r.entry.rel_path.as_str()).collect())))
        } else if !ch.put_back.is_empty() {
            ("unreference", format!("put back from their originals: {}", ch.put_back.join(", ")))
        } else if remove.is_empty() && keep.is_empty() && !discard_rest {
            ("restore", "set-aside files moved inside the archive".to_string())
        } else if remove.is_empty() {
            ("restore", format!("put back: {}", keep.join(", ")))
        } else {
            ("dedupe", format!("removed as duplicates: {}", remove.join(", ")))
        };
        cat.log_run(Some(id), what, &source, &now_utc(), "verified", &detail)?;
    }
    Ok(Restored { put_back: keep.len(), discarded: moved, aside_removed, moved_inside })
}

/// Unpacks every file and folder in `archive` into `dest` except the
/// manifest, keeping their times.
fn extract_all(archive: &Path, dest: &Path, threads: u32, progress: &Arc<Progress>) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut reader = ArchiveReader::open(archive, Password::empty())
        .with_context(|| format!("can't open {}", archive.display()))?
        .with_threads(threads.max(1));
    let time = |has: bool, t: sevenz_turbo::NtTime| has.then(|| SystemTime::from(t));
    let mut dirs = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    reader
        .for_each_entries(|entry, data| {
            let name = entry.name();
            if name == MANIFEST_ENTRY || name == DATA_DIR {
                return Ok(true);
            }
            if name.split(['/', '\\']).any(|p| p.is_empty() || p == "." || p == "..") || name.contains(':') {
                return Err(io::Error::other(format!("{name} has a path that can't be unpacked safely")).into());
            }
            let to = dest.join(name);
            let times = (time(entry.has_last_modified_date, entry.last_modified_date), time(entry.has_creation_date, entry.creation_date));
            if entry.is_directory() {
                std::fs::create_dir_all(&to)?;
                dirs.push((to, times));
                return Ok(true);
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&to)?;
            loop {
                progress.check_cancel()?;
                let k = data.read(&mut buf)?;
                if k == 0 {
                    break;
                }
                io::Write::write_all(&mut out, &buf[..k])?;
                progress.done.fetch_add(k as u64, Ordering::Relaxed);
            }
            drop(out);
            set_times(&to, times.0, times.1, false);
            Ok(true)
        })
        .map_err(|e| {
            if progress.cancel.load(Ordering::Relaxed) { anyhow!("cancelled") } else { anyhow!("can't unpack {}: {e}", archive.display()) }
        })?;
    // Folders last, deepest first, since writing into one changes its time.
    for (dir, (modified, created)) in dirs.iter().rev() {
        set_times(dir, *modified, *created, true);
    }
    Ok(())
}

/// Sets a file's or folder's times where the system allows; best effort.
fn set_times(path: &Path, modified: Option<SystemTime>, created: Option<SystemTime>, is_dir: bool) {
    let mut times = std::fs::FileTimes::new();
    if let Some(m) = modified {
        times = times.set_modified(m);
    }
    #[cfg(windows)]
    if let Some(c) = created {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(c);
    }
    #[cfg(not(windows))]
    let _ = created;
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    #[cfg(windows)]
    if is_dir {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        open.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    #[cfg(not(windows))]
    if is_dir {
        open.write(false).read(true);
    }
    if let Ok(f) = open.open(path) {
        let _ = f.set_times(times);
    }
}

fn check_hash(path: &Path, rec: &EntryRecord) -> Result<()> {
    let mut h = blake3::Hasher::new();
    let mut f = File::open(path)?;
    io::copy(&mut f, &mut h)?;
    if rec.blake3.as_deref() != Some(h.finalize().to_hex().as_str()) {
        bail!("{} doesn't match its recorded hash", rec.rel_path);
    }
    Ok(())
}

/// What the check before a delete found.
pub struct DeleteCheck {
    pub files_checked: u64,
    pub bytes_checked: u64,
    /// Files that will be deleted without being kept anywhere, because a
    /// discard rule left them out when the folder was archived.
    pub discarded: Vec<EntryRecord>,
    /// The second archive holding the set-aside files, if there is one
    /// beside the main archive (older archives).
    pub set_aside_archive: Option<PathBuf>,
    /// Whether the set-aside files are in an archive inside the main one,
    /// which was unpacked and checked too.
    pub set_aside_inside: bool,
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
    let out_dir = archive.parent().ok_or_else(|| anyhow!("{} has no folder", archive.display()))?;
    let check_dir = out_dir.join(DATA_DIR).join(format!("check-{}", chrono::Local::now().format("%Y%m%d-%H%M%S%3f")));
    let rec_files: BTreeMap<&str, &EntryRecord> = recorded
        .iter()
        .chain(&excluded.set_aside)
        .chain(&excluded.discarded)
        .chain(&excluded.duplicates)
        .chain(excluded.deps.iter().map(|d| &d.entry))
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
            // The archive inside can only be read once it is unpacked, so it
            // is saved as the main archive is checked, then checked itself.
            let inner = (aside_archive.is_none() && excluded.set_aside_inside.is_some()).then(|| check_dir.join("excluded.7z"));
            if inner.is_some() {
                std::fs::create_dir_all(&check_dir)?;
            }
            verify_archive_saving(archive, &with_inner(recorded, &excluded), threads, progress, inner.as_deref())?;
            match (&aside_archive, &inner) {
                (Some(p), _) => verify_archive(p, &excluded.set_aside, threads, progress),
                (None, Some(p)) => verify_archive(p, &excluded.set_aside, threads, progress),
                (None, None) => Ok(0),
            }
        });
        (folder.join().unwrap(), arch.join().unwrap())
    });
    let _ = std::fs::remove_dir_all(&check_dir);
    folder_result?;
    archive_result?;
    let discarded = excluded.discarded.iter().filter(|d| now.files.iter().any(|f| f.rel == d.rel_path)).cloned().collect();
    Ok(DeleteCheck {
        files_checked: now.files.len() as u64,
        bytes_checked: now.total_bytes,
        discarded,
        set_aside_inside: excluded.set_aside_inside.is_some() && aside_archive.is_none(),
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
/// A fingerprint of a folder as it is now, without reading file contents:
/// every file's path, size and modified time, and, on NTFS, its change
/// journal number ([`crate::journal`]), which changes with any write even if
/// the timestamps are set back. Taken when a folder passes its check, and
/// again just before it is deleted, so a folder changed in between is
/// caught. Starts `usn:` if the journal numbers were used, `meta:` if not.
pub fn folder_snapshot(source: &Path) -> Result<String> {
    let s = scan(source)?;
    // A volume without a journal answers no file; stop asking after a few.
    let mut journal = true;
    let mut misses = 0;
    let mut rows: Vec<String> = Vec::with_capacity(s.files.len());
    for f in &s.files {
        let usn = if journal { crate::journal::file_usn(&f.abs) } else { None };
        if usn.is_none() && journal {
            misses += 1;
            if misses >= 8 && rows.len() < misses * 2 {
                journal = false;
            }
        }
        let modified = f.modified.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        rows.push(format!("{}\0{}\0{:?}\0{:?}", f.rel, f.size, modified, usn));
    }
    let used_journal = journal && !s.files.is_empty() && misses == 0;
    rows.extend(s.skipped_links.iter().map(|l| format!("link\0{l}")));
    rows.sort();
    let mut h = blake3::Hasher::new();
    for r in rows {
        h.update(r.as_bytes());
        h.update(b"\n");
    }
    Ok(format!("{}:{}", if used_journal { "usn" } else { "meta" }, h.finalize().to_hex()))
}

/// Whether a snapshot from [`folder_snapshot`] used the change journal.
pub fn snapshot_used_journal(snapshot: &str) -> bool {
    snapshot.starts_with("usn:")
}

/// Looks for an archived source folder that was renamed: among the folders
/// beside where it was, one holding exactly the files recorded for it
/// (every path and size, including those the exclusions left out). `skip`
/// leaves out folders known for other reasons, such as other archives'
/// sources. Only metadata is read. `None` if nothing matches, or if more
/// than one folder does.
pub fn find_renamed_source(missing: &Path, archive: &Path, recorded: &[EntryRecord], skip: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    RenameFinder::default().find(missing, archive, recorded, skip)
}

/// [`find_renamed_source`] for many folders: each parent folder's
/// subfolders are listed and scanned once, however many folders are looked
/// for in it.
#[derive(Default)]
pub struct RenameFinder {
    /// Parent folder → its subfolders, each with its files' paths and sizes.
    seen: HashMap<PathBuf, Vec<(PathBuf, BTreeMap<String, u64>)>>,
}

impl RenameFinder {
    pub fn find(&mut self, missing: &Path, archive: &Path, recorded: &[EntryRecord], skip: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
        let parent = missing.parent()?;
        let ex = read_manifest(archive).map(|m| m.excluded).unwrap_or_default();
        let want: BTreeMap<&str, u64> = recorded
            .iter()
            .chain(&ex.set_aside)
            .chain(&ex.discarded)
            .chain(&ex.duplicates)
            .chain(ex.deps.iter().map(|d| &d.entry))
            .filter(|e| !e.is_dir)
            .map(|e| (e.rel_path.as_str(), e.size))
            .collect();
        if want.is_empty() {
            return None;
        }
        let subfolders = self.seen.entry(parent.to_path_buf()).or_insert_with(|| {
            let Ok(rd) = std::fs::read_dir(parent) else { return Vec::new() };
            rd.flatten()
                .map(|d| d.path())
                .filter(|p| p.is_dir())
                .filter_map(|p| {
                    let s = scan(&p).ok()?;
                    let files = s.files.into_iter().map(|f| (f.rel, f.size)).collect();
                    Some((p, files))
                })
                .collect()
        });
        let mut found = None;
        for (p, files) in subfolders.iter() {
            if skip(p.as_path()) || files.len() != want.len() {
                continue;
            }
            if files.iter().all(|(rel, size)| want.get(rel.as_str()) == Some(size)) {
                if found.is_some() {
                    return None;
                }
                found = Some(p.clone());
            }
        }
        found
    }
}

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
    fn a_renamed_archive_is_relinked_not_added_twice() {
        let (_tmp, src, out) = run(true);
        let (a, before) = recorded(&out, &src);
        let old = a.archive_path.file_name().unwrap().to_string_lossy().into_owned();
        let renamed = out.join("Renamed in Explorer.7z");
        std::fs::rename(&a.archive_path, &renamed).unwrap();
        let now = crate::settings::strip_verbatim(&renamed);

        let r = relink_missing(&out).unwrap();
        assert_eq!(r.relinked, vec![(old.clone(), "Renamed in Explorer.7z".to_string())]);
        assert!(r.missing.is_empty());
        let cat = Catalog::open_existing(&out).unwrap();
        let all = cat.archives().unwrap();
        assert_eq!(all.len(), 1, "relinked, not added again");
        assert_eq!(PathBuf::from(&all[0].archive_path), now);
        // Reference files naming the old file name still find it.
        assert_eq!(cat.path_for_alias(&old).unwrap().map(PathBuf::from), Some(now.clone()));
        assert_eq!(original_archive_file(&out, &old), now);
        let (_, after) = recorded(&out, &src);
        assert_eq!(after, before);

        // A copy under a third name is reported, not catalogued twice.
        std::fs::copy(&renamed, out.join("Copy.7z")).unwrap();
        let r = rebuild_catalog(&out).unwrap();
        assert_eq!((r.added, r.already_there, r.copies.len()), (0, 1, 1), "{r:?}");
        assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);
    }

    #[test]
    fn a_renamed_source_folder_is_found_by_its_files() {
        let (_tmp, src, out) = run(true);
        let (a, entries) = recorded(&out, &src);
        let renamed = src.with_file_name("renamed source");
        std::fs::rename(&src, &renamed).unwrap();
        let none = |_: &Path| false;
        assert_eq!(find_renamed_source(&src, &a.archive_path, &entries, &none), Some(renamed.clone()));
        // Skipped folders aren't matched.
        let skip = |p: &Path| p == renamed.as_path();
        assert_eq!(find_renamed_source(&src, &a.archive_path, &entries, &skip), None);
        // A changed file means no match.
        let f = walkdir::WalkDir::new(&renamed).into_iter().flatten().find(|e| e.file_type().is_file()).unwrap();
        std::fs::write(f.path(), b"something else entirely, of another size").unwrap();
        assert_eq!(find_renamed_source(&src, &a.archive_path, &entries, &none), None);
    }

    #[test]
    fn a_folder_snapshot_changes_when_the_folder_does() {
        let (_tmp, src, _out) = run(true);
        let before = folder_snapshot(&src).unwrap();
        assert_eq!(folder_snapshot(&src).unwrap(), before);
        std::fs::write(src.join("new file.txt"), b"hello").unwrap();
        assert_ne!(folder_snapshot(&src).unwrap(), before);
    }

    #[test]
    fn a_missing_archive_with_no_match_stays_missing() {
        let (_tmp, src, out) = run(true);
        let (a, _) = recorded(&out, &src);
        std::fs::remove_file(&a.archive_path).unwrap();
        let r = relink_missing(&out).unwrap();
        assert!(r.relinked.is_empty());
        assert_eq!(r.missing.len(), 1);
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
        // The set-aside files are inside the main archive: the destination holds one archive.
        assert!(o.excluded.set_aside_archive.is_none());
        let inner = o.excluded.set_aside_inside.clone().expect("set-aside archive inside");
        assert_eq!((inner.rel_path.as_str(), inner.method.as_str()), (EXCLUDED_ENTRY, "copy"));
        let names: Vec<String> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.iter().filter(|n| !n.starts_with('.')).collect::<Vec<_>>(), ["src.7z"], "{names:?}");
        let dir = tmp.path().join("unpacked");
        std::fs::create_dir_all(&dir).unwrap();
        let aside = extract_file(&out.join("src.7z"), EXCLUDED_ENTRY, &dir, 2).unwrap();
        check_hash(&aside, &inner).unwrap();
        assert_eq!(std::fs::read(src.join("sub/random.dat")).unwrap().len() as u64, read_manifest(&aside).unwrap().entries[0].size);

        // The folder list finds the main archive, and the catalogue knows what was left out.
        let (a, entries) = recorded(&out, &src);
        assert!(a.archive_path.ends_with("src.7z"));
        assert!(!entries.iter().any(|e| e.rel_path.ends_with(".LNK") || e.rel_path.ends_with(".dat")));
        let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert!(folders[0].aside_inside);
        assert!(folders[0].aside_archive.is_none());
        assert_eq!((folders[0].set_aside.len(), folders[0].discarded.len()), (1, 1));
        assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);

        // Every file is accounted for, so the folder may go.
        let c = check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).unwrap();
        assert_eq!(c.discarded.len(), 1);
        assert!(c.set_aside_inside && c.set_aside_archive.is_none());
        assert!(std::fs::read_dir(out.join(DATA_DIR)).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("check-")));
        // But not if a discarded file has changed since...
        std::fs::write(src.join("sub/Shortcut.LNK"), b"lunk").unwrap();
        assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).is_err());
        std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
        // ...or the set-aside file has.
        std::fs::write(src.join("sub/random.dat"), b"changed").unwrap();
        assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).is_err());
        make_tree(&src);

        // Rebuilding brings back the archive and what was left out.
        Catalog::open_existing(&out).unwrap().clear().unwrap();
        let r = rebuild_catalog(&out).unwrap();
        assert_eq!(r.added, 1, "{r:?}");
        let cat = Catalog::open_existing(&out).unwrap();
        assert_eq!(cat.excluded_folders().unwrap().len(), 1);
        assert!(cat.archived_folder(&src).unwrap().unwrap().archive_path.ends_with("src.7z"));

        // A rule that doesn't compile stops the job before anything is written.
        s.exclusions = vec![Rule { kind: Kind::Regex, pattern: "*.lnk".into(), ..Rule::default() }];
        assert!(archive_folder(&src, &s, &Arc::new(Progress::default())).is_err());
    }

    #[test]
    fn excluded_files_go_back_into_the_archive() {
        use crate::exclude::Action;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let out = tmp.path().join("out");
        make_tree(&src);
        std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
        std::fs::write(src.join("sub/deeper/keep.tmp"), b"keep me").unwrap();
        let mut s = settings(&out, true);
        let rule = |pattern: &str, action| Rule { pattern: pattern.into(), action, ..Rule::default() };
        s.exclusions = vec![rule("*.lnk", Action::Discard), rule("*.dat", Action::SetAside), rule("*.tmp", Action::SetAside)];
        archive_folder(&src, &s, &Arc::new(Progress::default())).unwrap();
        let main = out.join("src.7z");
        let p = Arc::new(Progress::default());
        let inner_files = |main: &Path| {
            let dir = tempfile::tempdir().unwrap();
            let a = extract_file(main, EXCLUDED_ENTRY, dir.path(), 2).unwrap();
            read_manifest(&a).unwrap().entries.iter().filter(|e| !e.is_dir).map(|e| e.rel_path.clone()).collect::<Vec<_>>()
        };
        assert_eq!(inner_files(&main), ["sub/deeper/keep.tmp", "sub/random.dat"]);

        // Not a file that was left out.
        assert!(restore_excluded(&main, &["notes.txt".into()], false, &s, &p).is_err());

        // One set-aside file and the discarded one go back; the other stays aside.
        let keep = vec!["sub/deeper/keep.tmp".to_string(), "sub/Shortcut.LNK".to_string()];
        let r = restore_excluded(&main, &keep, false, &s, &p).unwrap();
        assert_eq!((r.put_back, r.discarded, r.aside_removed), (2, 0, false));
        let m = read_manifest(&main).unwrap();
        assert!(m.entries.iter().any(|e| e.rel_path == "sub/deeper/keep.tmp"));
        assert!(m.entries.iter().any(|e| e.rel_path == "sub/Shortcut.LNK"));
        assert!(m.entries.iter().any(|e| e.rel_path == "empty folder" && e.is_dir));
        assert_eq!(m.archive.file_count, 6);
        assert!(m.excluded.discarded.is_empty());
        assert_eq!(m.excluded.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);
        // The archive inside was written again without the file put back.
        assert_eq!(inner_files(&main), ["sub/random.dat"]);
        assert_eq!(m.excluded.set_aside_inside.as_ref().unwrap().rel_path, EXCLUDED_ENTRY);
        let orig = std::fs::metadata(src.join("notes.txt")).unwrap().modified().unwrap();
        assert_eq!(m.entries.iter().find(|e| e.rel_path == "notes.txt").unwrap().modified_utc, utc(Some(orig)));

        // The catalogue follows, and the folder still checks out for deleting.
        let (a, entries) = recorded(&out, &src);
        assert_eq!(entries.iter().filter(|e| !e.is_dir).count(), 6);
        let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
        assert_eq!((folders[0].set_aside.len(), folders[0].discarded.len()), (1, 0));
        check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

        // Discarding the rest takes the set-aside archive out of the main one.
        let r = restore_excluded(&main, &[], true, &s, &p).unwrap();
        assert_eq!((r.put_back, r.discarded, r.aside_removed), (0, 1, true));
        let m = read_manifest(&main).unwrap();
        assert_eq!(m.excluded.discarded.iter().map(|e| e.method.as_str()).collect::<Vec<_>>(), ["discard"]);
        assert!(m.excluded.set_aside_archive.is_none() && m.excluded.set_aside_inside.is_none());
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_file(&main, EXCLUDED_ENTRY, dir.path(), 2).is_err());
        let cat = Catalog::open_existing(&out).unwrap();
        assert_eq!(cat.archives().unwrap().len(), 1);
        let (a, entries) = recorded(&out, &src);
        let c = check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
        assert_eq!(c.discarded.len(), 1);
        assert!(std::fs::read_dir(out.join(DATA_DIR)).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("work-")));

        // A discarded file can't come back once the folder no longer has it.
        std::fs::remove_file(src.join("sub/random.dat")).unwrap();
        let e = restore_excluded(&main, &["sub/random.dat".into()], false, &s, &p).err().unwrap();
        assert!(format!("{e:#}").contains("no longer has it"), "{e:#}");
        assert!(read_manifest(&main).is_ok());
    }

    /// Archives `src` the way versions before the set-aside files went
    /// inside did: those files in `<name>.excluded.7z` beside the main archive.
    fn archive_with_aside_beside(src: &Path, s: &Settings) -> (PathBuf, PathBuf) {
        let p = Arc::new(Progress::default());
        let scan = scan_excluding(src, &Exclusions::new(&s.exclusions).unwrap()).unwrap();
        std::fs::create_dir_all(&s.out_dir).unwrap();
        let (main, aside) = (s.out_dir.join("src.7z"), s.out_dir.join("src.excluded.7z"));
        let record = |name: &str, key: String, sc: &Scan| ArchiveRecord {
            name: name.into(),
            archive_path: String::new(),
            source_path: key,
            source_display: src.display().to_string(),
            created_utc: now_utc(),
            engine: s.engine_label(),
            level: s.level,
            threads: s.threads,
            input_bytes: sc.total_bytes,
            archive_bytes: 0,
            file_count: sc.files.len() as u64,
            dir_count: sc.dirs.len() as u64,
        };
        let aside_scan = Scan { total_bytes: scan.set_aside.iter().map(|f| f.size).sum(), files: scan.set_aside.clone(), ..Scan::default() };
        let mut aside_rec = record("src (excluded)", format!("{}|excluded", path_key(src)), &aside_scan);
        let aside_entries = write_archive(&aside_scan, &aside, s, &p, &aside_rec, &Excluded::default(), None).unwrap();
        let ex = Excluded {
            set_aside_archive: Some("src.excluded.7z".into()),
            set_aside: aside_entries.clone(),
            discarded: scan.discarded.iter().map(|f| hash_record(f, &p).unwrap()).collect(),
            ..Excluded::default()
        };
        let mut rec = record("src", path_key(src), &scan);
        let entries = write_archive(&scan, &main, s, &p, &rec, &ex, None).unwrap();
        let mut cat = Catalog::open(&s.out_dir).unwrap();
        aside_rec.archive_path = aside.display().to_string();
        cat.insert_archive(&aside_rec, &aside_entries).unwrap();
        rec.archive_path = main.display().to_string();
        let id = cat.insert_archive(&rec, &entries).unwrap();
        cat.insert_excluded(id, &ex).unwrap();
        (main, aside)
    }

    #[test]
    fn set_aside_archives_beside_older_archives_still_work_and_move_inside() {
        use crate::exclude::Action;
        let tmp = tempfile::tempdir().unwrap();
        let src = std::fs::canonicalize(tmp.path()).unwrap().join("src");
        let out = tmp.path().join("out");
        make_tree(&src);
        std::fs::write(src.join("sub/deeper/keep.tmp"), b"keep me").unwrap();
        let mut s = settings(&out, true);
        s.exclusions = vec![
            Rule { pattern: "*.dat".into(), action: Action::SetAside, ..Rule::default() },
            Rule { pattern: "*.tmp".into(), action: Action::SetAside, ..Rule::default() },
        ];
        let p = Arc::new(Progress::default());
        let (main, aside) = archive_with_aside_beside(&src, &s);

        // The check before a delete still reads the archive beside.
        let (a, entries) = recorded(&out, &src);
        let c = check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
        assert_eq!(c.set_aside_archive.as_deref(), Some(aside.as_path()));
        assert!(!c.set_aside_inside);
        let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
        assert_eq!(folders[0].aside_archive.as_deref(), Some(aside.as_path()));
        assert!(!folders[0].aside_inside);

        // Putting one back takes it from the archive beside, and the rest move inside.
        let r = restore_excluded(&main, &["sub/deeper/keep.tmp".into()], false, &s, &p).unwrap();
        assert!(r.moved_inside && !r.aside_removed, "{r:?}");
        assert!(!aside.exists());
        let m = read_manifest(&main).unwrap();
        assert!(m.excluded.set_aside_archive.is_none());
        assert_eq!(m.excluded.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);
        assert!(m.entries.iter().any(|e| e.rel_path == "sub/deeper/keep.tmp"));
        let cat = Catalog::open_existing(&out).unwrap();
        assert_eq!(cat.archives().unwrap().len(), 1, "the archive beside left the catalogue");
        assert!(cat.excluded_folders().unwrap()[0].aside_inside);
        drop(cat);
        let (a, entries) = recorded(&out, &src);
        assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap().set_aside_inside);

        // Moving inside on its own, with nothing put back.
        std::fs::remove_dir_all(&out).unwrap();
        let (main, aside) = archive_with_aside_beside(&src, &s);
        assert!(move_aside_inside(&main, &s, &p).unwrap().moved_inside);
        assert!(!aside.exists());
        assert_eq!(read_manifest(&main).unwrap().excluded.set_aside.len(), 2);
        assert!(move_aside_inside(&main, &s, &p).is_err(), "nothing beside it any more");
        let (a, entries) = recorded(&out, &src);
        check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

        // An archive beside that has gone missing stays named, and blocks a delete.
        std::fs::remove_dir_all(&out).unwrap();
        let (main, aside) = archive_with_aside_beside(&src, &s);
        std::fs::remove_file(&aside).unwrap();
        let (a, entries) = recorded(&out, &src);
        let e = check_before_delete(&src, &a.archive_path, &entries, 2, &p).err().unwrap();
        assert!(format!("{e:#}").contains("set-aside"), "{e:#}");
        assert!(restore_excluded(&main, &["sub/random.dat".into()], false, &s, &p).is_err());
        assert_eq!(read_manifest(&main).unwrap().excluded.set_aside_archive.as_deref(), Some("src.excluded.7z"));
    }

    #[test]
    fn duplicates_are_removed_but_never_the_last_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let s = settings(&out, true);
        let p = Arc::new(Progress::default());
        for name in ["one", "two"] {
            let src = tmp.path().join(name);
            std::fs::create_dir_all(src.join("pics")).unwrap();
            std::fs::write(src.join("pics/same.jpg"), b"identical bytes").unwrap();
            std::fs::write(src.join(format!("{name}.txt")), name).unwrap();
            archive_folder(&src, &s, &p).unwrap();
        }
        let cat = Catalog::open_existing(&out).unwrap();
        let groups = cat.duplicate_groups(10).unwrap();
        assert_eq!(groups.len(), 1);
        let ids: Vec<i64> = groups[0].copies.iter().map(|c| c.entry_id).collect();
        drop(cat);

        // Both copies can't go.
        let e = remove_duplicates(&out, &ids, &s, &p).err().unwrap();
        assert!(format!("{e:#}").contains("no copy left"), "{e:#}");

        // The copy in "two" goes; "one" keeps its own.
        let two = groups[0].copies.iter().find(|c| c.archive_name == "two").unwrap().entry_id;
        let r = remove_duplicates(&out, &[two], &s, &p).unwrap();
        assert_eq!((r.files, r.archives, r.bytes), (1, 1, 15));
        let m = read_manifest(&out.join("two.7z")).unwrap();
        assert!(!m.entries.iter().any(|e| e.rel_path == "pics/same.jpg"));
        assert_eq!(m.excluded.duplicates.iter().map(|e| e.method.as_str()).collect::<Vec<_>>(), ["duplicate"]);
        assert!(read_manifest(&out.join("one.7z")).unwrap().entries.iter().any(|e| e.rel_path == "pics/same.jpg"));
        let cat = Catalog::open_existing(&out).unwrap();
        assert!(cat.duplicate_groups(10).unwrap().is_empty());
        assert!(cat.excluded_folders().unwrap().is_empty());

        // The folder still on disk passes the check before a delete.
        let src = tmp.path().join("two");
        let (a, entries) = recorded(&out, &src);
        check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
    }

    #[test]
    fn duplicates_are_replaced_by_references_and_put_back() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let s = settings(&out, false);
        let p = Arc::new(Progress::default());
        for name in ["one", "two"] {
            let src = tmp.path().join(name);
            std::fs::create_dir_all(src.join("pics")).unwrap();
            std::fs::write(src.join("pics/same.jpg"), "identical bytes ".repeat(1000)).unwrap();
            std::fs::write(src.join(format!("{name}.txt")), name).unwrap();
            if name == "two" {
                std::fs::create_dir_all(src.join("other")).unwrap();
                std::fs::write(src.join("other/again.jpg"), "identical bytes ".repeat(1000)).unwrap();
            }
            archive_folder(&src, &s, &p).unwrap();
        }
        let cat = Catalog::open_existing(&out).unwrap();
        let tree = cat.dedupe_tree().unwrap();
        assert_eq!(tree.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["one", "two"], "oldest first");
        assert_eq!((tree[0].originals, tree[0].leaves.len()), (1, 0));
        let leaves: Vec<(&str, &str, &str)> =
            tree[1].leaves.iter().map(|l| (l.rel_path.as_str(), l.original_archive.as_str(), l.original_path.as_str())).collect();
        assert_eq!(leaves, [("other/again.jpg", "one", "pics/same.jpg"), ("pics/same.jpg", "one", "pics/same.jpg")]);
        let ids: Vec<i64> = tree[1].leaves.iter().map(|l| l.entry_id).collect();
        // The original itself can't be replaced.
        let original = cat.copies_of(&tree[1].leaves[0].blake3, tree[1].leaves[0].size).unwrap()[0].0;
        assert!(cat.dedupe_leaf(original).unwrap().is_none());
        drop(cat);
        let two = out.join("two.7z");
        let before = std::fs::read(&two).unwrap();

        // A dry run checks the original and changes nothing.
        let r = replace_duplicates(&out, &ids, true, &s, &p).unwrap();
        assert_eq!((r.files, r.originals_checked, r.archives.len()), (2, 1, 1));
        assert_eq!(std::fs::read(&two).unwrap(), before);

        // With the original's archive gone, nothing is changed either.
        std::fs::rename(out.join("one.7z"), out.join("one.moved")).unwrap();
        let e = replace_duplicates(&out, &ids, false, &s, &p).err().unwrap();
        assert!(format!("{e:#}").contains("nothing was changed"), "{e:#}");
        assert_eq!(std::fs::read(&two).unwrap(), before);
        std::fs::rename(out.join("one.moved"), out.join("one.7z")).unwrap();

        let r = replace_duplicates(&out, &ids, false, &s, &p).unwrap();
        assert_eq!((r.files, r.bytes), (2, 32_000));
        let m = read_manifest(&two).unwrap();
        let files: Vec<(&str, &str)> = m.entries.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e.method.as_str())).collect();
        assert_eq!(
            files,
            [("other/again.jpg.atticarcdep", "dep"), ("pics/same.jpg.atticarcdep", "dep"), ("two.txt", "lzma2")]
        );
        assert_eq!(m.excluded.deps.len(), 2);
        // The reference file says where the original is.
        let dir = tmp.path().join("x");
        std::fs::create_dir_all(&dir).unwrap();
        let dep = extract_file(&two, "pics/same.jpg.atticarcdep", &dir, 2).unwrap();
        let d: DepFile = serde_json::from_slice(&std::fs::read(dep).unwrap()).unwrap();
        assert_eq!((d.replaced.original_archive.as_str(), d.replaced.original_path.as_str()), ("one.7z", "pics/same.jpg"));

        let cat = Catalog::open_existing(&out).unwrap();
        assert!(cat.dedupe_tree().unwrap().iter().all(|a| a.leaves.is_empty()));
        assert!(cat.duplicate_groups(10).unwrap().is_empty());
        assert_eq!(cat.dedupe_tree().unwrap()[1].replaced, 2);
        // The original is protected now.
        let e = remove_duplicates(&out, &[original], &s, &p).err().unwrap();
        assert!(format!("{e:#}").contains("reference files point to"), "{e:#}");
        let two_id = cat.archive_id_for(&two).unwrap().unwrap();
        let rep = cat.dep_at(two_id, "other/again.jpg.atticarcdep").unwrap().unwrap();
        drop(cat);

        // Following a reference extracts the original under the replaced file's name.
        let got = extract_original(&out, &rep, &dir, 2).unwrap();
        assert_eq!(got.file_name().unwrap(), "again.jpg");
        assert_eq!(std::fs::read(&got).unwrap(), "identical bytes ".repeat(1000).as_bytes());

        // The folder still on disk passes the check before a delete.
        let src = tmp.path().join("two");
        let (a, entries) = recorded(&out, &src);
        check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

        // The references survive a rebuild of the catalogue.
        Catalog::open_existing(&out).unwrap().clear().unwrap();
        rebuild_catalog(&out).unwrap();
        let cat = Catalog::open_existing(&out).unwrap();
        let two_id = cat.archive_id_for(&two).unwrap().unwrap();
        assert_eq!(cat.deps(two_id).unwrap().len(), 2);
        drop(cat);

        // Putting one back brings the real file back into the archive.
        assert_eq!(put_back_replaced(&two, &["pics/same.jpg".to_string()], &s, &p).unwrap(), 1);
        let m = read_manifest(&two).unwrap();
        assert!(m.entries.iter().any(|e| e.rel_path == "pics/same.jpg" && e.method == "lzma2"));
        assert!(!m.entries.iter().any(|e| e.rel_path == "pics/same.jpg.atticarcdep"));
        assert_eq!(m.excluded.deps.len(), 1);
        let cat = Catalog::open_existing(&out).unwrap();
        assert_eq!(cat.dedupe_tree().unwrap()[1].leaves.len(), 1);
        drop(cat);
        let (a, entries) = recorded(&out, &src);
        check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
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
