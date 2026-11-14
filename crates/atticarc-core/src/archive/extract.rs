//! Getting files back out of an archive.

use crate::catalog::DATA_DIR;
use anyhow::{Context, Result, anyhow, bail};
use sevenz_turbo::{ArchiveReader, Password};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use super::*;

/// Extracts one file from `archive` into `dest_dir`, keeping only its file
/// name, and returns the path written. Refuses to overwrite an existing file.
pub fn extract_file(archive: &Path, rel_path: &str, dest_dir: &Path, threads: u32) -> Result<PathBuf> {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    extract_file_as(archive, rel_path, &dest_dir.join(name), threads)
}

/// Extracts the file `rel_path` in `archive` to `dest`, refusing to overwrite.
pub(super) fn extract_file_as(archive: &Path, rel_path: &str, dest: &Path, threads: u32) -> Result<PathBuf> {
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
pub(super) fn extract_many(archive: &Path, files: &[(String, PathBuf)], threads: u32) -> Result<()> {
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

/// Unpacks every file and folder in `archive` into `dest` except the
/// manifest, keeping their times.
pub(super) fn extract_all(archive: &Path, dest: &Path, threads: u32, progress: &Arc<Progress>) -> Result<()> {
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
