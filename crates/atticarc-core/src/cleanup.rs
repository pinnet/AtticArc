//! Clean up: deleting archived folders that are still on disk, safely.
//!
//! Shared by the GUI's Clean up page and `atticarc-cli cleanup`:
//!
//! 1. A folder's check is normally done when it is archived: every file is
//!    hashed as it is compressed and the archive decoded against those
//!    hashes, and if the folder's change-journal snapshot is the same before
//!    and after, a passed check is saved then ([`archive::archive_folder`]).
//!    [`check`] runs the full check (every file hashed, the archive decoded)
//!    for folders without one: archived before this, on a volume without a
//!    change journal, changed while archiving, or after a failed delete.
//! 2. [`delete`] deletes a folder only if its saved check passed and, in
//!    seconds rather than hours: its snapshot is unchanged (on NTFS this
//!    catches any write, even one whose timestamps were set back), the
//!    archive's size and modified time are unchanged, and the archive's header
//!    still lists every recorded file. If the archive was last decoded more
//!    than [`DECODE_AGE_DAYS`] ago, it is decoded again first ([`scrub_one`]).
//! 3. [`scrub_one`] decodes an archive again to catch damage since it was made
//!    (bit rot, a bad sector), which no snapshot or size can show. It reads
//!    only the archive, never the folder.
//!
//! Measured on a USB HDD, per 4 TB (sevenz-bench `verify`): the snapshot,
//! fingerprint and header take seconds; decoding the archive 14.5 h; hashing
//! the folder 14.8 h; both at once on one disk 59 h.
//!
//! The folders ticked are saved too, so a run carries on across sessions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::archive::{self, Progress, Settings};
use crate::catalog::{self, Catalog, SavedCheck, ScrubItem, path_key};

/// An archive last decoded longer ago than this is decoded again before its
/// folder is deleted, since after the delete it is the only copy.
pub const DECODE_AGE_DAYS: i64 = 30;

/// Why a protected folder is never checked or deleted.
pub const PROTECTED: &str = "it is protected";

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
    /// When its archive last decoded and matched its record.
    pub decoded_utc: String,
    /// In one of the protected folders: never checked or deleted.
    pub protected: bool,
}

