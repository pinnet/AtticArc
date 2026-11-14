//! Archives made on 14 Nov 2026 (0.9.0 plus fast clean-up, stored once and
//! Archive again), checked in under `tests/fixtures/dest-2026-11`, must keep
//! working for good: this opens, lists, verifies, checks, scrubs, extracts
//! and rebuilds them with whatever the code is now.
//!
//! The fixture is a destination folder as AtticARC left it: four archives,
//! one superseded archive and the catalogue. The folders they were made from
//! are generated again by `common` (byte for byte the same), so the check
//! before a delete can run against them.
//!
//! To make the fixture again (only ever on purpose, with a new folder name):
//! `cargo test -p atticarc-core --test fixtures -- --ignored make_the_fixture`.

mod common;

use std::path::Path;

use atticarc_core::archive::{
    self, DepFile, Manifest, archive_again, archive_folder, check_before_delete, check_index, read_manifest, rebuild_catalog,
    scrub_archive, verify_archive,
};
use atticarc_core::catalog::{Catalog, DATA_DIR, EntryRecord};
use atticarc_core::exclude::Action;
use common::*;

/// The folders the fixture was made from, generated again under `root`.
fn sources(root: &Path) {
    plain_tree(&root.join("Plain"));
    rules_tree(&root.join("Rules"));
    twice_tree(&root.join("Twice"));
    again_tree(&root.join("Again"));
}

#[test]
#[ignore = "writes the checked-in fixture; run by hand, once"]
fn make_the_fixture() {
    let dest = fixture_dir();
    assert!(!dest.exists(), "{} is already there; fixtures are never remade in place", dest.display());
    let tmp = tempfile::tempdir().unwrap();
    sources(tmp.path());
    let p = progress();
    let mut s = settings(&dest);
    archive_folder(&tmp.path().join("Plain"), &s, &p).unwrap();
    s.exclusions = rules();
    archive_folder(&tmp.path().join("Rules"), &s, &p).unwrap();
    s.exclusions = vec![];
    s.identical_once = true;
    archive_folder(&tmp.path().join("Twice"), &s, &p).unwrap();
    s.identical_once = false;
    archive_folder(&tmp.path().join("Again"), &s, &p).unwrap();
    s.exclusions = vec![rule("*.tmp", Action::Discard)];
    let again = archive_again(&tmp.path().join("Again"), &s, &p).unwrap();
    assert!(again.superseded.is_some());
}

/// The fixture copied to a temp folder, with its sources generated beside it.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    copy_dir(&fixture_dir(), &out);
    sources(tmp.path());
    (tmp, out)
}

#[test]
fn the_fixture_catalogue_still_opens() {
    let (_tmp, out) = fixture();
    // The catalogue as written then: any schema change since must migrate it.
    let cat = Catalog::open_existing(&out).unwrap();
    let mut names: Vec<String> = cat.archives().unwrap().into_iter().map(|a| a.name).collect();
    names.sort();
    assert_eq!(names, ["Again (2)", "Plain", "Rules", "Twice"], "the superseded archive is not listed");
    for a in cat.archives().unwrap() {
        assert_eq!(cat.entries(a.id).unwrap().iter().filter(|e| !e.is_dir).count() as u64, a.file_count);
    }
    assert_eq!(cat.excluded_folders().unwrap().len(), 2, "Rules and Again (2) left files out");
    assert_eq!(cat.search("take", None, 10).unwrap().len(), 3);
    assert_eq!(cat.cleanup_checks().unwrap().len() + cat.scrub_list().unwrap().len(), 4, "no checks saved, four archives to scrub");
}

