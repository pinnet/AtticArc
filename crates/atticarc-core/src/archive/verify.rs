//! Checking an archive against its record: its header, or every byte.

use crate::catalog::EntryRecord;
use anyhow::{Context, Result, anyhow, bail};
use sevenz_turbo::{ArchiveReader, Password};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

/// An archive's size and modified time (Unix nanoseconds): any rewrite of it
/// changes one or the other.
pub fn archive_fingerprint(archive: &Path) -> Result<(u64, i64)> {
    let meta = std::fs::metadata(archive).with_context(|| format!("can't read {}", archive.display()))?;
    let modified = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    Ok((meta.len(), modified))
}

/// Reads only the archive's header (no decoding) and checks that every file
/// recorded for it is listed there with the recorded size. Catches an archive
/// cut short, replaced, or rewritten without these files, in a moment.
pub fn check_index(archive: &Path, recorded: &[EntryRecord]) -> Result<()> {
    let header = sevenz_turbo::Archive::open(archive).map_err(|e| anyhow!("can't read the archive's header: {e}"))?;
    let listed: HashMap<&str, u64> = header.files.iter().filter(|f| !f.is_directory()).map(|f| (f.name(), f.size())).collect();
    let mut problems = Vec::new();
    for e in recorded.iter().filter(|e| !e.is_dir) {
        match listed.get(e.rel_path.as_str()) {
            None => problems.push(format!("{} isn't in the archive", e.rel_path)),
            Some(&n) if n != e.size => problems.push(format!("{} has a different size in the archive", e.rel_path)),
            _ => {}
        }
    }
    if !problems.is_empty() {
        bail!("the archive doesn't list what was recorded:\n{}", summarize(&problems));
    }
    Ok(())
}

/// Decodes the archive again and checks every file against its recorded
/// hash, including the set-aside files (inside it or in their own archive
/// beside it). Reads only the archive, not the folder: this is what catches
/// damage to an archive after it was made (bit rot, a bad sector).
pub fn scrub_archive(archive: &Path, recorded: &[EntryRecord], threads: u32, progress: &Arc<Progress>) -> Result<()> {
    let excluded = read_manifest(archive).map(|m| m.excluded).unwrap_or_default();
    let total = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
    progress.set_phase("Decoding the archive", recorded.iter().map(|e| e.size).sum::<u64>().max(total));
    verify_archive(archive, &with_inner(recorded, &excluded), threads, progress)?;
    if let Some(name) = &excluded.set_aside_archive {
        let aside = archive.with_file_name(name);
        if !aside.is_file() {
            bail!("{} holds this archive's set-aside files but isn't there", aside.display());
        }
        progress.set_phase("Decoding the set-aside files' archive", excluded.set_aside.iter().map(|e| e.size).sum());
        verify_archive(&aside, &excluded.set_aside, threads, progress)?;
    }
    Ok(())
}

/// Decodes every file in `archive` and checks it against `expected`.
/// Nothing is written to disk.
pub fn verify_archive(archive: &Path, expected: &[EntryRecord], threads: u32, progress: &Arc<Progress>) -> Result<u64> {
    verify_archive_saving(archive, expected, threads, progress, None)
}

/// [`verify_archive`], also writing the inner archive of set-aside files to
/// `save_inner` as it is decoded, so it can be checked next without
/// decoding the main archive again.
pub(super) fn verify_archive_saving(
    archive: &Path,
    expected: &[EntryRecord],
    threads: u32,
    progress: &Arc<Progress>,
    save_inner: Option<&Path>,
) -> Result<u64> {
    let want: HashMap<&str, &EntryRecord> = expected.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e)).collect();
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
            progress.set_file("verifying", entry.name(), entry.size());
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
        .map_err(
            |e| {
                if progress.cancel.load(Ordering::Relaxed) { anyhow!("cancelled") } else { anyhow!("archive can't be decoded: {e}") }
            },
        )?;
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
