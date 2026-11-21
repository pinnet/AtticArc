//! Walking a folder to be archived: every file and folder, links apart.

use crate::catalog::DATA_DIR;
use crate::exclude::{Action, Exclusions};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

pub(super) fn rel_name(root: &Path, p: &Path) -> Result<String> {
    let rel = p.strip_prefix(root)?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(parts.join("/"))
}