#[test]
fn the_fixture_archives_still_verify_check_scrub_and_rebuild() {
    let (tmp, out) = fixture();
    let p = progress();
    // As if carried to another machine: no catalogue, only the archives.
    std::fs::remove_file(out.join(DATA_DIR).join("catalog.db")).unwrap();
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.failed.len(), r.copies.len()), (4, 0, 0), "{r:?}");

    let expect: [(&str, &str, usize); 4] =
        [("Plain", "Plain.7z", 5), ("Rules", "Rules.7z", 2), ("Twice", "Twice.7z", 6), ("Again", "Again (2).7z", 1)];
    for (folder, file, files) in expect {
        let archive = out.join(file);
        let m: Manifest = read_manifest(&archive).unwrap();
        assert_eq!(m.entries.iter().filter(|e| !e.is_dir).count(), files, "{file}");
        // The header lists every file; every file decodes to its recorded hash.
        check_index(&archive, &m.entries).unwrap();
        // (The archive of set-aside files inside counts as one more entry to find.)
        let all: Vec<EntryRecord> = m.entries.iter().cloned().chain(m.excluded.set_aside_inside.clone()).collect();
        verify_archive(&archive, &all, 2, &p).unwrap();
        scrub_archive(&archive, &m.entries, 2, &p).unwrap();
        // The folder it was made from still passes the check before a delete.
        let check = check_before_delete(&tmp.path().join(folder), &archive, &m.entries, 2, &p).unwrap();
        assert_eq!(check.set_aside_inside, folder == "Rules", "{file}");
    }

    // What each kind of archive records.
    let rules = read_manifest(&out.join("Rules.7z")).unwrap().excluded;
    assert_eq!(rules.set_aside.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["work/more.tmp", "work/scratch.tmp"]);
    assert_eq!(rules.discarded.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["Shortcut.lnk"]);
    assert!(rules.set_aside_inside.is_some() && rules.set_aside_archive.is_none());
    let twice = read_manifest(&out.join("Twice.7z")).unwrap().excluded;
    let deps: Vec<(&str, &str, &str)> =
        twice.deps.iter().map(|d| (d.entry.rel_path.as_str(), d.original_archive.as_str(), d.original_path.as_str())).collect();
    assert_eq!(deps, [("b/c/take.wav", "Twice.7z", "a/take.wav"), ("b/take copy.wav", "Twice.7z", "a/take.wav")]);
    let again = read_manifest(&out.join("Again (2).7z")).unwrap().excluded;
    assert_eq!(again.discarded.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(), ["junk.tmp"]);

    // Files come back byte for byte; a reference file leads to its original.
    let dir = tmp.path().join("x");
    std::fs::create_dir_all(&dir).unwrap();
    let got = archive::extract_file(&out.join("Plain.7z"), "sub/already.zip", &dir, 2).unwrap();
    assert_eq!(std::fs::read(got).unwrap(), noise(0x1234_5678, 20_000));
    let dep = archive::extract_file(&out.join("Twice.7z"), "b/take copy.wav.atticarcdep", &dir, 2).unwrap();
    let d: DepFile = serde_json::from_slice(&std::fs::read(dep).unwrap()).unwrap();
    assert_eq!(d.atticarc_dep, 1);
    let original = archive::extract_original(&out, &d.replaced, &dir, 2).unwrap();
    assert_eq!(std::fs::read_to_string(original).unwrap(), "same take ".repeat(7_000));

    // The superseded archive was only moved: it is intact, and holds the file discarded later.
    let old = out.join(DATA_DIR).join(archive::SUPERSEDED_DIR).join("Again.7z");
    let m = read_manifest(&old).unwrap();
    verify_archive(&old, &m.entries, 2, &p).unwrap();
    assert!(m.entries.iter().any(|e| e.rel_path == "junk.tmp"));
}

