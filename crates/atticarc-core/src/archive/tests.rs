use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sevenz_turbo::{ArchiveReader, Password};
use sha2::{Digest, Sha256};

use crate::catalog::{ArchiveRecord, Catalog, DATA_DIR, EntryRecord, now_utc, path_key};
use crate::exclude::{Action, Exclusions, Rule};

use super::*;
use std::io::Write;

fn settings(out: &Path, store: bool) -> Settings {
    Settings {
        out_dir: out.to_path_buf(),
        level: 5,
        store_mode: store,
        threads: 4,
        protected: vec![],
        exclusions: vec![],
        identical_once: false,
    }
}

/// A tree with the awkward cases: Unicode names, an empty folder, an
/// empty file, nested folders, compressible and incompressible files.
fn make_tree(root: &Path) {
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(root.join("empty folder")).unwrap();
    std::fs::write(root.join("notes.txt"), "hello world\n".repeat(50_000)).unwrap();
    std::fs::write(root.join("sub/zero.bin"), b"").unwrap();
    std::fs::write(root.join("sub/deeper/caf\u{e9} \u{1f600}.txt"), "unicode name").unwrap();
    let mut noise = Vec::with_capacity(3 << 20);
    let mut x: u64 = 0x1234_5678;
    for _ in 0..(3 << 20) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        noise.push(x as u8);
    }
    std::fs::write(root.join("sub/already.zip"), &noise).unwrap();
    std::fs::write(root.join("sub/random.dat"), &noise[..1 << 20]).unwrap();
}

fn run(store: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("My Folder");
    let out = tmp.path().join("out");
    make_tree(&src);
    let p = Arc::new(Progress::default());
    let o = archive_folder(&src, &settings(&out, store), &p).unwrap();
    assert_eq!(o.record.file_count, 5);
    assert_eq!(o.record.dir_count, 3);
    (tmp, src, out)
}

fn recorded(out: &Path, src: &Path) -> (crate::catalog::ArchivedFolder, Vec<EntryRecord>) {
    let cat = Catalog::open(out).unwrap();
    let a = cat.archived_folder(src).unwrap().expect("archived");
    let e = cat.entries(a.archive_id).unwrap();
    (a, e)
}

#[test]
fn reading_a_missing_catalogue_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("not here");
    assert!(Catalog::open_existing(&out).is_err());
    assert!(!out.exists());
}

#[test]
fn original_folder_keeps_its_spelling() {
    let (_tmp, src, out) = run(true);
    let cat = Catalog::open_existing(&out).unwrap();
    let shown = PathBuf::from(&cat.archives().unwrap()[0].source_path);
    assert_eq!(shown, crate::settings::strip_verbatim(&std::fs::canonicalize(&src).unwrap()));
    assert!(shown.ends_with("My Folder"), "{}", shown.display());
    let hit = &cat.search("notes", None, 5).unwrap()[0];
    assert!(hit.source_path.ends_with("My Folder"), "{}", hit.source_path);
}

#[test]
fn clearing_the_catalogue_keeps_the_archives() {
    let (_tmp, src, out) = run(true);
    let (a, _) = recorded(&out, &src);
    let mut cat = Catalog::open_existing(&out).unwrap();
    assert_eq!(cat.clear().unwrap(), 1);
    assert!(cat.archives().unwrap().is_empty());
    assert!(cat.search("notes", None, 5).unwrap().is_empty());
    assert!(cat.archived_folder(&src).unwrap().is_none());
    assert!(a.archive_path.is_file());
    // Archiving again works and is searchable.
    archive_folder(&src, &settings(&out, true), &Arc::new(Progress::default())).unwrap();
    assert_eq!(Catalog::open_existing(&out).unwrap().search("notes", None, 5).unwrap().len(), 1);
}

#[test]
fn rebuilding_the_catalogue_from_the_archives() {
    let (_tmp, src, out) = run(true);
    let (a, before) = recorded(&out, &src);
    Catalog::open_existing(&out).unwrap().clear().unwrap();
    // Something that isn't an AtticARC archive is reported, not added.
    std::fs::write(out.join("other.7z"), b"not an archive").unwrap();
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.already_there, r.failed.len()), (1, 0, 1), "{r:?}");
    let (b, after) = recorded(&out, &src);
    assert_eq!(after, before);
    assert_eq!(b.archive_path, a.archive_path);
    assert_eq!(b.archive_bytes, a.archive_bytes);
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.already_there), (0, 1));
}

#[test]
fn a_renamed_archive_is_relinked_not_added_twice() {
    let (_tmp, src, out) = run(true);
    let (a, before) = recorded(&out, &src);
    let old = a.archive_path.file_name().unwrap().to_string_lossy().into_owned();
    let renamed = out.join("Renamed in Explorer.7z");
    std::fs::rename(&a.archive_path, &renamed).unwrap();
    let now = crate::settings::strip_verbatim(&renamed);

    let r = relink_missing(&out).unwrap();
    assert_eq!(r.relinked, vec![(old.clone(), "Renamed in Explorer.7z".to_string())]);
    assert!(r.missing.is_empty());
    let cat = Catalog::open_existing(&out).unwrap();
    let all = cat.archives().unwrap();
    assert_eq!(all.len(), 1, "relinked, not added again");
    assert_eq!(PathBuf::from(&all[0].archive_path), now);
    // Reference files naming the old file name still find it.
    assert_eq!(cat.path_for_alias(&old).unwrap().map(PathBuf::from), Some(now.clone()));
    assert_eq!(original_archive_file(&out, &old), now);
    let (_, after) = recorded(&out, &src);
    assert_eq!(after, before);

    // A copy under a third name is reported, not catalogued twice.
    std::fs::copy(&renamed, out.join("Copy.7z")).unwrap();
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.already_there, r.copies.len()), (0, 1, 1), "{r:?}");
    assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);
}

