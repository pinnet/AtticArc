//! Fast metadata scan: lists files without reading their contents.
//!
//! Hashing means reading every byte, which is slow on USB disks and network shares.
//! A file can only be a duplicate of another file of exactly the same size, so a
//! metadata scan first lets us hash only the files that share a size with another.
//! [`hash_candidates`] then confirms those, reading only a 64 KiB prefix of most
//! files that turn out to differ.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One file found by the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the scan root.
    pub rel_path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Something the scan couldn't read. Recorded rather than aborting the whole scan.
#[derive(Debug)]
pub struct ScanError {
    pub path: PathBuf,
    pub error: io::Error,
}

/// Result of scanning one root folder.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<FileEntry>,
    pub errors: Vec<ScanError>,
}

impl ScanResult {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Groups of files sharing a size: the only possible duplicates.
    /// Empty files are skipped (all empty files are trivially identical).
    /// Groups are sorted by wasted space (size × extra copies), largest first.
    pub fn size_collisions(&self) -> Vec<SizeGroup<'_>> {
        let mut by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
        for f in self.files.iter().filter(|f| f.size > 0) {
            by_size.entry(f.size).or_default().push(f);
        }
        let mut groups: Vec<SizeGroup<'_>> = by_size
            .into_iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(size, files)| SizeGroup { size, files })
            .collect();
        groups.sort_by(|a, b| b.potential_waste().cmp(&a.potential_waste()).then(a.size.cmp(&b.size)));
        groups
    }
}

/// Files that share one size. Not yet confirmed duplicates; that needs hashing.
#[derive(Debug)]
pub struct SizeGroup<'a> {
    pub size: u64,
    pub files: Vec<&'a FileEntry>,
}

impl SizeGroup<'_> {
    /// Space that would be freed if every file in the group turned out identical.
    pub fn potential_waste(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

/// How much of each file the first pass reads. Most same-size files that
/// differ already differ here, so the full read is only needed for the rest.
pub const PREFIX_BYTES: u64 = 64 * 1024;

/// Files confirmed identical: same size and same BLAKE3 of the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub size: u64,
    /// BLAKE3 of the whole file, as hex.
    pub blake3: String,
    /// Paths relative to the scan root, sorted.
    pub files: Vec<PathBuf>,
}

impl DuplicateGroup {
    /// Space freed by keeping one copy.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

/// What [`hash_candidates`] found.
#[derive(Debug, Default)]
pub struct HashReport {
    /// Confirmed duplicates, most wasted space first.
    pub groups: Vec<DuplicateGroup>,
    /// Bytes actually read, prefixes included.
    pub bytes_read: u64,
    /// Files that couldn't be read, or changed size while being read. They
    /// are left out of the groups rather than stopping the run.
    pub errors: Vec<ScanError>,
}

impl HashReport {
    pub fn wasted(&self) -> u64 {
        self.groups.iter().map(DuplicateGroup::wasted).sum()
    }
}

/// Confirms which size collisions from [`ScanResult::size_collisions`] are
/// really duplicates. Files larger than [`PREFIX_BYTES`] are first compared
/// by a hash of their first 64 KiB, and only files still matching are read
/// in full. `progress` is called with the bytes read so far after each
/// chunk; it may return `false` to stop early, in which case the report
/// holds the groups finished so far.
pub fn hash_candidates(root: &Path, groups: &[SizeGroup<'_>], mut progress: impl FnMut(u64) -> bool) -> HashReport {
    let mut report = HashReport::default();
    let mut buf = vec![0u8; 1 << 20];
    'groups: for g in groups {
        let mut alive: Vec<&FileEntry> = g.files.clone();
        if g.size > PREFIX_BYTES {
            let mut by_prefix: HashMap<blake3::Hash, Vec<&FileEntry>> = HashMap::new();
            for f in alive {
                match hash_file(&root.join(&f.rel_path), Some(PREFIX_BYTES), g.size, &mut buf, &mut report.bytes_read, &mut progress) {
                    Ok(Some(h)) => by_prefix.entry(h).or_default().push(f),
                    Ok(None) => break 'groups,
                    Err(error) => report.errors.push(ScanError { path: f.rel_path.clone(), error }),
                }
            }
            alive = by_prefix.into_values().filter(|v| v.len() > 1).flatten().collect();
        }
        if alive.len() < 2 {
            continue;
        }
        let mut by_hash: HashMap<blake3::Hash, Vec<PathBuf>> = HashMap::new();
        for f in alive {
            match hash_file(&root.join(&f.rel_path), None, g.size, &mut buf, &mut report.bytes_read, &mut progress) {
                Ok(Some(h)) => by_hash.entry(h).or_default().push(f.rel_path.clone()),
                Ok(None) => break 'groups,
                Err(error) => report.errors.push(ScanError { path: f.rel_path.clone(), error }),
            }
        }
        for (h, mut files) in by_hash {
            if files.len() > 1 {
                files.sort();
                report.groups.push(DuplicateGroup { size: g.size, blake3: h.to_hex().to_string(), files });
            }
        }
    }
    report.groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.size.cmp(&b.size)).then(a.files.cmp(&b.files)));
    report
}

