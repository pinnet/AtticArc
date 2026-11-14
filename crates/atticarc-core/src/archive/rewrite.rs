//! Rewriting an archive with files put back, removed or replaced.

use crate::catalog::{Catalog, DATA_DIR, EntryRecord, now_utc};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;

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

/// What to change when an archive is rewritten.
#[derive(Default)]
pub(super) struct Changes<'a> {
    /// Left-out files to put back.
    pub(super) keep: &'a [String],
    /// Move the set-aside files not kept to the discarded list.
    pub(super) discard_rest: bool,
    /// Files to remove as duplicates.
    pub(super) remove: &'a [String],
    /// Duplicates to replace by a reference file.
    pub(super) replace: &'a [Replaced],
    /// Files replaced by a reference file to put back, by their path.
    pub(super) put_back: &'a [String],
}

/// Rewrites `archive` with `ch`: puts back left-out files, optionally
/// discards the rest of the set-aside files, removes duplicates, replaces
/// duplicates by reference files, and puts replaced files back.
pub(super) fn rewrite(archive: &Path, ch: &Changes<'_>, settings: &Settings, progress: &Arc<Progress>) -> Result<Restored> {
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
        let needed =
            unpacked.saturating_add(crate::disk::archive_worst_case(unpacked)).saturating_add(crate::disk::archive_worst_case(aside));
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