#[test]
fn a_renamed_source_folder_is_found_by_its_files() {
    let (_tmp, src, out) = run(true);
    let (a, entries) = recorded(&out, &src);
    let renamed = src.with_file_name("renamed source");
    std::fs::rename(&src, &renamed).unwrap();
    let none = |_: &Path| false;
    assert_eq!(find_renamed_source(&src, &a.archive_path, &entries, &none), Some(renamed.clone()));
    // Skipped folders aren't matched.
    let skip = |p: &Path| p == renamed.as_path();
    assert_eq!(find_renamed_source(&src, &a.archive_path, &entries, &skip), None);
    // A changed file means no match.
    let f = walkdir::WalkDir::new(&renamed).into_iter().flatten().find(|e| e.file_type().is_file()).unwrap();
    std::fs::write(f.path(), b"something else entirely, of another size").unwrap();
    assert_eq!(find_renamed_source(&src, &a.archive_path, &entries, &none), None);
}

#[test]
fn same_name_different_content_gets_a_numbered_archive() {
    let (_tmp, src, out) = run(true);
    let first = recorded(&out, &src).0;
    assert!(first.archive_path.ends_with("My Folder.7z"));
    // Different files under the same name, archived again.
    std::fs::remove_dir_all(&src).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("other.txt"), b"something else").unwrap();
    let o = archive_folder(&src, &settings(&out, true), &Arc::new(Progress::default())).unwrap();
    assert_eq!(o.record.name, "My Folder (2)");
    assert!(o.record.archive_path.ends_with("My Folder (2).7z"), "{}", o.record.archive_path);
    // Both archives are there and intact; the folder now goes with the new one.
    let (second, entries) = recorded(&out, &src);
    assert!(second.archive_path.ends_with("My Folder (2).7z"));
    assert_eq!(entries.iter().filter(|e| !e.is_dir).count(), 1);
    let cat = Catalog::open(&out).unwrap();
    verify_archive(&first.archive_path, &cat.entries(first.archive_id).unwrap(), 2, &Arc::new(Progress::default())).unwrap();
    // A third time: "(3)".
    std::fs::write(src.join("more.txt"), b"and more").unwrap();
    let o = archive_folder(&src, &settings(&out, true), &Arc::new(Progress::default())).unwrap();
    assert!(o.record.archive_path.ends_with("My Folder (3).7z"));
}

#[test]
fn index_and_scrub_catch_what_they_should() {
    let (_tmp, src, out) = run(true);
    let (a, entries) = recorded(&out, &src);
    check_index(&a.archive_path, &entries).unwrap();
    scrub_archive(&a.archive_path, &entries, 2, &Arc::new(Progress::default())).unwrap();
    // A file the archive doesn't list.
    let mut more = entries.clone();
    more.push(EntryRecord { rel_path: "not/there.txt".into(), is_dir: false, size: 3, ..entries[0].clone() });
    assert!(check_index(&a.archive_path, &more).is_err());
    // Damage inside the stored data: the header still reads, decoding fails.
    let mut bytes = std::fs::read(&a.archive_path).unwrap();
    bytes[40] ^= 0x5A;
    std::fs::write(&a.archive_path, &bytes).unwrap();
    check_index(&a.archive_path, &entries).unwrap();
    assert!(scrub_archive(&a.archive_path, &entries, 2, &Arc::new(Progress::default())).is_err());
    // Cut short: even the header can't be read.
    std::fs::write(&a.archive_path, &bytes[..bytes.len() - 1024]).unwrap();
    assert!(check_index(&a.archive_path, &entries).is_err());
}

#[test]
fn a_folder_snapshot_changes_when_the_folder_does() {
    let (_tmp, src, _out) = run(true);
    let before = folder_snapshot(&src).unwrap();
    assert_eq!(folder_snapshot(&src).unwrap(), before);
    std::fs::write(src.join("new file.txt"), b"hello").unwrap();
    assert_ne!(folder_snapshot(&src).unwrap(), before);
}

#[test]
fn a_missing_archive_with_no_match_stays_missing() {
    let (_tmp, src, out) = run(true);
    let (a, _) = recorded(&out, &src);
    std::fs::remove_file(&a.archive_path).unwrap();
    let r = relink_missing(&out).unwrap();
    assert!(r.relinked.is_empty());
    assert_eq!(r.missing.len(), 1);
}

#[test]
fn old_catalogue_moves_into_its_folder() {
    let (_tmp, _src, out) = run(true);
    let db = crate::catalog::db_path(&out);
    std::fs::rename(&db, out.join("catalog.db")).unwrap();
    std::fs::remove_dir_all(out.join(".atticarc")).unwrap();
    assert!(crate::catalog::exists(&out));
    assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);
    assert!(db.is_file());
    assert!(!out.join("catalog.db").exists());
}

