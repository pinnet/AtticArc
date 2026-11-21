//! Checks that a copy holds everything a source folder does, byte for byte: for
//! proving a copy finished (an `rclone copy`, a drag-and-drop, a restore) before
//! trusting it.
//!
//! What fails the check, because the copy is not complete:
//! - a source file missing from the copy, or something else (a folder) in its place;
//! - a file whose size or contents differ (both sides are hashed with BLAKE3);
//! - a file that couldn't be read on either side.
//!
//! What is reported but doesn't fail it, because a correct copy can have it:
//! - files only in the copy (`rclone copy` never deletes from the destination);
//! - modified times more than [`TIME_TOLERANCE`] apart;
//! - empty source folders the copy lacks (`rclone` skips them unless
//!   `--create-empty-src-dirs`);
//! - links in the source, which aren't followed or compared;
//! - the system's own folders and files on a drive ([`SYSTEM_NAMES`]: Spotlight's index, the
//!   recycle bin, the page file...), skipped on both sides unless
//!   [`Options::include_system`]. Nothing can copy them properly, and nothing needs to.
//!
//! [`Options::strict`] asks for the two folders to be identical: then files only in the
//! copy and missing empty folders fail it too.
//!
//! Paths are matched the way the system matches them, so on Windows and macOS a
//! copy that spells a name with different capitals still counts as the same file.

use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

/// Modified times closer than this count as the same: FAT keeps times to 2 seconds,
/// and some rclone remotes to 1.
pub const TIME_TOLERANCE: Duration = Duration::from_secs(2);

const BUFFER: usize = 1 << 20;

/// How often progress is reported while a big file is being read.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Where a comparison has got to, for the progress callback.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    /// Bytes read from both sides so far, counting the file being read now.
    pub bytes_read: u64,
    /// Source files looked at so far, whatever was found.
    pub files_checked: u64,
    /// The file being compared (or, at the end, the copy's folder being listed for extras).
    pub current: &'a Path,
}

/// Names the system keeps for itself on a drive, matched in any letter case at any depth.
/// macOS: Spotlight's index, file-event log, bins, versions, Finder view settings.
/// Windows: restore points and the volume's index, the recycle bins, page and hibernation
/// files. Linux: fsck's lost+found.
pub const SYSTEM_NAMES: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    ".PKInstallSandboxManager",
    ".PKInstallSandboxManager-SystemSoftware",
    ".MobileBackups",
    ".DS_Store",
    "System Volume Information",
    "$RECYCLE.BIN",
    "RECYCLER",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "DumpStack.log.tmp",
    "lost+found",
];

/// Whether `name` is one of [`SYSTEM_NAMES`].
pub fn is_system_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    SYSTEM_NAMES.iter().any(|s| s.eq_ignore_ascii_case(&name))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Compare sizes only, without reading the files: fast, but doesn't prove the contents.
    pub quick: bool,
    /// The folders must be identical: also fail on anything only in the copy, and on
    /// empty source folders the copy lacks.
    pub strict: bool,
    /// Compare the system's own folders and files ([`SYSTEM_NAMES`]) too, instead of skipping them.
    pub include_system: bool,
}

/// Which side of the comparison something is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Source,
    Copy,
}

/// Why a source file doesn't count as copied.
#[derive(Debug)]
pub enum Problem {
    Missing,
    /// The copy has a folder (or other non-file) where the source has a file.
    NotAFile,
    SizeDiffers {
        source: u64,
        copy: u64,
    },
    ContentDiffers,
    Unreadable {
        side: Side,
        error: io::Error,
    },
}

#[derive(Debug)]
pub struct Finding {
    /// Relative to both folders.
    pub path: PathBuf,
    pub problem: Problem,
}

