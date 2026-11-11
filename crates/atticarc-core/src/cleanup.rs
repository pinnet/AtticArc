//! Clean up: deleting archived folders that are still on disk, safely.
//!
//! Two separate steps, shared by the GUI's Clean up page and
//! `atticarc-cli cleanup`:
//!
//! 1. [`check`] runs the full check on a folder (every file against the
//!    catalogue and hashed, the archive decoded) and deletes nothing. The
//!    result is saved in the catalogue, so checks survive a restart and a
//!    long run can be done over several sessions.
//! 2. [`delete`] deletes a folder only if its saved check passed, and only if
//!    a fresh [`archive::folder_snapshot`] still matches the one taken at the
//!    check; otherwise the folder is left alone. On NTFS the snapshot holds
//!    each file's change journal number, so any write since the check is
//!    caught, even one whose timestamps were set back.
//!
//! The folders ticked are saved too, so a run carries on across sessions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::archive::{self, Progress, Settings};
use crate::catalog::{self, Catalog, SavedCheck, path_key};

/// An archived folder still on disk, not yet deleted.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub archive_id: i64,
    /// Where the folder is now.
    pub source: PathBuf,
    /// The folder's name when it was archived, if it has been renamed since.
    pub renamed_from: Option<String>,
    pub archive_path: PathBuf,
    pub bytes: u64,
    /// The date it was archived, `YYYY-MM-DD`.
    pub archived: String,
    /// Its latest check, if it has been checked at this path.
    pub check: Option<SavedCheck>,
}