#[test]
fn excluded_files_are_set_aside_or_discarded() {
    use crate::exclude::{Action, Kind};
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let out = tmp.path().join("out");
    make_tree(&src);
    std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
    let mut s = settings(&out, true);
    let rule = |pattern: &str, action| Rule { pattern: pattern.into(), action, ..Rule::default() };
    s.exclusions = vec![rule("*.lnk", Action::Discard), rule("*.dat", Action::SetAside)];
    let o = archive_folder(&src, &s, &Arc::new(Progress::default())).unwrap();
    assert_eq!(o.record.file_count, 4);
    assert_eq!(o.excluded.discarded.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/Shortcut.LNK"]);
    assert_eq!(o.excluded.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);
    // The set-aside files are inside the main archive: the destination holds one archive.
    assert!(o.excluded.set_aside_archive.is_none());
    let inner = o.excluded.set_aside_inside.clone().expect("set-aside archive inside");
    assert_eq!((inner.rel_path.as_str(), inner.method.as_str()), (EXCLUDED_ENTRY, "copy"));
    let names: Vec<String> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names.iter().filter(|n| !n.starts_with('.')).collect::<Vec<_>>(), ["src.7z"], "{names:?}");
    let dir = tmp.path().join("unpacked");
    std::fs::create_dir_all(&dir).unwrap();
    let aside = extract_file(&out.join("src.7z"), EXCLUDED_ENTRY, &dir, 2).unwrap();
    check_hash(&aside, &inner).unwrap();
    assert_eq!(std::fs::read(src.join("sub/random.dat")).unwrap().len() as u64, read_manifest(&aside).unwrap().entries[0].size);

    // The folder list finds the main archive, and the catalogue knows what was left out.
    let (a, entries) = recorded(&out, &src);
    assert!(a.archive_path.ends_with("src.7z"));
    assert!(!entries.iter().any(|e| e.rel_path.ends_with(".LNK") || e.rel_path.ends_with(".dat")));
    let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
    assert_eq!(folders.len(), 1);
    assert!(folders[0].aside_inside);
    assert!(folders[0].aside_archive.is_none());
    assert_eq!((folders[0].set_aside.len(), folders[0].discarded.len()), (1, 1));
    assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);

    // Every file is accounted for, so the folder may go.
    let c = check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).unwrap();
    assert_eq!(c.discarded.len(), 1);
    assert!(c.set_aside_inside && c.set_aside_archive.is_none());
    assert!(std::fs::read_dir(out.join(DATA_DIR)).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("check-")));
    // But not if a discarded file has changed since...
    std::fs::write(src.join("sub/Shortcut.LNK"), b"lunk").unwrap();
    assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).is_err());
    std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
    // ...or the set-aside file has.
    std::fs::write(src.join("sub/random.dat"), b"changed").unwrap();
    assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &Arc::new(Progress::default())).is_err());
    make_tree(&src);

    // Rebuilding brings back the archive and what was left out.
    Catalog::open_existing(&out).unwrap().clear().unwrap();
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!(r.added, 1, "{r:?}");
    let cat = Catalog::open_existing(&out).unwrap();
    assert_eq!(cat.excluded_folders().unwrap().len(), 1);
    assert!(cat.archived_folder(&src).unwrap().unwrap().archive_path.ends_with("src.7z"));

    // A rule that doesn't compile stops the job before anything is written.
    s.exclusions = vec![Rule { kind: Kind::Regex, pattern: "*.lnk".into(), ..Rule::default() }];
    assert!(archive_folder(&src, &s, &Arc::new(Progress::default())).is_err());
}

#[test]
fn excluded_files_go_back_into_the_archive() {
    use crate::exclude::Action;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let out = tmp.path().join("out");
    make_tree(&src);
    std::fs::write(src.join("sub/Shortcut.LNK"), b"link").unwrap();
    std::fs::write(src.join("sub/deeper/keep.tmp"), b"keep me").unwrap();
    let mut s = settings(&out, true);
    let rule = |pattern: &str, action| Rule { pattern: pattern.into(), action, ..Rule::default() };
    s.exclusions = vec![rule("*.lnk", Action::Discard), rule("*.dat", Action::SetAside), rule("*.tmp", Action::SetAside)];
    archive_folder(&src, &s, &Arc::new(Progress::default())).unwrap();
    let main = out.join("src.7z");
    let p = Arc::new(Progress::default());
    let inner_files = |main: &Path| {
        let dir = tempfile::tempdir().unwrap();
        let a = extract_file(main, EXCLUDED_ENTRY, dir.path(), 2).unwrap();
        read_manifest(&a).unwrap().entries.iter().filter(|e| !e.is_dir).map(|e| e.rel_path.clone()).collect::<Vec<_>>()
    };
    assert_eq!(inner_files(&main), ["sub/deeper/keep.tmp", "sub/random.dat"]);

    // Not a file that was left out.
    assert!(restore_excluded(&main, &["notes.txt".into()], false, &s, &p).is_err());

    // One set-aside file and the discarded one go back; the other stays aside.
    let keep = vec!["sub/deeper/keep.tmp".to_string(), "sub/Shortcut.LNK".to_string()];
    let r = restore_excluded(&main, &keep, false, &s, &p).unwrap();
    assert_eq!((r.put_back, r.discarded, r.aside_removed), (2, 0, false));
    let m = read_manifest(&main).unwrap();
    assert!(m.entries.iter().any(|e| e.rel_path == "sub/deeper/keep.tmp"));
    assert!(m.entries.iter().any(|e| e.rel_path == "sub/Shortcut.LNK"));
    assert!(m.entries.iter().any(|e| e.rel_path == "empty folder" && e.is_dir));
    assert_eq!(m.archive.file_count, 6);
    assert!(m.excluded.discarded.is_empty());
    assert_eq!(m.excluded.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);
    // The archive inside was written again without the file put back.
    assert_eq!(inner_files(&main), ["sub/random.dat"]);
    assert_eq!(m.excluded.set_aside_inside.as_ref().unwrap().rel_path, EXCLUDED_ENTRY);
    let orig = std::fs::metadata(src.join("notes.txt")).unwrap().modified().unwrap();
    assert_eq!(m.entries.iter().find(|e| e.rel_path == "notes.txt").unwrap().modified_utc, utc(Some(orig)));

    // The catalogue follows, and the folder still checks out for deleting.
    let (a, entries) = recorded(&out, &src);
    assert_eq!(entries.iter().filter(|e| !e.is_dir).count(), 6);
    let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
    assert_eq!((folders[0].set_aside.len(), folders[0].discarded.len()), (1, 0));
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

    // Discarding the rest takes the set-aside archive out of the main one.
    let r = restore_excluded(&main, &[], true, &s, &p).unwrap();
    assert_eq!((r.put_back, r.discarded, r.aside_removed), (0, 1, true));
    let m = read_manifest(&main).unwrap();
    assert_eq!(m.excluded.discarded.iter().map(|e| e.method.as_str()).collect::<Vec<_>>(), ["discard"]);
    assert!(m.excluded.set_aside_archive.is_none() && m.excluded.set_aside_inside.is_none());
    let dir = tempfile::tempdir().unwrap();
    assert!(extract_file(&main, EXCLUDED_ENTRY, dir.path(), 2).is_err());
    let cat = Catalog::open_existing(&out).unwrap();
    assert_eq!(cat.archives().unwrap().len(), 1);
    let (a, entries) = recorded(&out, &src);
    let c = check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
    assert_eq!(c.discarded.len(), 1);
    assert!(std::fs::read_dir(out.join(DATA_DIR)).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("work-")));

    // A discarded file can't come back once the folder no longer has it.
    std::fs::remove_file(src.join("sub/random.dat")).unwrap();
    let e = restore_excluded(&main, &["sub/random.dat".into()], false, &s, &p).err().unwrap();
    assert!(format!("{e:#}").contains("no longer has it"), "{e:#}");
    assert!(read_manifest(&main).is_ok());
}