#[derive(Debug, Default)]
pub struct Comparison {
    /// Source files found the same in the copy (by contents, or by size if quick).
    pub files_matched: u64,
    pub bytes_matched: u64,
    /// Bytes read from both sides while hashing.
    pub bytes_read: u64,
    /// Source files looked at, whatever was found.
    pub files_checked: u64,
    /// Everything that makes the copy incomplete, in path order.
    pub problems: Vec<Finding>,
    /// Files and folders only in the copy (a folder is listed, not everything in it).
    pub extra: Vec<PathBuf>,
    /// Same contents, modified times further apart than [`TIME_TOLERANCE`].
    pub times_differ: Vec<PathBuf>,
    /// Empty source folders the copy doesn't have.
    pub empty_folders_missing: Vec<PathBuf>,
    /// Links in the source, not followed or compared.
    pub links_skipped: Vec<PathBuf>,
    /// System folders and files skipped ([`SYSTEM_NAMES`]), on either side.
    pub system_skipped: Vec<PathBuf>,
    /// The progress callback asked to stop; the result covers only what was done.
    pub stopped: bool,
}

impl Comparison {
    /// Whether the copy holds everything the source does (and, if `strict`, is identical).
    pub fn is_complete(&self, strict: bool) -> bool {
        !self.stopped && self.problems.is_empty() && (!strict || (self.extra.is_empty() && self.empty_folders_missing.is_empty()))
    }
}

/// Compares `copy` against `source`. Errors only if either folder can't be opened at
/// all; anything unreadable inside them is a [`Problem`] in the result.
///
/// `progress` hears where it has got to: before each file, several times a second while a big
/// file is read, and while the copy is listed. It returns `false` to stop, even mid-file.
pub fn compare(source: &Path, copy: &Path, options: Options, mut progress: impl FnMut(&Progress) -> bool) -> io::Result<Comparison> {
    for (root, side) in [(source, "source"), (copy, "copy")] {
        let meta = fs::metadata(root).map_err(|e| io::Error::new(e.kind(), format!("the {side} {}: {e}", root.display())))?;
        if !meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the {side} {} is not a folder", root.display())));
        }
    }
    let mut result = Comparison::default();
    let mut buffers = (vec![0u8; BUFFER], vec![0u8; BUFFER]);

    // The source: every file must be in the copy.
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let entries = match read_dir_sorted(&source.join(&rel)) {
            Ok(e) => e,
            Err(error) => {
                result.problems.push(Finding { path: rel, problem: Problem::Unreadable { side: Side::Source, error } });
                continue;
            }
        };
        if entries.is_empty() && !rel.as_os_str().is_empty() && fs::symlink_metadata(copy.join(&rel)).is_err() {
            result.empty_folders_missing.push(rel.clone());
        }
        // Pushed in reverse, so folders are visited in name order.
        let mut subfolders = Vec::new();
        for (name, meta) in entries {
            let path = rel.join(&name);
            let ft = meta.file_type();
            if !options.include_system && is_system_name(&name) {
                result.system_skipped.push(path);
            } else if ft.is_symlink() {
                result.links_skipped.push(path);
            } else if ft.is_dir() {
                subfolders.push(path);
            } else if ft.is_file() && !compare_file(source, copy, path, &meta, options, &mut buffers, &mut result, &mut progress) {
                result.stopped = true;
                break;
            }
        }
        if result.stopped {
            break;
        }
        stack.extend(subfolders.into_iter().rev());
    }

    // The copy: what's there that the source doesn't have.
    if !result.stopped {
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            if !progress(&Progress { bytes_read: result.bytes_read, files_checked: result.files_checked, current: &rel }) {
                result.stopped = true;
                break;
            }
            let Ok(entries) = read_dir_sorted(&copy.join(&rel)) else { continue };
            let mut subfolders = Vec::new();
            for (name, meta) in entries {
                let path = rel.join(&name);
                if !options.include_system && is_system_name(&name) {
                    if !result.system_skipped.contains(&path) {
                        result.system_skipped.push(path);
                    }
                    continue;
                }
                // Asked of the system, so a name differing only in capitals matches where the system says it does.
                match fs::symlink_metadata(source.join(&path)) {
                    Err(_) => result.extra.push(path),
                    Ok(src) if src.is_dir() && meta.is_dir() => subfolders.push(path),
                    Ok(_) => {}
                }
            }
            stack.extend(subfolders.into_iter().rev());
        }
    }

    result.problems.sort_by(|a, b| a.path.cmp(&b.path));
    for list in [
        &mut result.extra,
        &mut result.times_differ,
        &mut result.empty_folders_missing,
        &mut result.links_skipped,
        &mut result.system_skipped,
    ] {
        list.sort();
    }
    Ok(result)
}

