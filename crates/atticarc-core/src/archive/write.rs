//! Writing an archive of a folder: hashing every file as it is compressed,
//! then verifying the result before it counts.

use crate::catalog::{ArchiveRecord, Catalog, DATA_DIR, EntryRecord, SavedCheck, now_utc, path_key};
use crate::exclude::Exclusions;
use anyhow::{Context, Result, anyhow, bail};
use sevenz_turbo::encoder_options::Lzma2Options;
use sevenz_turbo::{ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod, SourceReader};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::*;

/// Files at least this big go in a block of their own; smaller files are
/// packed into solid blocks that stay under it. Same as sevenz-turbo's
/// own directory helper, which the benchmark used.
pub(super) const MAX_BLOCK: u64 = 4 * 1024 * 1024 * 1024;

/// Extensions of formats that are already compressed. In store mode these
/// are copied into the archive without compressing them again.
pub(super) const STORE_EXTS: &[&str] = &[
    "7z",
    "zip",
    "rar",
    "gz",
    "tgz",
    "bz2",
    "xz",
    "zst",
    "lz4",
    "cab",
    "whl",
    "jar",
    "apk",
    "nupkg",
    "docx",
    "xlsx",
    "pptx",
    "odt",
    "epub",
    "jpg",
    "jpeg",
    "png",
    "gif",
    "webp",
    "heic",
    "avif",
    "mp3",
    "aac",
    "m4a",
    "ogg",
    "opus",
    "flac",
    "mp4",
    "m4v",
    "mkv",
    "mov",
    "avi",
    "webm",
    "wmv",
    "safetensors",
    "gguf",
    "pdf",
    "msi",
    "dmg",
];

#[derive(Clone, Debug)]
pub(super) struct FileHash {
    blake3: String,
    sha256: String,
    bytes: u64,
}

/// Opens its file on first read and hashes every byte it hands to the
/// compressor, so each source byte is read from disk once.
pub(super) struct HashReader {
    path: PathBuf,
    rel: String,
    size: u64,
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
            if self.finished {
                return Ok(0);
            }
            let f = File::open(&self.path).map_err(|e| io::Error::new(e.kind(), format!("can't open {}: {e}", self.path.display())))?;
            self.progress.set_file("compressing", &self.rel, self.size);
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

pub(super) fn lzma2_config(level: u32, threads: u32) -> EncoderConfiguration {
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
pub(super) type Written = (Vec<EntryRecord>, u64);

pub struct ArchiveOutcome {
    pub record: ArchiveRecord,
    pub skipped_links: Vec<String>,
    /// What the exclusions left out, as recorded in the manifest.
    pub excluded: Excluded,
    /// With [`archive_again`]: where the folder's older archive was moved.
    pub superseded: Option<PathBuf>,
    /// With [`archive_again`]: why the older archive was left where it was.
    pub superseded_problem: Option<String>,
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
    let name = source.file_name().ok_or_else(|| anyhow!("can't archive a drive root"))?.to_string_lossy().into_owned();
    let final_path = unique_archive_path(&out_dir, &name);
    // "Name (2)" when "Name" is taken: the archive's own name from here on.
    let name = final_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(name);
    let partial = PathBuf::from(format!("{}.partial", final_path.display()));

    // Snapshots before reading and after verifying (see the end): if they
    // match, the folder didn't change while it was archived.
    progress.set_phase("Taking a snapshot of the folder", 0);
    let snapshot_before = folder_snapshot_with(&source, progress).ok();

    progress.set_phase("Scanning", 0);
    let mut scan = scan_excluding(&source, &exclusions)?;

    // Identical files stored once: the reference files standing in for the
    // later copies are written here, then archived like any other file.
    let deps_dir = PathBuf::from(format!("{}.deps.partial", final_path.display()));
    let deps = if settings.identical_once {
        let archive_file = final_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        match store_identical_once(&mut scan, &deps_dir, &archive_file, settings.store_mode, progress) {
            Ok(d) => d,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&deps_dir);
                return Err(e);
            }
        }
    } else {
        Vec::new()
    };

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
    let aside_scan = Scan { total_bytes: scan.set_aside.iter().map(|f| f.size).sum(), files: scan.set_aside.clone(), ..Scan::default() };
    let aside_partial = (!aside_scan.files.is_empty()).then(|| PathBuf::from(format!("{}.excluded.partial", final_path.display())));
    let aside_record = record_for(format!("{name} (excluded)"), format!("{}|excluded", path_key(&source)), &aside_scan);

