//! Knowing archives and folders by what they hold, not their names:
//! rebuilding the catalogue, relinking renamed archives, finding renamed folders.

use crate::catalog::{Catalog, EntryRecord};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::*;

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
pub(super) fn archive_files(dir: &Path) -> Result<Vec<PathBuf>> {
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
pub(super) fn catalogue_fingerprints(cat: &Catalog) -> Result<HashMap<String, String>> {
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
pub(super) fn original_archive_file(out_dir: &Path, name: &str) -> PathBuf {
    let plain = out_dir.join(name);
    if plain.is_file() {
        return plain;
    }
    Catalog::open_existing(out_dir).and_then(|c| c.path_for_alias(name)).ok().flatten().map(PathBuf::from).unwrap_or(plain)
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
    seen: HashMap<PathBuf, Vec<Subfolder>>,
}

/// A subfolder and its files' paths and sizes.
pub(super) type Subfolder = (PathBuf, BTreeMap<String, u64>);

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
