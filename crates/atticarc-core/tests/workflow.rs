//! The whole workflow on generated folders, end to end, through the public
//! API only: archive → duplicates → excluded → clean up (check, restart,
//! delete) → rebuild the catalogue → compare. docs/behaviour.md names the
//! rules this holds; a refactor must leave it passing unchanged.

mod common;

use std::collections::BTreeMap;

use atticarc_core::archive::{self, archive_folder, read_manifest, rebuild_catalog};
use atticarc_core::catalog::{self, Catalog};
use atticarc_core::cleanup;
use common::*;

#[test]
fn archive_duplicates_excluded_cleanup_rebuild_compare() {
    let tmp = tempfile::tempdir().unwrap();
    let (photos, copy, out) = (tmp.path().join("Photos"), tmp.path().join("Photos copy"), tmp.path().join("out"));
    plain_tree(&photos);
    rules_tree(&photos);
    // A second folder sharing one file with the first.
    rules_tree(&copy);
    std::fs::write(copy.join("only here.txt"), b"unique to the copy").unwrap();
    let before_photos = files_of(&photos);
    let mut s = settings(&out);
    s.exclusions = rules();
    let p = progress();

    // Nothing exists until something is archived, and reading creates nothing.
    assert!(!catalog::exists(&out));
    assert!(cleanup::candidates(&out, &BTreeMap::new(), &[]).unwrap().0.is_empty());
    assert!(!out.exists());

    // Archive: one .7z per folder, verified, catalogued, no partial files left.
    let a = archive_folder(&photos, &s, &p).unwrap();
    let b = archive_folder(&copy, &s, &p).unwrap();
    assert!(out.join("Photos.7z").is_file() && out.join("Photos copy.7z").is_file());
    let left: Vec<_> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(left.iter().all(|n| !n.contains("partial")), "{left:?}");
    assert_eq!(a.excluded.set_aside.len(), 2);
    assert_eq!(a.excluded.discarded.len(), 1);
    assert_eq!(b.excluded.discarded.len(), 1);
    // The source folders are untouched.
    assert_eq!(files_of(&photos), before_photos);

    let cat = Catalog::open_existing(&out).unwrap();
    let archives = cat.archives().unwrap();
    assert_eq!(archives.len(), 2);
    // Search finds a file by a word of its name.
    let found = cat.search("random", None, 10).unwrap();
    assert_eq!(found.iter().map(|f| f.rel_path.as_str()).collect::<Vec<_>>(), ["sub/random.dat"]);

    // Duplicates: the files both folders hold, found by hash, across archives.
    let groups = cat.duplicate_groups(100).unwrap();
    let dup: Vec<&str> = groups.iter().map(|g| g.copies[0].rel_path.as_str()).collect();
    assert!(dup.contains(&"keep.txt") && dup.contains(&"work/data.bin"), "{dup:?}");
    assert!(groups.iter().all(|g| g.copies.len() == 2 && g.reclaimable() == g.size));
    assert!(!dup.contains(&"only here.txt"));

    // Excluded: what the rules left out of each folder, set aside inside the archive or discarded.
    let ex = cat.excluded_folders().unwrap();
    assert_eq!(ex.len(), 2);
    for f in &ex {
        assert!(f.aside_inside && f.aside_archive.is_none());
        assert_eq!(f.set_aside.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["work/more.tmp", "work/scratch.tmp"]);
        assert_eq!(f.discarded.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["Shortcut.lnk"]);
    }
    drop(cat);

    // Clean up: both folders are candidates, nothing ticked to start.
    let (mut items, missing) = cleanup::candidates(&out, &BTreeMap::new(), &[]).unwrap();
    assert!(missing.is_empty());
    assert_eq!(items.len(), 2);
    assert!(cleanup::ticked(&out).unwrap().is_empty());
    // An unchecked folder is never deleted. (On NTFS the check was saved at
    // archive time; forget it, so the full check runs on every system.)
    for c in &items {
        Catalog::open_existing(&out).unwrap().forget_cleanup_check(c.archive_id).unwrap();
    }
    (items, _) = cleanup::candidates(&out, &BTreeMap::new(), &[]).unwrap();
    assert!(items.iter().all(|c| !c.passed()));
    assert!(cleanup::delete(&items[0], &s, &p).is_err());
    assert!(photos.is_dir());

    // Check both, tick one. Deletes nothing.
    for c in &items {
        let k = cleanup::check(c, &s, &p);
        assert!(k.passed, "{}", k.detail);
        assert_eq!(k.discarded, 1);
    }
    let photos_id = items.iter().find(|c| c.source.ends_with("Photos")).unwrap().archive_id;
    cleanup::set_ticked(&out, &[photos_id]).unwrap();
    assert!(photos.is_dir() && copy.is_dir());

    // "Restart": everything is read again from the catalogue; checks and ticks are still there.
    let (items, _) = cleanup::candidates(&out, &BTreeMap::new(), &[]).unwrap();
    assert!(items.iter().all(|c| c.passed()));
    assert_eq!(cleanup::ticked(&out).unwrap(), [photos_id]);

    // A folder changed after its check is left alone; checked again, it fails.
    std::fs::write(copy.join("late.txt"), b"added after the check").unwrap();
    let c_copy = items.iter().find(|c| c.source.ends_with("Photos copy")).unwrap();
    assert!(cleanup::delete(c_copy, &s, &p).is_err());
    assert!(copy.is_dir());
    assert!(!cleanup::check(c_copy, &s, &p).passed);

    // Delete the one that passed: the folder goes, the archive stays, the tick is dropped.
    let c_photos = items.iter().find(|c| c.archive_id == photos_id).unwrap();
    cleanup::delete(c_photos, &s, &p).unwrap();
    assert!(!photos.exists());
    assert!(out.join("Photos.7z").is_file());
    let (items, missing) = cleanup::candidates(&out, &BTreeMap::new(), &[]).unwrap();
    assert_eq!((items.len(), missing.len()), (1, 0));
    assert!(cleanup::ticked(&out).unwrap().is_empty());

    // Rebuild: the catalogue is thrown away and comes back from the archives' manifests.
    let record = |out: &std::path::Path| {
        let cat = Catalog::open_existing(out).unwrap();
        let mut v: Vec<_> = cat
            .archives()
            .unwrap()
            .into_iter()
            .map(|a| (a.name.clone(), a.file_count, a.input_bytes, a.archive_bytes, a.created_utc.clone(), cat.entries(a.id).unwrap()))
            .collect();
        v.sort_by(|x, y| x.0.cmp(&y.0));
        v
    };
    let before = record(&out);
    Catalog::open_existing(&out).unwrap().clear().unwrap();
    assert!(Catalog::open_existing(&out).unwrap().archives().unwrap().is_empty());
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.failed.len()), (2, 0));
    assert_eq!(record(&out), before);
    assert_eq!(Catalog::open_existing(&out).unwrap().excluded_folders().unwrap().len(), 2);
    // A second rebuild adds nothing.
    let r = rebuild_catalog(&out).unwrap();
    assert_eq!((r.added, r.already_there), (0, 2));

    // Compare: every file of the deleted folder comes back from its archive, byte for byte.
    let back = tmp.path().join("back");
    let arch = out.join("Photos.7z");
    let m = read_manifest(&arch).unwrap();
    for e in m.entries.iter().filter(|e| !e.is_dir) {
        let dir = back.join(std::path::Path::new(&e.rel_path).parent().unwrap_or(std::path::Path::new("")));
        std::fs::create_dir_all(&dir).unwrap();
        archive::extract_file(&arch, &e.rel_path, &dir, 2).unwrap();
    }
    let kept: Vec<_> = before_photos.into_iter().filter(|(rel, _)| !rel.ends_with(".tmp") && !rel.ends_with(".lnk")).collect();
    assert_eq!(files_of(&back), kept);
}