/// Compares one source file with the copy's. Returns `false` if progress asked to stop.
#[allow(clippy::too_many_arguments)]
fn compare_file(
    source: &Path,
    copy: &Path,
    path: PathBuf,
    src_meta: &Metadata,
    options: Options,
    buffers: &mut (Vec<u8>, Vec<u8>),
    result: &mut Comparison,
    progress: &mut impl FnMut(&Progress) -> bool,
) -> bool {
    result.files_checked += 1;
    if !progress(&Progress { bytes_read: result.bytes_read, files_checked: result.files_checked, current: &path }) {
        return false;
    }
    let problem = |problem| Finding { path: path.clone(), problem };
    let copy_meta = match fs::metadata(copy.join(&path)) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            result.problems.push(problem(Problem::Missing));
            return true;
        }
        Err(error) => {
            result.problems.push(problem(Problem::Unreadable { side: Side::Copy, error }));
            return true;
        }
    };
    if !copy_meta.is_file() {
        result.problems.push(problem(Problem::NotAFile));
        return true;
    }
    let (src_size, copy_size) = (src_meta.len(), copy_meta.len());
    if src_size != copy_size {
        result.problems.push(problem(Problem::SizeDiffers { source: src_size, copy: copy_size }));
        return true;
    }
    if !options.quick {
        // Both sides at once: a source and its copy are usually on different drives.
        // Both read on their own threads while this one reports progress, so a big file on a
        // slow network share still shows movement and can be stopped part way.
        let (a, b) = (source.join(&path), copy.join(&path));
        let (buf_a, buf_b) = (&mut buffers.0, &mut buffers.1);
        let (live, stop) = (AtomicU64::new(0), AtomicBool::new(false));
        let mut asked_to_stop = false;
        let me = std::thread::current();
        let done = AtomicUsize::new(0);
        // Each reader counts itself done and wakes this thread, so a small file costs no waiting.
        // (Not `is_finished`: a thread that has woken us may not count as finished yet.)
        let finish = || {
            done.fetch_add(1, Ordering::Release);
            me.unpark();
        };
        let (ha, hb) = std::thread::scope(|s| {
            let ta = s.spawn(|| {
                let h = hash(&a, buf_a, &live, &stop);
                finish();
                h
            });
            let tb = s.spawn(|| {
                let h = hash(&b, buf_b, &live, &stop);
                finish();
                h
            });
            while done.load(Ordering::Acquire) < 2 {
                std::thread::park_timeout(PROGRESS_EVERY);
                let read = result.bytes_read + live.load(Ordering::Relaxed);
                if !asked_to_stop && !progress(&Progress { bytes_read: read, files_checked: result.files_checked, current: &path }) {
                    asked_to_stop = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
            let failed = || Err(io::Error::other("hashing thread failed"));
            (ta.join().unwrap_or_else(|_| failed()), tb.join().unwrap_or_else(|_| failed()))
        });
        result.bytes_read += live.load(Ordering::Relaxed);
        if asked_to_stop {
            return false;
        }
        match (ha, hb) {
            (Ok(ha), Ok(hb)) => {
                if ha != hb {
                    result.problems.push(problem(Problem::ContentDiffers));
                    return true;
                }
            }
            (Err(error), _) => {
                result.problems.push(problem(Problem::Unreadable { side: Side::Source, error }));
                return true;
            }
            (_, Err(error)) => {
                result.problems.push(problem(Problem::Unreadable { side: Side::Copy, error }));
                return true;
            }
        }
    }
    result.files_matched += 1;
    result.bytes_matched += src_size;
    if let (Ok(a), Ok(b)) = (src_meta.modified(), copy_meta.modified())
        && apart(a, b) > TIME_TOLERANCE
    {
        result.times_differ.push(path);
    }
    true
}

fn apart(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b).or_else(|_| b.duration_since(a)).unwrap_or_default()
}