/// Archives `src` the way versions before the set-aside files went
/// inside did: those files in `<name>.excluded.7z` beside the main archive.
fn archive_with_aside_beside(src: &Path, s: &Settings) -> (PathBuf, PathBuf) {
    let p = Arc::new(Progress::default());
    let scan = scan_excluding(src, &Exclusions::new(&s.exclusions).unwrap()).unwrap();
    std::fs::create_dir_all(&s.out_dir).unwrap();
    let (main, aside) = (s.out_dir.join("src.7z"), s.out_dir.join("src.excluded.7z"));
    let record = |name: &str, key: String, sc: &Scan| ArchiveRecord {
        name: name.into(),
        archive_path: String::new(),
        source_path: key,
        source_display: src.display().to_string(),
        created_utc: now_utc(),
        engine: s.engine_label(),
        level: s.level,
        threads: s.threads,
        input_bytes: sc.total_bytes,
        archive_bytes: 0,
        file_count: sc.files.len() as u64,
        dir_count: sc.dirs.len() as u64,
    };
    let aside_scan = Scan { total_bytes: scan.set_aside.iter().map(|f| f.size).sum(), files: scan.set_aside.clone(), ..Scan::default() };
    let mut aside_rec = record("src (excluded)", format!("{}|excluded", path_key(src)), &aside_scan);
    let aside_entries = write_archive(&aside_scan, &aside, s, &p, &aside_rec, &Excluded::default(), None).unwrap();
    let ex = Excluded {
        set_aside_archive: Some("src.excluded.7z".into()),
        set_aside: aside_entries.clone(),
        discarded: scan.discarded.iter().map(|f| hash_record(f, &p).unwrap()).collect(),
        ..Excluded::default()
    };
    let mut rec = record("src", path_key(src), &scan);
    let entries = write_archive(&scan, &main, s, &p, &rec, &ex, None).unwrap();
    let mut cat = Catalog::open(&s.out_dir).unwrap();
    aside_rec.archive_path = aside.display().to_string();
    cat.insert_archive(&aside_rec, &aside_entries).unwrap();
    rec.archive_path = main.display().to_string();
    let id = cat.insert_archive(&rec, &entries).unwrap();
    cat.insert_excluded(id, &ex).unwrap();
    (main, aside)
}

#[test]
fn set_aside_archives_beside_older_archives_still_work_and_move_inside() {
    use crate::exclude::Action;
    let tmp = tempfile::tempdir().unwrap();
    let src = std::fs::canonicalize(tmp.path()).unwrap().join("src");
    let out = tmp.path().join("out");
    make_tree(&src);
    std::fs::write(src.join("sub/deeper/keep.tmp"), b"keep me").unwrap();
    let mut s = settings(&out, true);
    s.exclusions = vec![
        Rule { pattern: "*.dat".into(), action: Action::SetAside, ..Rule::default() },
        Rule { pattern: "*.tmp".into(), action: Action::SetAside, ..Rule::default() },
    ];
    let p = Arc::new(Progress::default());
    let (main, aside) = archive_with_aside_beside(&src, &s);

    // The check before a delete still reads the archive beside.
    let (a, entries) = recorded(&out, &src);
    let c = check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
    assert_eq!(c.set_aside_archive.as_deref(), Some(aside.as_path()));
    assert!(!c.set_aside_inside);
    let folders = Catalog::open_existing(&out).unwrap().excluded_folders().unwrap();
    assert_eq!(folders[0].aside_archive.as_deref(), Some(aside.as_path()));
    assert!(!folders[0].aside_inside);

    // Putting one back takes it from the archive beside, and the rest move inside.
    let r = restore_excluded(&main, &["sub/deeper/keep.tmp".into()], false, &s, &p).unwrap();
    assert!(r.moved_inside && !r.aside_removed, "{r:?}");
    assert!(!aside.exists());
    let m = read_manifest(&main).unwrap();
    assert!(m.excluded.set_aside_archive.is_none());
    assert_eq!(m.excluded.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);
    assert!(m.entries.iter().any(|e| e.rel_path == "sub/deeper/keep.tmp"));
    let cat = Catalog::open_existing(&out).unwrap();
    assert_eq!(cat.archives().unwrap().len(), 1, "the archive beside left the catalogue");
    assert!(cat.excluded_folders().unwrap()[0].aside_inside);
    drop(cat);
    let (a, entries) = recorded(&out, &src);
    assert!(check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap().set_aside_inside);

    // Moving inside on its own, with nothing put back.
    std::fs::remove_dir_all(&out).unwrap();
    let (main, aside) = archive_with_aside_beside(&src, &s);
    assert!(move_aside_inside(&main, &s, &p).unwrap().moved_inside);
    assert!(!aside.exists());
    assert_eq!(read_manifest(&main).unwrap().excluded.set_aside.len(), 2);
    assert!(move_aside_inside(&main, &s, &p).is_err(), "nothing beside it any more");
    let (a, entries) = recorded(&out, &src);
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

    // An archive beside that has gone missing stays named, and blocks a delete.
    std::fs::remove_dir_all(&out).unwrap();
    let (main, aside) = archive_with_aside_beside(&src, &s);
    std::fs::remove_file(&aside).unwrap();
    let (a, entries) = recorded(&out, &src);
    let e = check_before_delete(&src, &a.archive_path, &entries, 2, &p).err().unwrap();
    assert!(format!("{e:#}").contains("set-aside"), "{e:#}");
    assert!(restore_excluded(&main, &["sub/random.dat".into()], false, &s, &p).is_err());
    assert_eq!(read_manifest(&main).unwrap().excluded.set_aside_archive.as_deref(), Some("src.excluded.7z"));
}