/// A destination carried to another machine or system: the catalogue still
/// names the archives where they were, so none is found; they are relinked
/// by what they hold, not added twice, and work from then on.
#[test]
fn a_destination_carried_elsewhere_is_relinked_by_what_it_holds() {
    let (tmp, out) = fixture();
    // The fixture's catalogue names the folder it was made in, which is gone
    // on any machine but the one that made it; make sure of that here too.
    let mut cat = Catalog::open_existing(&out).unwrap();
    for a in cat.archives().unwrap() {
        let name = Path::new(&a.archive_path).file_name().unwrap().to_owned();
        cat.relink(a.id, &tmp.path().join("another machine").join(name).to_string_lossy()).unwrap();
    }
    assert!(cat.archives().unwrap().iter().all(|a| !Path::new(&a.archive_path).is_file()));
    drop(cat);
    let r = archive::relink_missing(&out).unwrap();
    assert_eq!((r.relinked.len(), r.missing.len()), (4, 0), "{r:?}");
    let cat = Catalog::open_existing(&out).unwrap();
    let archives = cat.archives().unwrap();
    assert_eq!(archives.len(), 4);
    let p = progress();
    for a in &archives {
        assert!(Path::new(&a.archive_path).is_file(), "{}", a.archive_path);
        check_index(Path::new(&a.archive_path), &cat.entries(a.id).unwrap()).unwrap();
    }
    // The folders they were made from are somewhere else too: none is a candidate for clean up, none is lost.
    let (found, missing) = atticarc_core::cleanup::candidates(&out, &Default::default(), &[]).unwrap();
    assert_eq!((found.len(), missing.len()), (0, 4));
    // A rebuild finds nothing new.
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.already_there), (0, 4), "{r:?}");
    let plain = archives.iter().find(|a| a.name == "Plain").unwrap();
    let m = read_manifest(Path::new(&plain.archive_path)).unwrap();
    check_before_delete(&tmp.path().join("Plain"), Path::new(&plain.archive_path), &m.entries, 2, &p).unwrap();
}

/// The same folder archived now gives the same record as it did then, and a
/// manifest with the same fields: nothing added, dropped or renamed. (Times,
/// paths and the engine's name are the machine's and the day's, not the format's.)
#[test]
fn archiving_the_same_folders_now_gives_the_same_manifests() {
    let (tmp, old_out) = fixture();
    let new_out = tmp.path().join("new");
    let p = progress();
    let mut s = settings(&new_out);
    archive_folder(&tmp.path().join("Plain"), &s, &p).unwrap();
    s.exclusions = rules();
    archive_folder(&tmp.path().join("Rules"), &s, &p).unwrap();
    s.exclusions = vec![];
    s.identical_once = true;
    archive_folder(&tmp.path().join("Twice"), &s, &p).unwrap();

    for file in ["Plain.7z", "Rules.7z", "Twice.7z"] {
        let (then, now) = (raw_manifest(&old_out.join(file), tmp.path()), raw_manifest(&new_out.join(file), tmp.path()));
        assert_eq!(shape(&then), shape(&now), "{file}: the manifest's fields changed");
        assert_eq!(timeless(then), timeless(now), "{file}: the manifest's contents changed");
    }
}

fn raw_manifest(archive: &Path, work: &Path) -> serde_json::Value {
    let dir = tempfile::tempdir_in(work).unwrap();
    let at = archive::extract_file(archive, archive::MANIFEST_ENTRY, dir.path(), 2).unwrap();
    serde_json::from_slice(&std::fs::read(at).unwrap()).unwrap()
}

/// The manifest's structure: every key path, with values dropped.
fn shape(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(shape).collect()),
        // A value is a value, set or not: whether a drive records a file's
        // created time is the drive's business, not the format's.
        _ => Value::Bool(true),
    }
}

/// The manifest without what depends on the day or the machine.
fn timeless(mut v: serde_json::Value) -> serde_json::Value {
    fn strip(v: &mut serde_json::Value) {
        use serde_json::Value;
        match v {
            Value::Object(m) => {
                for k in ["created_utc", "modified_utc", "replaced_utc", "source_path", "source_display", "note"] {
                    if m.contains_key(k) {
                        m.insert(k.into(), Value::Null);
                    }
                }
                // The archive of set-aside files holds times too, so its size and hashes move with them.
                if let Some(inner) = m.get_mut("set_aside_inside").filter(|i| i.is_object()) {
                    for k in ["size", "blake3", "sha256"] {
                        inner[k] = Value::Null;
                    }
                }
                // A reference file holds the times of the file it stands for and of its
                // making, so its size and hashes move too, and the folder's total with them.
                if m.get("method").and_then(Value::as_str) == Some("dep") {
                    for k in ["size", "blake3", "sha256"] {
                        m.insert(k.into(), Value::Null);
                    }
                }
                if m.contains_key("input_bytes") {
                    m.insert("input_bytes".into(), Value::Null);
                }
                m.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut v);
    v
}