/// BLAKE3 of the whole file, adding what it reads to `read`; gives up when `stop` is set.
fn hash(path: &Path, buf: &mut [u8], read: &AtomicU64, stop: &AtomicBool) -> io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        let k = file.read(buf)?;
        if k == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buf[..k]);
        read.fetch_add(k as u64, Ordering::Relaxed);
    }
}

/// A folder's entries with their own metadata (links described, not followed), by name.
fn read_dir_sorted(dir: &Path) -> io::Result<Vec<(std::ffi::OsString, Metadata)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        out.push((entry.file_name(), fs::symlink_metadata(entry.path())?));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    struct Pair {
        root: PathBuf,
    }

    impl Pair {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("atticarc-compare-{name}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("copy")).unwrap();
            Pair { root }
        }
        fn src(&self) -> PathBuf {
            self.root.join("src")
        }
        fn copy(&self) -> PathBuf {
            self.root.join("copy")
        }
        /// Writes the same file on both sides.
        fn both(&self, rel: &str, data: &[u8]) {
            self.write(&self.src(), rel, data);
            self.write(&self.copy(), rel, data);
        }
        fn write(&self, root: &Path, rel: &str, data: &[u8]) {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        fn run(&self, options: Options) -> Comparison {
            compare(&self.src(), &self.copy(), options, |_: &Progress| true).unwrap()
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn kinds(c: &Comparison) -> Vec<(String, &'static str)> {
        c.problems
            .iter()
            .map(|f| {
                let k = match f.problem {
                    Problem::Missing => "missing",
                    Problem::NotAFile => "not a file",
                    Problem::SizeDiffers { .. } => "size",
                    Problem::ContentDiffers => "content",
                    Problem::Unreadable { .. } => "unreadable",
                };
                (f.path.to_string_lossy().replace('\\', "/"), k)
            })
            .collect()
    }

    #[test]
    fn an_exact_copy_is_complete() {
        let p = Pair::new("exact");
        p.both("a.txt", b"hello");
        p.both("deep/er/b.bin", &vec![7u8; 3 * BUFFER + 11]);
        p.both("empty-file", b"");
        let c = p.run(Options::default());
        assert!(c.is_complete(true), "{c:?}");
        assert_eq!(c.files_matched, 3);
        assert_eq!(c.bytes_matched, 5 + 3 * BUFFER as u64 + 11);
        assert_eq!(c.bytes_read, 2 * c.bytes_matched, "both sides were read");
    }

    #[test]
    fn missing_different_and_wrong_type_fail_it() {
        let p = Pair::new("fail");
        p.both("same.txt", b"same");
        p.write(&p.src(), "gone.txt", b"only in the source");
        p.write(&p.src(), "sub/gone-too.txt", b"x");
        p.write(&p.src(), "size.txt", b"12345");
        p.write(&p.copy(), "size.txt", b"1234");
        p.write(&p.src(), "content.txt", b"abcde");
        p.write(&p.copy(), "content.txt", b"abcdX");
        p.write(&p.src(), "was-a-file", b"file");
        fs::create_dir_all(p.copy().join("was-a-file")).unwrap();
        let c = p.run(Options::default());
        assert!(!c.is_complete(false));
        assert_eq!(
            kinds(&c),
            [
                ("content.txt".into(), "content"),
                ("gone.txt".into(), "missing"),
                ("size.txt".into(), "size"),
                ("sub/gone-too.txt".into(), "missing"),
                ("was-a-file".into(), "not a file"),
            ]
        );
        assert_eq!(c.files_matched, 1);
    }

    #[test]
    fn a_same_size_difference_is_only_found_by_reading() {
        let p = Pair::new("quick");
        p.write(&p.src(), "f", b"aaaa");
        p.write(&p.copy(), "f", b"aaab");
        assert!(p.run(Options { quick: true, ..Options::default() }).is_complete(false), "quick compares sizes only");
        let full = p.run(Options::default());
        assert_eq!(kinds(&full), [("f".into(), "content")]);
        assert_eq!(full.bytes_read, 8);
    }

    #[test]
    fn extras_are_reported_and_only_fail_when_strict() {
        let p = Pair::new("extra");
        p.both("a", b"1");
        p.write(&p.copy(), "new.txt", b"only in the copy");
        p.write(&p.copy(), "newdir/inside/x", b"1");
        fs::create_dir_all(p.copy().join("emptyextra")).unwrap();
        let c = p.run(Options::default());
        let extra: Vec<String> = c.extra.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert_eq!(extra, ["emptyextra", "new.txt", "newdir"], "a folder is listed once, not its contents");
        assert!(c.is_complete(false));
        assert!(!c.is_complete(true));
    }

    #[test]
    fn empty_source_folders_are_reported_not_failed() {
        let p = Pair::new("empty");
        p.both("a", b"1");
        fs::create_dir_all(p.src().join("empty/nested-empty")).unwrap();
        fs::create_dir_all(p.src().join("kept")).unwrap();
        fs::create_dir_all(p.copy().join("kept")).unwrap();
        let c = p.run(Options::default());
        assert!(c.is_complete(false), "{c:?}");
        assert!(!c.is_complete(true), "identical means the empty folder too");
        assert_eq!(c.empty_folders_missing, [PathBuf::from("empty").join("nested-empty")]);
    }

    #[test]
    fn times_further_apart_than_the_tolerance_are_reported() {
        let p = Pair::new("times");
        p.both("close", b"1");
        p.both("far", b"1");
        let set = |rel: &str, secs: u64| {
            let f = File::options().write(true).open(p.copy().join(rel)).unwrap();
            f.set_modified(fs::metadata(p.src().join(rel)).unwrap().modified().unwrap() + Duration::from_secs(secs)).unwrap();
        };
        set("close", 1);
        set("far", 60);
        let c = p.run(Options::default());
        assert!(c.is_complete(true));
        assert_eq!(c.times_differ, [PathBuf::from("far")]);
    }

    #[test]
    fn stopping_part_way_never_counts_as_complete() {
        let p = Pair::new("stop");
        for i in 0..5 {
            p.both(&format!("f{i}"), b"data");
        }
        // Stops when told, before the third file.
        let c = compare(&p.src(), &p.copy(), Options::default(), |p: &Progress| p.files_checked < 3).unwrap();
        assert!(c.stopped);
        assert!(!c.is_complete(false));
        assert_eq!(c.files_matched, 2);
    }

    #[test]
    fn progress_never_goes_backwards_and_stopping_is_not_a_problem() {
        let p = Pair::new("bigfile");
        let big = vec![3u8; 64 * BUFFER]; // 64 MiB each side
        p.both("big.bin", &big);
        // A slow reader: each report waits, so several land while the file is read.
        let mut seen = Vec::new();
        let c = compare(&p.src(), &p.copy(), Options::default(), |pr: &Progress| {
            seen.push(pr.bytes_read);
            std::thread::sleep(Duration::from_millis(1));
            true
        })
        .unwrap();
        assert!(c.is_complete(true));
        assert_eq!(c.bytes_read, 2 * big.len() as u64);
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "never goes backwards");

        // Asked to stop at the first report: not counted as matched, and not a problem.
        let c = compare(&p.src(), &p.copy(), Options::default(), |_: &Progress| false).unwrap();
        assert!(c.stopped);
        assert_eq!(c.files_matched, 0);
        assert!(c.problems.is_empty(), "stopping isn't a problem with the file");
    }

    #[test]
    fn thousands_of_small_files_are_not_slowed_by_progress() {
        let p = Pair::new("many");
        for i in 0..2000 {
            p.both(&format!("d{}/f{i}", i % 20), b"tiny");
        }
        let t = std::time::Instant::now();
        let c = p.run(Options::default());
        assert_eq!(c.files_matched, 2000);
        assert!(t.elapsed() < Duration::from_secs(20), "took {:?}", t.elapsed());
    }

    #[test]
    fn folders_that_are_not_there_are_errors() {
        let p = Pair::new("roots");
        let e = compare(&p.root.join("nope"), &p.copy(), Options::default(), |_: &Progress| true).unwrap_err();
        assert!(e.to_string().starts_with("the source ") && e.to_string().contains("nope"), "{e}");
        p.write(&p.root, "afile", b"x");
        let e = compare(&p.src(), &p.root.join("afile"), Options::default(), |_: &Progress| true).unwrap_err();
        assert!(e.to_string().contains("is not a folder"), "{e}");
    }

    #[test]
    fn an_empty_source_matches_anything_but_strict() {
        let p = Pair::new("emptysrc");
        p.write(&p.copy(), "x", b"1");
        let c = p.run(Options::default());
        assert!(c.is_complete(false));
        assert!(!c.is_complete(true));
    }

    #[test]
    fn system_folders_are_skipped_on_both_sides() {
        let p = Pair::new("system");
        p.both("song.wav", b"music");
        // In the source: Spotlight's index, unreadable, and Finder files that differ.
        p.write(&p.src(), ".Spotlight-V100/Store-V2/index", b"index");
        p.write(&p.src(), "sub/.DS_Store", b"view settings A");
        p.write(&p.copy(), "sub/.DS_Store", b"view settings B, longer");
        p.write(&p.src(), "$Recycle.Bin/S-1-5/x", b"bin");
        // In the copy only: its own system folders are not extras.
        p.write(&p.copy(), "System Volume Information/tracking.log", b"x");
        p.write(&p.copy(), ".fseventsd/0001", b"x");
        let c = p.run(Options { strict: true, ..Options::default() });
        assert!(c.is_complete(true), "{c:?}");
        let skipped: Vec<String> = c.system_skipped.iter().map(|x| x.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(skipped, ["$Recycle.Bin", ".Spotlight-V100", ".fseventsd", "System Volume Information", "sub/.DS_Store"]);

        let all = p.run(Options { include_system: true, ..Options::default() });
        assert!(!all.is_complete(false), "with include_system they're compared like anything else");
        assert!(all.system_skipped.is_empty());
    }

    #[test]
    fn system_names_match_in_any_case() {
        for n in [".spotlight-v100", "$recycle.bin", "SYSTEM VOLUME INFORMATION", ".ds_store", "Lost+Found"] {
            assert!(is_system_name(std::ffi::OsStr::new(n)), "{n}");
        }
        for n in ["Spotlight-V100", "recycle", "my .DS_Store notes", "system volume"] {
            assert!(!is_system_name(std::ffi::OsStr::new(n)), "{n}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_are_skipped_and_listed_not_followed() {
        let p = Pair::new("links");
        p.both("real", b"1");
        std::os::unix::fs::symlink("/nonexistent/target", p.src().join("link")).unwrap();
        let c = p.run(Options::default());
        assert!(c.is_complete(false));
        assert_eq!(c.links_skipped, [PathBuf::from("link")]);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_source_file_fails_it() {
        use std::os::unix::fs::PermissionsExt;
        let p = Pair::new("unreadable");
        p.both("secret", b"1");
        fs::set_permissions(p.src().join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(p.src().join("secret")).is_ok() {
            return; // running as root: can't make a file unreadable
        }
        let c = p.run(Options::default());
        assert_eq!(kinds(&c), [("secret".into(), "unreadable")]);
        fs::set_permissions(p.src().join("secret"), fs::Permissions::from_mode(0o644)).unwrap();
    }
}