    let result = (|| -> Result<(Written, Excluded)> {
        // Must fit before anything is written, as if nothing compresses. The
        // set-aside files count twice: their own archive is written first,
        // then copied inside the main one before it is removed.
        let aside_worst = crate::disk::archive_worst_case(aside_scan.total_bytes);
        let needed = crate::disk::archive_worst_case(scan.total_bytes).saturating_add(aside_worst.saturating_mul(2));
        crate::disk::ensure_free(&out_dir, needed, &format!("archiving {name}"))?;
        let mut excluded = Excluded { deps: deps.clone(), ..Excluded::default() };
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
        // Each first copy was read twice, to find the copies and to archive
        // it: both reads must agree, or a reference would point to other bytes.
        for d in &excluded.deps {
            let original = entries.iter().find(|e| e.rel_path == d.original_path);
            if original.and_then(|o| o.blake3.as_deref()) != d.entry.blake3.as_deref() {
                bail!("{} changed while it was being archived; close whatever is writing to it and try again", d.original_path);
            }
        }
        progress.set_phase("Verifying", scan.total_bytes);
        verify_archive(&partial, &with_inner(&entries, &excluded), settings.threads, progress)?;
        let archive_bytes = std::fs::metadata(&partial)?.len();
        Ok(((entries, archive_bytes), excluded))
    })();
    if let Some(p) = &aside_partial {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir_all(&deps_dir);

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
    std::fs::rename(&partial, &final_path).with_context(|| format!("can't rename {} to {}", partial.display(), final_path.display()))?;
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
    if !excluded.deps.is_empty() {
        let bytes: u64 = excluded.deps.iter().map(|d| d.entry.size).sum();
        detail.push(format!(
            "stored once ({} identical copies, {bytes} bytes): {}",
            excluded.deps.len(),
            excluded.deps.iter().map(|d| format!("{} = {}", d.entry.rel_path, d.original_path)).collect::<Vec<_>>().join(", ")
        ));
    }
    let detail = detail.join("; ");
    cat.log_run(Some(archive_id), "archive", &source, &started, "verified", &detail)?;

    // The folder's check for Clean up, done here rather than later: every
    // file was hashed as it was compressed and the archive was decoded
    // against those hashes. If the folder is the same before and after (on
    // NTFS, by its change journal, which catches any write), the archive
    // matches the folder as it stands, and Clean up only has to compare
    // snapshots before deleting it. Without a change journal, a timestamp
    // snapshot isn't trusted for that; such folders get the full check.
    if let Some(before) = snapshot_before.filter(|s| snapshot_used_journal(s)) {
        progress.set_phase("Taking a snapshot of the folder", 0);
        if folder_snapshot_with(&source, progress).ok().as_deref() == Some(before.as_str()) {
            let (archive_len, archive_modified) = match archive_fingerprint(&final_path) {
                Ok((l, m)) => (Some(l), Some(m)),
                Err(_) => (None, None),
            };
            let _ = cat.save_cleanup_check(&SavedCheck {
                archive_id,
                source_path: source_display.clone(),
                passed: true,
                snapshot: Some(before),
                discarded: excluded.discarded.len(),
                detail: String::new(),
                checked_utc: now_utc(),
                archive_len,
                archive_modified,
            });
        }
    }
    Ok(ArchiveOutcome { record, skipped_links: scan.skipped_links, excluded, superseded: None, superseded_problem: None })
}

/// The record of an archive of set-aside files about to go inside the main
/// archive at [`EXCLUDED_ENTRY`]: its size and hashes, so the main archive's
/// check covers it byte for byte.
pub(super) fn inner_record(path: &Path) -> Result<EntryRecord> {
    let meta = std::fs::metadata(path).with_context(|| format!("can't read {}", path.display()))?;
    let f = ScannedFile {
        abs: path.to_path_buf(),
        rel: EXCLUDED_ENTRY.into(),
        size: meta.len(),
        modified: meta.modified().ok(),
        created: None,
    };
    let mut rec = hash_record(&f, &Arc::new(Progress::default()))?;
    rec.method = "copy".into();
    Ok(rec)
}

/// Hashes one file that isn't archived, for the manifest.
pub(super) fn hash_record(f: &ScannedFile, progress: &Arc<Progress>) -> Result<EntryRecord> {
    progress.set_file("hashing", &f.rel, f.size);
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

/// `name.7z`, or if that's taken `name (2).7z`, `name (3).7z` and so on: a
/// folder with the same name as one archived before (or the same folder
/// archived again) gets an archive and a catalogue entry of its own.
pub(super) fn unique_archive_path(out_dir: &Path, name: &str) -> PathBuf {
    let taken = |p: &Path| p.exists() || PathBuf::from(format!("{}.partial", p.display())).exists();
    let plain = out_dir.join(format!("{name}.7z"));
    if !taken(&plain) {
        return plain;
    }
    (2u32..).map(|n| out_dir.join(format!("{name} ({n}).7z"))).find(|p| !taken(p)).expect("a free name")
}

/// Writes the archive, with the manifest describing `record` and the files
/// last in a small block of its own so it can be read without decoding the
/// rest. `inner` is the archive of set-aside files to store, uncompressed,
/// at [`EXCLUDED_ENTRY`]; `excluded.set_aside_inside` must describe it.
pub(super) fn write_archive(
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

    let groups = [(pack, lzma2_config(settings.level, settings.threads)), (store, EncoderConfiguration::new(EncoderMethod::COPY))];
    for (indices, method) in groups {
        if indices.is_empty() {
            continue;
        }
        w.set_content_methods(vec![method]);
        let reader_for = |i: usize| HashReader {
            path: scan.files[i].abs.clone(),
            rel: scan.files[i].rel.clone(),
            size: scan.files[i].size,
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
            if (size >= MAX_BLOCK || block_bytes + size >= MAX_BLOCK) && !entries.is_empty() {
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
    let manifest = serde_json::to_vec_pretty(&Manifest { archive: record.clone(), entries: records.clone(), excluded: excluded.clone() })?;
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
pub(super) fn file_records(scan: &Scan, hashes: &[Option<FileHash>], is_store: &dyn Fn(&ScannedFile) -> bool) -> Result<Vec<EntryRecord>> {
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
