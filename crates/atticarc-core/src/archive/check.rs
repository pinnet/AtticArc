//! The checks that guard deleting a folder, the snapshot taken with
//! them, and the delete itself.

use crate::catalog::{DATA_DIR, EntryRecord, path_key};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

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
    is_protected_by(path, &settings.protected)
}

/// Whether `path` is one of the `protected` folders or inside one.
pub fn is_protected_by(path: &Path, protected: &[PathBuf]) -> bool {
    let key = path_key(path);
    protected.iter().any(|p| {
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

pub(super) fn hash_folder_against(now: &Scan, rec: &BTreeMap<&str, &EntryRecord>, progress: &Arc<Progress>) -> Result<()> {
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
    folder_snapshot_with(source, &Progress::default())
}

/// [`folder_snapshot`], counting files in `progress` as it goes.
pub fn folder_snapshot_with(source: &Path, progress: &Progress) -> Result<String> {
    let s = scan(source)?;
    progress.total.store(s.files.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
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
        progress.done.fetch_add(1, Ordering::Relaxed);
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

pub fn delete_folder(source: &Path) -> Result<()> {
    delete_folder_with(source, &Progress::default())
}

/// [`delete_folder`], counting files in `progress` as they go: the files
/// first, one by one, then what is left (folders and links) in one go.
pub fn delete_folder_with(source: &Path, progress: &Progress) -> Result<()> {
    let mut files = Vec::new();
    for item in walkdir::WalkDir::new(source).follow_links(false).into_iter().flatten() {
        if let Ok(meta) = item.path().symlink_metadata() {
            let mut perms = meta.permissions();
            if perms.readonly() && !meta.file_type().is_symlink() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(item.path(), perms);
            }
            if meta.file_type().is_file() {
                files.push(item.into_path());
            }
        }
    }
    progress.total.store(files.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    for f in &files {
        std::fs::remove_file(f).with_context(|| format!("can't delete {}", f.display()))?;
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    std::fs::remove_dir_all(source).with_context(|| format!("can't delete {}", source.display()))
}