#[test]
fn duplicates_are_removed_but_never_the_last_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let s = settings(&out, true);
    let p = Arc::new(Progress::default());
    for name in ["one", "two"] {
        let src = tmp.path().join(name);
        std::fs::create_dir_all(src.join("pics")).unwrap();
        std::fs::write(src.join("pics/same.jpg"), b"identical bytes").unwrap();
        std::fs::write(src.join(format!("{name}.txt")), name).unwrap();
        archive_folder(&src, &s, &p).unwrap();
    }
    let cat = Catalog::open_existing(&out).unwrap();
    let groups = cat.duplicate_groups(10).unwrap();
    assert_eq!(groups.len(), 1);
    let ids: Vec<i64> = groups[0].copies.iter().map(|c| c.entry_id).collect();
    drop(cat);

    // Both copies can't go.
    let e = remove_duplicates(&out, &ids, &s, &p).err().unwrap();
    assert!(format!("{e:#}").contains("no copy left"), "{e:#}");

    // The copy in "two" goes; "one" keeps its own.
    let two = groups[0].copies.iter().find(|c| c.archive_name == "two").unwrap().entry_id;
    let r = remove_duplicates(&out, &[two], &s, &p).unwrap();
    assert_eq!((r.files, r.archives, r.bytes), (1, 1, 15));
    let m = read_manifest(&out.join("two.7z")).unwrap();
    assert!(!m.entries.iter().any(|e| e.rel_path == "pics/same.jpg"));
    assert_eq!(m.excluded.duplicates.iter().map(|e| e.method.as_str()).collect::<Vec<_>>(), ["duplicate"]);
    assert!(read_manifest(&out.join("one.7z")).unwrap().entries.iter().any(|e| e.rel_path == "pics/same.jpg"));
    let cat = Catalog::open_existing(&out).unwrap();
    assert!(cat.duplicate_groups(10).unwrap().is_empty());
    assert!(cat.excluded_folders().unwrap().is_empty());

    // The folder still on disk passes the check before a delete.
    let src = tmp.path().join("two");
    let (a, entries) = recorded(&out, &src);
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
}

#[test]
fn duplicates_are_replaced_by_references_and_put_back() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let s = settings(&out, false);
    let p = Arc::new(Progress::default());
    for name in ["one", "two"] {
        let src = tmp.path().join(name);
        std::fs::create_dir_all(src.join("pics")).unwrap();
        std::fs::write(src.join("pics/same.jpg"), "identical bytes ".repeat(1000)).unwrap();
        std::fs::write(src.join(format!("{name}.txt")), name).unwrap();
        if name == "two" {
            std::fs::create_dir_all(src.join("other")).unwrap();
            std::fs::write(src.join("other/again.jpg"), "identical bytes ".repeat(1000)).unwrap();
        }
        archive_folder(&src, &s, &p).unwrap();
    }
    let cat = Catalog::open_existing(&out).unwrap();
    let tree = cat.dedupe_tree().unwrap();
    assert_eq!(tree.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["one", "two"], "oldest first");
    assert_eq!((tree[0].originals, tree[0].leaves.len()), (1, 0));
    let leaves: Vec<(&str, &str, &str)> =
        tree[1].leaves.iter().map(|l| (l.rel_path.as_str(), l.original_archive.as_str(), l.original_path.as_str())).collect();
    assert_eq!(leaves, [("other/again.jpg", "one", "pics/same.jpg"), ("pics/same.jpg", "one", "pics/same.jpg")]);
    let ids: Vec<i64> = tree[1].leaves.iter().map(|l| l.entry_id).collect();
    // The original itself can't be replaced.
    let original = cat.copies_of(&tree[1].leaves[0].blake3, tree[1].leaves[0].size).unwrap()[0].0;
    assert!(cat.dedupe_leaf(original).unwrap().is_none());
    drop(cat);
    let two = out.join("two.7z");
    let before = std::fs::read(&two).unwrap();

    // A dry run checks the original and changes nothing.
    let r = replace_duplicates(&out, &ids, true, &s, &p).unwrap();
    assert_eq!((r.files, r.originals_checked, r.archives.len()), (2, 1, 1));
    assert_eq!(std::fs::read(&two).unwrap(), before);

    // With the original's archive gone, nothing is changed either.
    std::fs::rename(out.join("one.7z"), out.join("one.moved")).unwrap();
    let e = replace_duplicates(&out, &ids, false, &s, &p).err().unwrap();
    assert!(format!("{e:#}").contains("nothing was changed"), "{e:#}");
    assert_eq!(std::fs::read(&two).unwrap(), before);
    std::fs::rename(out.join("one.moved"), out.join("one.7z")).unwrap();

    let r = replace_duplicates(&out, &ids, false, &s, &p).unwrap();
    assert_eq!((r.files, r.bytes), (2, 32_000));
    let m = read_manifest(&two).unwrap();
    let files: Vec<(&str, &str)> = m.entries.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e.method.as_str())).collect();
    assert_eq!(files, [("other/again.jpg.atticarcdep", "dep"), ("pics/same.jpg.atticarcdep", "dep"), ("two.txt", "lzma2")]);
    assert_eq!(m.excluded.deps.len(), 2);
    // The reference file says where the original is.
    let dir = tmp.path().join("x");
    std::fs::create_dir_all(&dir).unwrap();
    let dep = extract_file(&two, "pics/same.jpg.atticarcdep", &dir, 2).unwrap();
    let d: DepFile = serde_json::from_slice(&std::fs::read(dep).unwrap()).unwrap();
    assert_eq!((d.replaced.original_archive.as_str(), d.replaced.original_path.as_str()), ("one.7z", "pics/same.jpg"));

    let cat = Catalog::open_existing(&out).unwrap();
    assert!(cat.dedupe_tree().unwrap().iter().all(|a| a.leaves.is_empty()));
    assert!(cat.duplicate_groups(10).unwrap().is_empty());
    assert_eq!(cat.dedupe_tree().unwrap()[1].replaced, 2);
    // The original is protected now.
    let e = remove_duplicates(&out, &[original], &s, &p).err().unwrap();
    assert!(format!("{e:#}").contains("reference files point to"), "{e:#}");
    let two_id = cat.archive_id_for(&two).unwrap().unwrap();
    let rep = cat.dep_at(two_id, "other/again.jpg.atticarcdep").unwrap().unwrap();
    drop(cat);

    // Following a reference extracts the original under the replaced file's name.
    let got = extract_original(&out, &rep, &dir, 2).unwrap();
    assert_eq!(got.file_name().unwrap(), "again.jpg");
    assert_eq!(std::fs::read(&got).unwrap(), "identical bytes ".repeat(1000).as_bytes());

    // The folder still on disk passes the check before a delete.
    let src = tmp.path().join("two");
    let (a, entries) = recorded(&out, &src);
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

    // The references survive a rebuild of the catalogue.
    Catalog::open_existing(&out).unwrap().clear().unwrap();
    rebuild_catalog(&out).unwrap();
    let cat = Catalog::open_existing(&out).unwrap();
    let two_id = cat.archive_id_for(&two).unwrap().unwrap();
    assert_eq!(cat.deps(two_id).unwrap().len(), 2);
    drop(cat);

    // Putting one back brings the real file back into the archive.
    assert_eq!(put_back_replaced(&two, &["pics/same.jpg".to_string()], &s, &p).unwrap(), 1);
    let m = read_manifest(&two).unwrap();
    assert!(m.entries.iter().any(|e| e.rel_path == "pics/same.jpg" && e.method == "lzma2"));
    assert!(!m.entries.iter().any(|e| e.rel_path == "pics/same.jpg.atticarcdep"));
    assert_eq!(m.excluded.deps.len(), 1);
    let cat = Catalog::open_existing(&out).unwrap();
    assert_eq!(cat.dedupe_tree().unwrap()[1].leaves.len(), 1);
    drop(cat);
    let (a, entries) = recorded(&out, &src);
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();
}

