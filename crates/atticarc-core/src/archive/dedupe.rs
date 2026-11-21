//! Duplicates: identical files stored once within a folder, and copies
//! across archives removed or replaced by reference files.

use crate::catalog::{Catalog, EntryRecord, now_utc};
use anyhow::{Context, Result, anyhow, bail};
use sevenz_turbo::{ArchiveReader, Password};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

/// Finds files in `scan` with the same content (same size and BLAKE3 hash,
/// at least [`IDENTICAL_MIN_BYTES`]) and keeps only the first of each set,
/// by path; every later copy is taken out of `scan` and a reference file to
/// the first, written in `dir`, goes in at `<copy>.atticarcdep`. Only files
/// sharing a size with another are read. Returns the copies replaced, as
/// the manifest records them; the folder on disk is not touched.
pub(super) fn store_identical_once(
    scan: &mut Scan,
    dir: &Path,
    archive_file: &str,
    store_mode: bool,
    progress: &Arc<Progress>,
) -> Result<Vec<Replaced>> {
    let mut by_size: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, f) in scan.files.iter().enumerate() {
        if f.size >= IDENTICAL_MIN_BYTES {
            by_size.entry(f.size).or_default().push(i);
        }
    }
    let candidates: Vec<usize> = by_size.into_values().filter(|v| v.len() > 1).flatten().collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    progress.set_phase("Looking for identical files", candidates.iter().map(|&i| scan.files[i].size).sum());
    let mut sets: BTreeMap<(u64, String), Vec<(usize, EntryRecord)>> = BTreeMap::new();
    for i in candidates {
        let rec = hash_record(&scan.files[i], progress)?;
        sets.entry((rec.size, rec.blake3.clone().unwrap_or_default())).or_default().push((i, rec));
    }
    // Windows names ignore case, so a reference file mustn't clash with a file in any case.
    let taken: std::collections::HashSet<String> = scan.files.iter().map(|f| f.rel.to_lowercase()).collect();
    let now = now_utc();
    let mut gone = std::collections::HashSet::new();
    let mut deps = Vec::new();
    let mut added = Vec::new();
    for (_, mut copies) in sets {
        if copies.len() < 2 {
            continue;
        }
        copies.sort_by(|a, b| a.1.rel_path.cmp(&b.1.rel_path));
        let original = copies[0].1.rel_path.clone();
        for (i, mut entry) in copies.into_iter().skip(1) {
            let at = dep_path(&entry.rel_path);
            if taken.contains(&at.to_lowercase()) {
                continue;
            }
            let store = store_mode && STORE_EXTS.contains(&ext_of(&entry.rel_path).as_str());
            entry.method = if store { "copy" } else { "lzma2" }.into();
            let r = Replaced {
                entry,
                dep_path: at.clone(),
                original_archive: archive_file.to_string(),
                original_path: original.clone(),
                replaced_utc: now.clone(),
            };
            std::fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
            let file = dir.join(format!("{}.json", deps.len()));
            std::fs::write(&file, serde_json::to_vec_pretty(&DepFile::new(r.clone()))?)
                .with_context(|| format!("can't write {}", file.display()))?;
            set_times(&file, parse_utc(r.entry.modified_utc.as_deref()), parse_utc(r.entry.created_utc.as_deref()), false);
            let meta = std::fs::metadata(&file)?;
            added.push(ScannedFile { abs: file, rel: at, size: meta.len(), modified: meta.modified().ok(), created: meta.created().ok() });
            gone.insert(i);
            deps.push(r);
        }
    }
    let mut i = 0;
    scan.files.retain(|_| {
        i += 1;
        !gone.contains(&(i - 1))
    });
    scan.files.extend(added);
    scan.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    scan.total_bytes = scan.files.iter().map(|f| f.size).sum();
    deps.sort_by(|a, b| a.entry.rel_path.cmp(&b.entry.rel_path));
    Ok(deps)
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
        let kept = cat.copies_of(hash, f.size)?.into_iter().any(|(other, path)| !entry_ids.contains(&other) && path.is_file());
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
        leaves.push(
            cat.dedupe_leaf(*id)?
                .ok_or_else(|| anyhow!("a file picked is no longer a duplicate in the catalogue; refresh and try again"))?,
        );
    }
    leaves
        .sort_by(|a, b| (&a.archive_created, a.archive_id, &a.entry.rel_path).cmp(&(&b.archive_created, b.archive_id, &b.entry.rel_path)));
    for leaf in leaves {
        if !leaf.original_archive_path.is_file() {
            bail!(
                "{} holds the original of {} but isn't there; nothing was changed",
                leaf.original_archive_path.display(),
                leaf.entry.rel_path
            );
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
pub(super) fn hash_entries(
    archive: &Path,
    rels: &[String],
    threads: u32,
    progress: &Arc<Progress>,
) -> Result<HashMap<String, (String, u64)>> {
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
            if progress.cancel.load(Ordering::Relaxed) {
                anyhow!("cancelled")
            } else {
                anyhow!("{} can't be decoded: {e}", archive.display())
            }
        })?;
    Ok(out)
}