impl Candidate {
    pub fn archive_name(&self) -> String {
        self.archive_path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Whether its saved check passed (and so it may be deleted).
    pub fn passed(&self) -> bool {
        self.check.as_ref().is_some_and(|c| c.passed)
    }

    /// Whether its saved check also covered the NTFS change journal, so even
    /// an edit with its timestamps set back would be caught before deleting.
    pub fn journal_checked(&self) -> bool {
        self.check.as_ref().and_then(|c| c.snapshot.as_deref()).is_some_and(archive::snapshot_used_journal)
    }
}

/// An archived folder that isn't where it was, and isn't marked deleted.
#[derive(Clone, Debug)]
pub struct Missing {
    pub archive_id: i64,
    pub source: PathBuf,
    pub archive_path: PathBuf,
}

/// The folders ticked on the Clean up page, saved so they survive a restart.
pub fn ticked(out_dir: &Path) -> Result<Vec<i64>> {
    if !catalog::exists(out_dir) {
        return Ok(Vec::new());
    }
    Catalog::open_existing(out_dir)?.cleanup_ticked()
}

/// Saves the ticked folders.
pub fn set_ticked(out_dir: &Path, ids: &[i64]) -> Result<()> {
    Catalog::open_existing(out_dir)?.set_cleanup_ticked(ids)
}

/// Every archive in `out_dir`'s catalogue not marked deleted: those whose
/// folder is on disk (or found renamed, per `renamed`), with any saved check
/// made at that same path, and those whose folder isn't.
pub fn candidates(out_dir: &Path, renamed: &BTreeMap<i64, PathBuf>) -> Result<(Vec<Candidate>, Vec<Missing>)> {
    if !catalog::exists(out_dir) {
        return Ok((Vec::new(), Vec::new()));
    }
    let cat = Catalog::open_existing(out_dir)?;
    let checks: BTreeMap<i64, SavedCheck> = cat.cleanup_checks()?.into_iter().map(|c| (c.archive_id, c)).collect();
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for a in cat.archives()? {
        if a.source_deleted_utc.is_some() {
            continue;
        }
        let recorded = PathBuf::from(&a.source_path);
        let archive_path = PathBuf::from(&a.archive_path);
        let (source, renamed_from) = if recorded.is_dir() {
            (recorded, None)
        } else if let Some(p) = renamed.get(&a.id).filter(|p| p.is_dir()) {
            let old = recorded.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (p.clone(), Some(old))
        } else {
            missing.push(Missing { archive_id: a.id, source: recorded, archive_path });
            continue;
        };
        // A check made at another path (before a rename, say) doesn't count.
        let check = checks.get(&a.id).filter(|c| path_key(Path::new(&c.source_path)) == path_key(&source)).cloned();
        found.push(Candidate {
            archive_id: a.id,
            source,
            renamed_from,
            archive_path,
            bytes: a.input_bytes,
            archived: a.created_utc.get(..10).unwrap_or(&a.created_utc).to_string(),
            check,
        });
    }
    found.sort_by(|a, b| a.source.cmp(&b.source));
    Ok((found, missing))
}

/// Step 1: the full check of one folder against the catalogue and its
/// archive, and a snapshot of it if it passes. Deletes nothing. The result
/// is saved in the catalogue and returned.
pub fn check(c: &Candidate, settings: &Settings, progress: &Arc<Progress>) -> SavedCheck {
    let res = (|| -> Result<(String, usize)> {
        if archive::is_protected(&c.source, settings) {
            bail!("it is protected");
        }
        let cat = Catalog::open_existing(&settings.out_dir)?;
        let recorded = cat.entries(c.archive_id)?;
        let check = archive::check_before_delete(&c.source, &c.archive_path, &recorded, settings.threads, progress)?;
        progress.set_phase("Taking a snapshot of the folder", 0);
        let snapshot = archive::folder_snapshot_with(&c.source, progress)?;
        Ok((snapshot, check.discarded.len()))
    })();
    let saved = SavedCheck {
        archive_id: c.archive_id,
        source_path: c.source.to_string_lossy().into_owned(),
        passed: res.is_ok(),
        snapshot: res.as_ref().ok().map(|(s, _)| s.clone()),
        discarded: res.as_ref().map(|(_, d)| *d).unwrap_or(0),
        detail: res.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default(),
        checked_utc: catalog::now_utc(),
    };
    if let Ok(cat) = Catalog::open_existing(&settings.out_dir) {
        let _ = cat.save_cleanup_check(&saved);
    }
    saved
}

/// Step 2: deletes one folder, only if its saved check passed, it isn't
/// protected, and it hasn't changed since the check. Marks it deleted in
/// the catalogue and logs the run either way.
pub fn delete(c: &Candidate, settings: &Settings, progress: &Arc<Progress>) -> Result<()> {
    let started = catalog::now_utc();
    let result = (|| -> Result<()> {
        let Some(snapshot) = c.check.as_ref().filter(|k| k.passed).and_then(|k| k.snapshot.clone()) else {
            bail!("it hasn't passed a check");
        };
        if archive::is_protected(&c.source, settings) {
            bail!("it is protected");
        }
        progress.set_phase("Making sure the folder hasn't changed since its check", 0);
        if archive::folder_snapshot_with(&c.source, progress)? != snapshot {
            bail!("it has changed since it was checked. Check it again");
        }
        progress.set_phase("Deleting the folder", 0);
        archive::delete_folder_with(&c.source, progress)?;
        Catalog::open_existing(&settings.out_dir)?.mark_source_deleted(c.archive_id, &catalog::now_utc())?;
        Ok(())
    })();
    if let Ok(cat) = Catalog::open_existing(&settings.out_dir) {
        let detail = result.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default();
        let outcome = if result.is_ok() { "deleted" } else { "failed" };
        let _ = cat.log_run(Some(c.archive_id), "delete", &c.source, &started, outcome, &detail);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::archive_folder;

    fn settings(out: &Path) -> Settings {
        Settings { out_dir: out.to_path_buf(), level: 1, store_mode: true, threads: 2, protected: vec![], exclusions: vec![] }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("Folder");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("sub").join("b.bin"), vec![7u8; 5000]).unwrap();
        let out = tmp.path().join("out");
        archive_folder(&src, &settings(&out), &Arc::new(Progress::default())).unwrap();
        (tmp, src, out)
    }

    fn only(out: &Path) -> Candidate {
        let (mut c, missing) = candidates(out, &BTreeMap::new()).unwrap();
        assert!(missing.is_empty());
        assert_eq!(c.len(), 1);
        c.remove(0)
    }

    #[test]
    fn a_check_is_saved_and_survives_a_restart() {
        let (_tmp, src, out) = setup();
        let c = only(&out);
        assert!(c.check.is_none());
        let saved = check(&c, &settings(&out), &Arc::new(Progress::default()));
        assert!(saved.passed, "{}", saved.detail);
        // As if AtticARC were started again: the check is read back.
        let again = only(&out);
        assert_eq!(again.check.as_ref(), Some(&saved));
        assert!(again.passed());
        // And deleting needs nothing else.
        delete(&again, &settings(&out), &Arc::new(Progress::default())).unwrap();
        assert!(!src.exists());
        assert!(candidates(&out, &BTreeMap::new()).unwrap().0.is_empty());
        assert!(Catalog::open_existing(&out).unwrap().cleanup_checks().unwrap().is_empty());
    }

    #[test]
    fn a_folder_changed_after_its_check_is_left_alone() {
        let (_tmp, src, out) = setup();
        check(&only(&out), &settings(&out), &Arc::new(Progress::default()));
        std::fs::write(src.join("new.txt"), b"added after the check").unwrap();
        let err = delete(&only(&out), &settings(&out), &Arc::new(Progress::default())).unwrap_err().to_string();
        assert!(err.contains("changed since it was checked"), "{err}");
        assert!(src.join("a.txt").exists());
    }

    #[test]
    fn ticks_are_saved_and_dropped_once_deleted() {
        let (_tmp, _src, out) = setup();
        let c = only(&out);
        set_ticked(&out, &[c.archive_id]).unwrap();
        assert_eq!(ticked(&out).unwrap(), vec![c.archive_id]);
        check(&c, &settings(&out), &Arc::new(Progress::default()));
        delete(&only(&out), &settings(&out), &Arc::new(Progress::default())).unwrap();
        assert!(ticked(&out).unwrap().is_empty());
    }

    #[test]
    fn an_unchecked_folder_is_never_deleted() {
        let (_tmp, src, out) = setup();
        let err = delete(&only(&out), &settings(&out), &Arc::new(Progress::default())).unwrap_err().to_string();
        assert!(err.contains("hasn't passed a check"), "{err}");
        assert!(src.exists());
    }
}