#[test]
fn archiving_again_applies_new_exclusions_and_moves_the_old_archive_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("Songs");
    let out = tmp.path().join("out");
    make_tree(&src);
    std::fs::write(src.join("junk.tmp"), b"scratch").unwrap();
    let p = Arc::new(Progress::default());
    let mut s = settings(&out, true);
    // First time: nothing older, so it is just an archive.
    let first = archive_again(&src, &s, &p).unwrap();
    assert!(first.superseded.is_none() && first.superseded_problem.is_none());
    let (a1, _) = recorded(&out, &src);

    // An exclusion added afterwards, and the folder archived again.
    s.exclusions = vec![Rule { pattern: "*.tmp".into(), action: Action::Discard, ..Rule::default() }];
    let again = archive_again(&src, &s, &p).unwrap();
    let moved = again.superseded.clone().expect("older archive moved");
    assert_eq!(moved, std::fs::canonicalize(&out).unwrap().join(DATA_DIR).join(SUPERSEDED_DIR).join("Songs.7z"));
    assert!(moved.is_file() && !out.join("Songs.7z").exists());
    assert!(out.join("Songs (2).7z").is_file());
    let m = read_manifest(&out.join("Songs (2).7z")).unwrap();
    assert_eq!(m.excluded.discarded.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["junk.tmp"]);

    // Every list shows the new archive only; the folder still passes its check.
    let (a2, entries) = recorded(&out, &src);
    assert_ne!(a1.archive_id, a2.archive_id);
    let cat = Catalog::open_existing(&out).unwrap();
    assert_eq!(cat.archives().unwrap().len(), 1);
    drop(cat);
    check_before_delete(&src, &a2.archive_path, &entries, 2, &p).unwrap();
    // A rebuild doesn't bring the old one back.
    assert!(rebuild_catalog(&out).is_ok());
    assert_eq!(Catalog::open_existing(&out).unwrap().archives().unwrap().len(), 1);

    // A third time: the second goes aside too, under a name of its own.
    let third = archive_again(&src, &s, &p).unwrap();
    assert!(third.superseded.unwrap().ends_with("Songs (2).7z"));
    assert!(out.join("Songs.7z").is_file(), "the plain name is free again");
}

#[test]
fn a_detailed_log_names_every_file() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let p = Arc::new(Progress::default());
    archive_folder(&src, &settings(&tmp.path().join("quiet"), true), &p).unwrap();
    assert!(p.take_events().is_empty(), "nothing kept without a detailed log");
    p.detailed.store(true, Ordering::Relaxed);
    archive_folder(&src, &settings(&tmp.path().join("out"), true), &p).unwrap();
    let ev = p.take_events();
    for f in ["notes.txt", "sub/already.zip", "sub/random.dat"] {
        assert!(ev.iter().any(|l| l.contains("compressing") && l.contains(f)), "{f} compressing: {ev:#?}");
        assert!(ev.iter().any(|l| l.contains("verifying") && l.contains(f)), "{f} verifying: {ev:#?}");
    }
    assert!(ev.iter().any(|l| l.starts_with("Compressing")) && ev.iter().any(|l| l.starts_with("Verifying")));
    assert!(p.take_events().is_empty());
}