impl Candidate {
    pub fn archive_name(&self) -> String {
        self.archive_path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Whether its saved check passed (and so it may be deleted).
    pub fn passed(&self) -> bool {
        self.check.as_ref().is_some_and(|c| c.passed)
    }

    /// Whether its archive will be decoded again before deleting, because it
    /// was last decoded more than [`DECODE_AGE_DAYS`] ago.
    pub fn needs_decode(&self) -> bool {
        days_since(&self.decoded_utc).is_none_or(|d| d > DECODE_AGE_DAYS)
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
/// made at that same path, and those whose folder isn't. Folders in
/// `protected` are marked so, and get no check.
pub fn candidates(out_dir: &Path, renamed: &BTreeMap<i64, PathBuf>, protected: &[PathBuf]) -> Result<(Vec<Candidate>, Vec<Missing>)> {
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
        let is_protected = archive::is_protected_by(&source, protected);
        // A check made at another path (before a rename, say) doesn't count,
        // and a protected folder has none. Earlier versions saved "it is
        // protected" as a failed check; that says nothing about the folder.
        let check = checks
            .get(&a.id)
            .filter(|c| path_key(Path::new(&c.source_path)) == path_key(&source))
            .filter(|c| !is_protected && !(!c.passed && c.detail == PROTECTED))
            .cloned();
        let decoded_utc = cat.decoded_utc(a.id)?.unwrap_or_else(|| a.created_utc.clone());
        found.push(Candidate {
            archive_id: a.id,
            source,
            renamed_from,
            archive_path,
            bytes: a.input_bytes,
            archived: a.created_utc.get(..10).unwrap_or(&a.created_utc).to_string(),
            check,
            decoded_utc,
            protected: is_protected,
        });
    }
    found.sort_by(|a, b| a.source.cmp(&b.source));
    Ok((found, missing))
}

/// Step 1: the full check of one folder against the catalogue and its
/// archive, and a snapshot of it if it passes. Deletes nothing. The result
/// is saved in the catalogue and returned.
pub fn check(c: &Candidate, settings: &Settings, progress: &Arc<Progress>) -> SavedCheck {
    if archive::is_protected(&c.source, settings) {
        // Not saved: being protected says nothing about the folder or its archive.
        return SavedCheck {
            archive_id: c.archive_id,
            source_path: c.source.to_string_lossy().into_owned(),
            passed: false,
            snapshot: None,
            discarded: 0,
            detail: PROTECTED.into(),
            checked_utc: catalog::now_utc(),
            archive_len: None,
            archive_modified: None,
        };
    }
    let res = (|| -> Result<(String, usize)> {
        let cat = Catalog::open_existing(&settings.out_dir)?;
        let recorded = cat.entries(c.archive_id)?;
        let check = archive::check_before_delete(&c.source, &c.archive_path, &recorded, settings.threads, progress)?;
        // The archive was just decoded and matched.
        let _ = cat.set_decoded(c.archive_id, &catalog::now_utc());
        progress.set_phase("Taking a snapshot of the folder", 0);
        let snapshot = archive::folder_snapshot_with(&c.source, progress)?;
        Ok((snapshot, check.discarded.len()))
    })();
    let fingerprint = res.as_ref().ok().and_then(|_| archive::archive_fingerprint(&c.archive_path).ok());
    let saved = SavedCheck {
        archive_id: c.archive_id,
        source_path: c.source.to_string_lossy().into_owned(),
        passed: res.is_ok(),
        snapshot: res.as_ref().ok().map(|(s, _)| s.clone()),
        discarded: res.as_ref().map(|(_, d)| *d).unwrap_or(0),
        detail: res.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default(),
        checked_utc: catalog::now_utc(),
        archive_len: fingerprint.map(|f| f.0),
        archive_modified: fingerprint.map(|f| f.1),
    };
    if let Ok(cat) = Catalog::open_existing(&settings.out_dir) {
        let _ = cat.save_cleanup_check(&saved);
    }
    saved
}

/// Step 2: deletes one folder, only if its saved check passed, it isn't
/// protected, it hasn't changed since the check, and neither has its archive
/// (size, modified time and header); an archive last decoded more than
/// [`DECODE_AGE_DAYS`] ago is decoded again first. Marks it deleted in the
/// catalogue and logs the run either way.
pub fn delete(c: &Candidate, settings: &Settings, progress: &Arc<Progress>) -> Result<()> {
    let started = catalog::now_utc();
    let result = (|| -> Result<()> {
        let Some(check) = c.check.as_ref().filter(|k| k.passed) else {
            bail!("it hasn't passed a check");
        };
        let Some(snapshot) = check.snapshot.clone() else {
            bail!("it hasn't passed a check");
        };
        if archive::is_protected(&c.source, settings) {
            bail!("{PROTECTED}");
        }
        progress.set_phase("Making sure the folder hasn't changed since its check", 0);
        if archive::folder_snapshot_with(&c.source, progress)? != snapshot {
            bail!("it has changed since it was checked. Check it again");
        }
        progress.set_phase("Making sure the archive hasn't changed", 0);
        if let (Some(len), Some(modified)) = (check.archive_len, check.archive_modified)
            && archive::archive_fingerprint(&c.archive_path)? != (len, modified)
        {
            bail!("its archive has changed since the check (size or modified time). Check it again");
        }
        let cat = Catalog::open_existing(&settings.out_dir)?;
        let recorded = cat.entries(c.archive_id)?;
        archive::check_index(&c.archive_path, &recorded)?;
        if c.needs_decode() {
            let item = ScrubItem {
                archive_id: c.archive_id,
                name: c.archive_name(),
                archive_path: c.archive_path.clone(),
                decoded_utc: c.decoded_utc.clone(),
                archive_bytes: 0,
            };
            scrub_one(&item, settings, progress).context("its archive failed to decode again")?;
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

/// Whole days since `utc` (RFC 3339), if it can be read.
pub fn days_since(utc: &str) -> Option<i64> {
    let t = chrono::DateTime::parse_from_rfc3339(utc).ok()?;
    Some((chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_days())
}

/// Every archive with when it last decoded, oldest first; with `due_only`,
/// only those last decoded more than [`DECODE_AGE_DAYS`] ago.
pub fn scrub_list(out_dir: &Path, due_only: bool) -> Result<Vec<ScrubItem>> {
    if !catalog::exists(out_dir) {
        return Ok(Vec::new());
    }
    let all = Catalog::open_existing(out_dir)?.scrub_list()?;
    Ok(all
        .into_iter()
        .filter(|s| !due_only || days_since(&s.decoded_utc).is_none_or(|d| d > DECODE_AGE_DAYS))
        .collect())
}

/// Decodes one archive again and checks every file against the catalogue,
/// recording the date if it passes. Reads only the archive. Logged either way.
pub fn scrub_one(item: &ScrubItem, settings: &Settings, progress: &Arc<Progress>) -> Result<()> {
    let started = catalog::now_utc();
    let cat = Catalog::open_existing(&settings.out_dir)?;
    let result = (|| -> Result<()> {
        if !item.archive_path.is_file() {
            bail!("{} isn't there", item.archive_path.display());
        }
        let recorded = cat.entries(item.archive_id)?;
        archive::scrub_archive(&item.archive_path, &recorded, settings.threads, progress)?;
        cat.set_decoded(item.archive_id, &catalog::now_utc())?;
        Ok(())
    })();
    let detail = result.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default();
    let outcome = match &result {
        Ok(()) => "verified",
        Err(_) if progress.cancel.load(std::sync::atomic::Ordering::Relaxed) => "cancelled",
        Err(_) => "failed",
    };
    let _ = cat.log_run(Some(item.archive_id), "scrub", &item.archive_path, &started, outcome, &detail);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::archive_folder;

    fn settings(out: &Path) -> Settings {
        Settings { out_dir: out.to_path_buf(), level: 1, store_mode: true, threads: 2, protected: vec![], exclusions: vec![], identical_once: false }
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
        let (mut c, missing) = candidates(out, &BTreeMap::new(), &[]).unwrap();
        assert!(missing.is_empty());
        assert_eq!(c.len(), 1);
        c.remove(0)
    }

    /// Forgets the check saved at archive time, as for a folder archived
    /// before checks were saved then.
    fn forget(out: &Path) -> Candidate {
        let c = only(out);
        Catalog::open_existing(out).unwrap().forget_cleanup_check(c.archive_id).unwrap();
        only(out)
    }

    /// Whether this machine's temp folder keeps an NTFS change journal (the
    /// archive-time check needs one).
    fn journal_here(src: &Path) -> bool {
        archive::snapshot_used_journal(&archive::folder_snapshot(src).unwrap())
    }

    #[test]
    fn a_protected_folder_is_shown_as_protected_not_failed() {
        let (tmp, src, out) = setup();
        let c = forget(&out);
        let mut s = settings(&out);
        s.protected = vec![tmp.path().to_path_buf()];
        let (list, _) = candidates(&out, &BTreeMap::new(), &s.protected).unwrap();
        assert!(list[0].protected && list[0].check.is_none());
        // Checking it saves nothing, and deleting it is refused.
        let k = check(&list[0], &s, &Arc::new(Progress::default()));
        assert_eq!((k.passed, k.detail.as_str()), (false, PROTECTED));
        assert!(only(&out).check.is_none());
        assert!(delete(&list[0], &s, &Arc::new(Progress::default())).is_err());
        assert!(src.is_dir());
        // A failed "it is protected" check saved by an earlier version is dropped once it isn't.
        let old = SavedCheck { passed: false, detail: PROTECTED.into(), snapshot: None, ..k };
        Catalog::open_existing(&out).unwrap().save_cleanup_check(&old).unwrap();
        assert!(!only(&out).protected);
        assert!(only(&out).check.is_none(), "{:?}", only(&out).check);
        assert_eq!(c.archive_id, only(&out).archive_id);
    }

    #[test]
    fn a_folder_is_checked_when_archived_and_deleted_in_one_step() {
        let (_tmp, src, out) = setup();
        if !journal_here(&src) {
            eprintln!("no change journal here: archive-time checks aren't saved");
            assert!(only(&out).check.is_none());
            return;
        }
        let c = only(&out);
        let k = c.check.clone().expect("checked when archived");
        assert!(k.passed && k.archive_len.is_some() && k.archive_modified.is_some(), "{k:?}");
        assert!(!c.needs_decode(), "decoded when archived, today");
        delete(&c, &settings(&out), &Arc::new(Progress::default())).unwrap();
        assert!(!src.exists());
    }

    /// A passed check with the archive's fingerprint, on any system: the
    /// archive-time one where there's a change journal, else a full check.
    fn checked(out: &Path) -> Candidate {
        let c = only(out);
        if !c.passed() {
            assert!(check(&c, &settings(out), &Arc::new(Progress::default())).passed);
        }
        let c = only(out);
        assert!(c.check.as_ref().is_some_and(|k| k.archive_len.is_some()));
        c
    }

    #[test]
    fn a_changed_archive_is_left_alone() {
        let (_tmp, src, out) = setup();
        let c = checked(&out);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        std::fs::File::options().write(true).open(&c.archive_path).unwrap().set_modified(later).unwrap();
        let err = delete(&c, &settings(&out), &Arc::new(Progress::default())).unwrap_err().to_string();
        assert!(err.contains("archive has changed"), "{err}");
        assert!(src.exists());
    }

    #[test]
    fn an_archive_not_decoded_lately_is_decoded_before_deleting() {
        let (_tmp, src, out) = setup();
        let c = checked(&out);
        let cat = Catalog::open_existing(&out).unwrap();
        cat.set_decoded(c.archive_id, "2020-01-01T00:00:00Z").unwrap();
        // Damage inside the archive, size and time put back: only decoding sees it.
        let t = std::fs::metadata(&c.archive_path).unwrap().modified().unwrap();
        let mut bytes = std::fs::read(&c.archive_path).unwrap();
        bytes[40] ^= 0x5A;
        std::fs::write(&c.archive_path, &bytes).unwrap();
        std::fs::File::options().write(true).open(&c.archive_path).unwrap().set_modified(t).unwrap();
        let c = only(&out);
        assert!(c.needs_decode());
        let err = delete(&c, &settings(&out), &Arc::new(Progress::default())).unwrap_err();
        assert!(format!("{err:#}").contains("failed to decode again"), "{err:#}");
        assert!(src.exists(), "a damaged archive never lets its folder go");

        // Repaired, it decodes, the date is recorded, and the folder goes.
        bytes[40] ^= 0x5A;
        std::fs::write(&c.archive_path, &bytes).unwrap();
        std::fs::File::options().write(true).open(&c.archive_path).unwrap().set_modified(t).unwrap();
        delete(&only(&out), &settings(&out), &Arc::new(Progress::default())).unwrap();
        assert!(!src.exists());
        assert!(days_since(&cat.decoded_utc(c.archive_id).unwrap().unwrap()).unwrap() < 1);
    }

    #[test]
    fn scrub_lists_and_decodes() {
        let (_tmp, _src, out) = setup();
        let s = settings(&out);
        assert!(scrub_list(&out, true).unwrap().is_empty(), "made today: not due");
        let all = scrub_list(&out, false).unwrap();
        assert_eq!(all.len(), 1);
        let cat = Catalog::open_existing(&out).unwrap();
        cat.set_decoded(all[0].archive_id, "2020-01-01T00:00:00Z").unwrap();
        let due = scrub_list(&out, true).unwrap();
        assert_eq!(due.len(), 1);
        scrub_one(&due[0], &s, &Arc::new(Progress::default())).unwrap();
        assert!(scrub_list(&out, true).unwrap().is_empty());
    }

    #[test]
    fn a_check_is_saved_and_survives_a_restart() {
        let (_tmp, src, out) = setup();
        let c = forget(&out);
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
        assert!(candidates(&out, &BTreeMap::new(), &[]).unwrap().0.is_empty());
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
        let err = delete(&forget(&out), &settings(&out), &Arc::new(Progress::default())).unwrap_err().to_string();
        assert!(err.contains("hasn't passed a check"), "{err}");
        assert!(src.exists());
    }
}