/// BLAKE3 of the first `limit` bytes of `path` (all of it if `None`).
/// `Ok(None)` means `progress` asked to stop.
fn hash_file(
    path: &Path,
    limit: Option<u64>,
    expected_size: u64,
    buf: &mut [u8],
    bytes_read: &mut u64,
    progress: &mut impl FnMut(u64) -> bool,
) -> io::Result<Option<blake3::Hash>> {
    let file = File::open(path)?;
    let want = limit.map_or(expected_size, |l| l.min(expected_size));
    // One byte past what we want, so a file that grew is noticed.
    let mut reader = file.take(want + 1);
    let mut hasher = blake3::Hasher::new();
    let mut n = 0u64;
    loop {
        let k = reader.read(buf)?;
        if k == 0 {
            break;
        }
        let k_used = (k as u64).min(want - n.min(want)) as usize;
        hasher.update(&buf[..k_used]);
        n += k as u64;
        *bytes_read += k as u64;
        if !progress(*bytes_read) {
            return Ok(None);
        }
        if n > want {
            break;
        }
    }
    let changed = if limit.is_none() { n != expected_size } else { n < want };
    if changed {
        return Err(io::Error::other("changed size since the scan"));
    }
    Ok(Some(hasher.finalize()))
}

/// Recursively scan `root`. Symlinks and junctions are not followed, so a link
/// pointing back up the tree can't cause an endless loop or double counting.
pub fn scan(root: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(error) => {
                result.errors.push(ScanError { path: dir, error });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(error) => {
                    result.errors.push(ScanError { path: dir.clone(), error });
                    continue;
                }
            };
            let path = entry.path();
            // symlink_metadata: describes the link itself, never its target.
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(error) => {
                    result.errors.push(ScanError { path, error });
                    continue;
                }
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            } else if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let rel_path = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                result.files.push(FileEntry { rel_path, size: meta.len(), modified: meta.modified().ok() });
            }
        }
    }
    result.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("atticarc-test-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_nested_files_with_sizes() {
        let root = temp_dir("nested");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("top.txt"), b"hello").unwrap();
        fs::write(root.join("a/b/deep.bin"), vec![0u8; 1000]).unwrap();

        let r = scan(&root);
        assert!(r.errors.is_empty());
        assert_eq!(r.files.len(), 2);
        assert_eq!(r.total_bytes(), 1005);
        let deep = r.files.iter().find(|f| f.rel_path.ends_with("deep.bin")).unwrap();
        assert_eq!(deep.rel_path, Path::new("a").join("b").join("deep.bin"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn size_collisions_group_same_sizes_and_skip_empty_files() {
        let root = temp_dir("collide");
        fs::write(root.join("one.dat"), vec![1u8; 300]).unwrap();
        fs::write(root.join("two.dat"), vec![2u8; 300]).unwrap(); // same size, different content
        fs::write(root.join("big1.dat"), vec![3u8; 5000]).unwrap();
        fs::write(root.join("big2.dat"), vec![3u8; 5000]).unwrap();
        fs::write(root.join("unique.dat"), vec![4u8; 77]).unwrap();
        fs::write(root.join("empty1"), b"").unwrap();
        fs::write(root.join("empty2"), b"").unwrap();

        let r = scan(&root);
        let groups = r.size_collisions();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].size, 5000, "largest potential waste first");
        assert_eq!(groups[0].potential_waste(), 5000);
        assert_eq!(groups[1].size, 300);
        fs::remove_dir_all(root).unwrap();
    }

    fn hash_all(root: &Path) -> HashReport {
        let r = scan(root);
        hash_candidates(root, &r.size_collisions(), |_| true)
    }

    #[test]
    fn hashing_confirms_identical_files_and_drops_same_size_ones_that_differ() {
        let root = temp_dir("hash");
        fs::create_dir_all(root.join("copy")).unwrap();
        fs::write(root.join("one.dat"), vec![1u8; 300]).unwrap();
        fs::write(root.join("two.dat"), vec![2u8; 300]).unwrap(); // same size, different
        fs::write(root.join("big.dat"), vec![3u8; 5000]).unwrap();
        fs::write(root.join("copy/big.dat"), vec![3u8; 5000]).unwrap();
        fs::write(root.join("copy/big again.dat"), vec![3u8; 5000]).unwrap();

        let rep = hash_all(&root);
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(rep.groups.len(), 1);
        let g = &rep.groups[0];
        assert_eq!(g.size, 5000);
        assert_eq!(g.files.len(), 3);
        assert_eq!(g.wasted(), 10_000);
        assert_eq!(g.blake3, blake3::hash(&vec![3u8; 5000]).to_hex().to_string());
        assert_eq!(rep.wasted(), 10_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn large_files_with_the_same_start_are_told_apart_by_the_full_hash() {
        let root = temp_dir("prefix");
        let size = (PREFIX_BYTES * 3) as usize;
        let a = vec![7u8; size];
        let mut b = a.clone();
        *b.last_mut().unwrap() = 8; // same first 64 KiB, different end
        let c = vec![9u8; size]; // different from the first byte
        fs::write(root.join("a.bin"), &a).unwrap();
        fs::write(root.join("a copy.bin"), &a).unwrap();
        fs::write(root.join("b.bin"), &b).unwrap();
        fs::write(root.join("c.bin"), &c).unwrap();

        let rep = hash_all(&root);
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].files, vec![PathBuf::from("a copy.bin"), PathBuf::from("a.bin")]);
        assert_eq!(rep.groups[0].blake3, blake3::hash(&a).to_hex().to_string());
        // c.bin is dropped after its prefix, so it is never read in full.
        let full_reads = 3 * size as u64; // a, a copy, b
        let prefix_reads = 4 * PREFIX_BYTES;
        assert!(rep.bytes_read <= full_reads + prefix_reads + 4, "read {} bytes", rep.bytes_read);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_that_changed_since_the_scan_is_an_error_not_a_duplicate() {
        let root = temp_dir("changed");
        fs::write(root.join("x.dat"), vec![5u8; 400]).unwrap();
        fs::write(root.join("y.dat"), vec![5u8; 400]).unwrap();
        let r = scan(&root);
        let groups = r.size_collisions();
        fs::write(root.join("y.dat"), vec![5u8; 401]).unwrap();
        let rep = hash_candidates(&root, &groups, |_| true);
        assert!(rep.groups.is_empty());
        assert_eq!(rep.errors.len(), 1);
        assert_eq!(rep.errors[0].path, PathBuf::from("y.dat"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hashing_stops_when_progress_says_so() {
        let root = temp_dir("stop");
        fs::write(root.join("p.dat"), vec![1u8; 100]).unwrap();
        fs::write(root.join("q.dat"), vec![1u8; 100]).unwrap();
        let r = scan(&root);
        let rep = hash_candidates(&root, &r.size_collisions(), |_| false);
        assert!(rep.groups.is_empty());
        assert!(rep.errors.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_root_is_reported_not_fatal() {
        let r = scan(Path::new("/definitely/not/a/real/path/atticarc"));
        assert!(r.files.is_empty());
        assert_eq!(r.errors.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let root = temp_dir("links");
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/file.txt"), b"data").unwrap();
        std::os::unix::fs::symlink(&root, root.join("real/loop")).unwrap();

        let r = scan(&root);
        assert_eq!(r.files.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