#[test]
fn identical_files_in_a_folder_are_stored_once() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("Songs");
    let out = tmp.path().join("out");
    std::fs::create_dir_all(src.join("a")).unwrap();
    std::fs::create_dir_all(src.join("b/c")).unwrap();
    let big = "same take ".repeat(20_000);
    std::fs::write(src.join("a/take.wav"), &big).unwrap();
    std::fs::write(src.join("b/take copy.wav"), &big).unwrap();
    std::fs::write(src.join("b/c/take.wav"), &big).unwrap();
    // Same size, different bytes: not a copy.
    std::fs::write(src.join("b/other.wav"), "SAME TAKE ".repeat(20_000)).unwrap();
    // Identical but small: archived as they are.
    std::fs::write(src.join("a/small.txt"), "tiny").unwrap();
    std::fs::write(src.join("b/small.txt"), "tiny").unwrap();
    let mut s = settings(&out, true);
    s.identical_once = true;
    let p = Arc::new(Progress::default());
    let o = archive_folder(&src, &s, &p).unwrap();
    let archive = out.join("Songs.7z");
    assert!(!PathBuf::from(format!("{}.deps.partial", archive.display())).exists(), "work folder removed");

    let m = read_manifest(&archive).unwrap();
    let copies: Vec<(&str, &str, &str)> =
        m.excluded.deps.iter().map(|d| (d.entry.rel_path.as_str(), d.original_archive.as_str(), d.original_path.as_str())).collect();
    assert_eq!(copies, [("b/c/take.wav", "Songs.7z", "a/take.wav"), ("b/take copy.wav", "Songs.7z", "a/take.wav")]);
    assert_eq!(o.excluded.deps.len(), 2);
    let files: Vec<(&str, &str)> = m.entries.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e.method.as_str())).collect();
    assert_eq!(
        files,
        [
            ("a/small.txt", "lzma2"),
            ("a/take.wav", "lzma2"),
            ("b/c/take.wav.atticarcdep", "dep"),
            ("b/other.wav", "lzma2"),
            ("b/small.txt", "lzma2"),
            ("b/take copy.wav.atticarcdep", "dep"),
        ]
    );
    assert_eq!(o.record.file_count, 6);
    let dir = tmp.path().join("x");
    std::fs::create_dir_all(&dir).unwrap();
    let dep = extract_file(&archive, "b/take copy.wav.atticarcdep", &dir, 2).unwrap();
    let d: DepFile = serde_json::from_slice(&std::fs::read(dep).unwrap()).unwrap();
    assert_eq!(d.replaced.original_path, "a/take.wav");

    // The folder on disk is untouched and passes the check before a delete.
    assert_eq!(std::fs::read_to_string(src.join("b/take copy.wav")).unwrap(), big);
    let (a, entries) = recorded(&out, &src);
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

    // The first copy can't be removed as a duplicate; a copy comes back from it.
    let cat = Catalog::open_existing(&out).unwrap();
    assert!(cat.is_dep_original(&archive, "a/take.wav").unwrap());
    let rep = cat.dep_at(a.archive_id, "b/take copy.wav.atticarcdep").unwrap().unwrap();
    drop(cat);
    let got = extract_original(&out, &rep, &dir, 2).unwrap();
    assert_eq!(got.file_name().unwrap(), "take copy.wav");
    assert_eq!(std::fs::read_to_string(&got).unwrap(), big);

    // A copy changed on disk since is caught.
    std::fs::write(src.join("b/c/take.wav"), "Same take ".repeat(20_000)).unwrap();
    let e = check_before_delete(&src, &a.archive_path, &entries, 2, &p).err().unwrap();
    assert!(format!("{e:#}").contains("b/c/take.wav"), "{e:#}");
    std::fs::write(src.join("b/c/take.wav"), &big).unwrap();

    // Survives a rebuild; a copy can be put back into the archive.
    Catalog::open_existing(&out).unwrap().clear().unwrap();
    rebuild_catalog(&out).unwrap();
    assert_eq!(put_back_replaced(&archive, &["b/c/take.wav".to_string()], &s, &p).unwrap(), 1);
    let m = read_manifest(&archive).unwrap();
    assert!(m.entries.iter().any(|e| e.rel_path == "b/c/take.wav" && e.method == "lzma2"));
    assert_eq!(m.excluded.deps.len(), 1);
    let (a, entries) = recorded(&out, &src);
    check_before_delete(&src, &a.archive_path, &entries, 2, &p).unwrap();

    // Off: every copy is archived.
    let out2 = tmp.path().join("out2");
    archive_folder(&src, &settings(&out2, true), &p).unwrap();
    assert!(read_manifest(&out2.join("Songs.7z")).unwrap().excluded.deps.is_empty());
}

#[test]
fn extracted_manifest_is_not_archived_again() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    std::fs::create_dir_all(src.join(".atticarc")).unwrap();
    std::fs::write(src.join(".atticarc/manifest.json"), b"{}").unwrap();
    std::fs::create_dir_all(src.join("sub/.atticarc")).unwrap();
    let s = scan(&src).unwrap();
    assert!(!s.files.iter().any(|f| f.rel.starts_with(".atticarc")));
    assert!(s.dirs.iter().any(|d| d.1 == "sub/.atticarc"), "only the top-level one is skipped");
    let o = archive_folder(&src, &settings(&tmp.path().join("out"), true), &Arc::new(Progress::default())).unwrap();
    assert_eq!(o.record.file_count, 5);
}