/// Links are never followed or archived, and a folder holding one can't pass
/// the check before a delete (the archive doesn't hold what the link points to).
#[cfg(unix)]
#[test]
fn links_are_listed_not_archived_and_block_a_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, out, elsewhere) = (tmp.path().join("Linked"), tmp.path().join("out"), tmp.path().join("elsewhere"));
    plain_tree(&src);
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("outside.txt"), b"not part of the folder").unwrap();
    std::os::unix::fs::symlink(&elsewhere, src.join("link")).unwrap();
    let (s, p) = (settings(&out), progress());
    let o = archive_folder(&src, &s, &p).unwrap();
    assert_eq!(o.skipped_links, ["link"]);
    let m = read_manifest(&out.join("Linked.7z")).unwrap();
    assert!(m.entries.iter().all(|e| !e.rel_path.starts_with("link")));
    let e = archive::check_before_delete(&src, &out.join("Linked.7z"), &m.entries, 2, &p).err().unwrap();
    assert!(format!("{e:#}").contains("link"), "{e:#}");
}

/// A destination without room is refused before anything is written there.
#[test]
fn a_folder_that_does_not_fit_is_refused_before_anything_is_written() {
    use atticarc_core::disk;
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let e = disk::ensure_free(&out, u64::MAX / 2, "archiving Big").err().unwrap();
    assert!(format!("{e:#}").contains("Big"), "{e:#}");
    assert!(!out.exists());
    // The worst case allowed for is the folder's size plus a little, never less.
    assert!(disk::archive_worst_case(1_000_000) > 1_000_000);
}
