//! Shared by the integration tests: generated folder trees that come out
//! byte for byte the same every time, so archives made from them once (the
//! fixtures) can be checked against them for good.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use atticarc_core::archive::{Progress, Settings};
use atticarc_core::exclude::{Action, Rule};

/// `n` bytes that don't compress, the same for the same `seed`.
pub fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// A plain tree: text, noise, an empty file, an empty folder, a Unicode name.
pub fn plain_tree(root: &Path) {
    write(root, "notes.txt", "hello world\n".repeat(2_000).as_bytes());
    write(root, "sub/zero.bin", b"");
    write(root, "sub/deeper/caf\u{e9} \u{1f600}.txt", b"unicode name");
    write(root, "sub/already.zip", &noise(0x1234_5678, 20_000));
    write(root, "sub/random.dat", &noise(0x9ABC_DEF0, 9_000));
    std::fs::create_dir_all(root.join("empty folder")).unwrap();
}

/// A tree with files the rules in [`rules`] set aside (`*.tmp`) and discard (`*.lnk`).
pub fn rules_tree(root: &Path) {
    write(root, "keep.txt", "kept\n".repeat(500).as_bytes());
    write(root, "work/scratch.tmp", b"set aside");
    write(root, "work/more.tmp", &noise(7, 3_000));
    write(root, "Shortcut.lnk", b"discarded");
    write(root, "work/data.bin", &noise(11, 5_000));
}

/// A tree with identical files of 64 KiB and more (stored once), a file of
/// the same size that differs, and small identical files (kept as they are).
pub fn twice_tree(root: &Path) {
    let take = "same take ".repeat(7_000);
    write(root, "a/take.wav", take.as_bytes());
    write(root, "b/take copy.wav", take.as_bytes());
    write(root, "b/c/take.wav", take.as_bytes());
    write(root, "b/other.wav", "SAME TAKE ".repeat(7_000).as_bytes());
    write(root, "a/small.txt", b"tiny");
    write(root, "b/small.txt", b"tiny");
}

/// A tree archived twice in the fixtures: first as it is, then again with
/// `*.tmp` discarded.
pub fn again_tree(root: &Path) {
    write(root, "song.txt", "la la la\n".repeat(300).as_bytes());
    write(root, "junk.tmp", b"scratch");
}

pub fn rule(pattern: &str, action: Action) -> Rule {
    Rule { pattern: pattern.into(), action, ..Rule::default() }
}

/// `*.tmp` set aside, `*.lnk` discarded.
pub fn rules() -> Vec<Rule> {
    vec![rule("*.tmp", Action::SetAside), rule("*.lnk", Action::Discard)]
}

pub fn settings(out: &Path) -> Settings {
    Settings {
        out_dir: out.to_path_buf(),
        level: 5,
        store_mode: true,
        threads: 2,
        protected: vec![],
        exclusions: vec![],
        identical_once: false,
    }
}

pub fn progress() -> Arc<Progress> {
    Arc::new(Progress::default())
}

/// Every file under `root` as (path with `/`, bytes), sorted.
pub fn files_of(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Copies a folder and everything in it.
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (p, t) = (e.path(), to.join(e.file_name()));
        if p.is_dir() {
            copy_dir(&p, &t);
        } else {
            std::fs::copy(&p, &t).unwrap();
        }
    }
}

/// Where the checked-in fixture destination is.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("dest-2026-11")
}