#[test]
fn archive_path_is_stored_in_plain_form() {
    let (_tmp, src, out) = run(true);
    let (a, _) = recorded(&out, &src);
    assert!(!a.archive_path.to_string_lossy().starts_with(r"\\?\"), "{}", a.archive_path.display());
}

#[test]
fn archive_verify_and_catalog() {
    for store in [false, true] {
        let (_tmp, src, out) = run(store);
        let (a, entries) = recorded(&out, &src);
        assert!(a.archive_path.ends_with("My Folder.7z"));
        // The manifest is inside the archive and only archives are left
        // in the output folder.
        let m = read_manifest(&a.archive_path).unwrap();
        assert_eq!(m.entries, entries);
        assert_eq!(m.archive.file_count, 5);
        let mut names: Vec<_> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, [".atticarc", "My Folder.7z"]);
        assert!(crate::catalog::db_path(&out).is_file());
        assert!(!PathBuf::from(format!("{}.partial", a.archive_path.display())).exists());
        let zip = entries.iter().find(|e| e.rel_path == "sub/already.zip").unwrap();
        assert_eq!(zip.method, if store { "copy" } else { "lzma2" });
        let empty = entries.iter().find(|e| e.rel_path == "empty folder").unwrap();
        assert!(empty.is_dir);
        let txt = entries.iter().find(|e| e.rel_path == "notes.txt").unwrap();
        assert_eq!(txt.blake3.as_deref().unwrap(), blake3::hash(&std::fs::read(src.join("notes.txt")).unwrap()).to_hex().as_str());
        assert_eq!(txt.sha256.as_deref().unwrap(), hex(&Sha256::digest(std::fs::read(src.join("notes.txt")).unwrap())));
        // Extract with sevenz-turbo itself and compare every byte.
        let dest = out.join("x");
        ArchiveReader::open(&a.archive_path, Password::empty()).unwrap().with_threads(4).extract_to(&dest).unwrap();
        for e in entries.iter().filter(|e| !e.is_dir) {
            assert_eq!(std::fs::read(dest.join(&e.rel_path)).unwrap(), std::fs::read(src.join(&e.rel_path)).unwrap(), "{}", e.rel_path);
        }
        assert!(dest.join("empty folder").is_dir());
    }
}

#[test]
fn second_archive_gets_a_new_name() {
    let (_tmp, src, out) = run(true);
    let p = Arc::new(Progress::default());
    archive_folder(&src, &settings(&out, true), &p).unwrap();
    let n = std::fs::read_dir(&out).unwrap().flatten().filter(|d| d.path().extension().is_some_and(|x| x == "7z")).count();
    assert_eq!(n, 2);
}

#[test]
fn check_passes_then_delete() {
    let (_tmp, src, out) = run(true);
    let (a, entries) = recorded(&out, &src);
    let p = Arc::new(Progress::default());
    let c = check_before_delete(&src, &a.archive_path, &entries, 4, &p).unwrap();
    assert_eq!(c.files_checked, 5);
    delete_folder(&src).unwrap();
    assert!(!src.exists());
}

#[test]
fn check_fails_when_a_file_changed() {
    let (_tmp, src, out) = run(true);
    let (a, entries) = recorded(&out, &src);
    // Same size, different content: only the hash can catch it.
    let mut f = std::fs::OpenOptions::new().write(true).open(src.join("notes.txt")).unwrap();
    f.write_all(b"HELLO").unwrap();
    drop(f);
    let p = Arc::new(Progress::default());
    let e = check_before_delete(&src, &a.archive_path, &entries, 4, &p).err().unwrap();
    assert!(format!("{e:#}").contains("notes.txt on disk doesn't match"), "{e:#}");
}

#[test]
fn check_fails_when_a_file_was_added() {
    let (_tmp, src, out) = run(true);
    let (a, entries) = recorded(&out, &src);
    std::fs::write(src.join("sub/new.txt"), "new").unwrap();
    let p = Arc::new(Progress::default());
    let e = check_before_delete(&src, &a.archive_path, &entries, 4, &p).err().unwrap();
    assert!(format!("{e:#}").contains("sub/new.txt isn't in the archive: it is new"), "{e:#}");
}

#[test]
fn check_fails_when_the_archive_is_damaged() {
    let (_tmp, src, out) = run(false);
    let (a, entries) = recorded(&out, &src);
    let mut bytes = std::fs::read(&a.archive_path).unwrap();
    let mid = bytes.len() / 3;
    bytes[mid] ^= 0xff;
    std::fs::write(&a.archive_path, bytes).unwrap();
    let p = Arc::new(Progress::default());
    assert!(check_before_delete(&src, &a.archive_path, &entries, 4, &p).is_err());
}

#[test]
fn search_duplicates_and_extract() {
    let (_tmp, src, out) = run(true);
    // A second folder holding a copy of notes.txt.
    let other = src.parent().unwrap().join("Other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(src.join("notes.txt"), other.join("notes copy.txt")).unwrap();
    archive_folder(&other, &settings(&out, true), &Arc::new(Progress::default())).unwrap();

    let cat = Catalog::open(&out).unwrap();
    assert_eq!(cat.archives().unwrap().len(), 2);
    let hits = cat.search("caf", None, 50).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].rel_path, "sub/deeper/caf\u{e9} \u{1f600}.txt");
    let notes = cat.search("notes", None, 50).unwrap();
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().all(|f| f.copies == 2));
    assert!(cat.search("  ", None, 50).unwrap().is_empty());

    let groups = cat.duplicate_groups(10).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].copies.len(), 2);
    assert_eq!(groups[0].reclaimable(), std::fs::metadata(src.join("notes.txt")).unwrap().len());

    let a = cat.archived_folder(&src).unwrap().unwrap();
    let c = cat.contents(a.archive_id).unwrap();
    assert!(c.folders.iter().any(|d| d == "sub/deeper"), "{:?}", c.folders);
    assert!(c.files.iter().any(|f| f.rel_path == "sub/random.dat" && f.copies == 1));
    assert!(c.files.iter().any(|f| f.rel_path == "notes.txt" && f.copies == 2));
    let dest = out.join("restored");
    std::fs::create_dir_all(&dest).unwrap();
    let p = extract_file(&a.archive_path, "sub/random.dat", &dest, 2).unwrap();
    assert_eq!(std::fs::read(p).unwrap(), std::fs::read(src.join("sub/random.dat")).unwrap());
    assert!(extract_file(&a.archive_path, "sub/random.dat", &dest, 2).is_err(), "no overwrite");
    assert!(extract_file(&a.archive_path, "nope.txt", &dest, 2).is_err());
}

#[test]
fn protected_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = settings(tmp.path(), true);
    s.protected = vec![tmp.path().join("keep")];
    std::fs::create_dir_all(tmp.path().join("keep/inner")).unwrap();
    std::fs::create_dir_all(tmp.path().join("keeper")).unwrap();
    assert!(is_protected(&tmp.path().join("keep"), &s));
    assert!(is_protected(&tmp.path().join("keep/inner"), &s));
    assert!(!is_protected(&tmp.path().join("keeper"), &s));
}

#[test]
fn output_inside_protected_folder_is_refused_before_it_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let keep = tmp.path().join("keep");
    std::fs::create_dir_all(&keep).unwrap();
    let mut s = settings(&keep.join("out"), true);
    s.protected = vec![keep.clone()];
    let e = archive_folder(&src, &s, &Arc::new(Progress::default())).err().unwrap();
    assert!(format!("{e:#}").contains("protected"), "{e:#}");
    assert!(!keep.join("out").exists());
}

#[test]
fn refuses_output_inside_source() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let p = Arc::new(Progress::default());
    assert!(archive_folder(&src, &settings(&src.join("out"), true), &p).is_err());
}

#[test]
fn cancel_leaves_no_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let out = tmp.path().join("out");
    make_tree(&src);
    let p = Arc::new(Progress::default());
    p.cancel.store(true, Ordering::Relaxed);
    assert!(archive_folder(&src, &settings(&out, true), &p).is_err());
    let left: Vec<_> = std::fs::read_dir(&out).unwrap().flatten().map(|d| d.file_name()).collect();
    assert!(left.iter().all(|n| !n.to_string_lossy().contains(".7z")), "{left:?}");
}
