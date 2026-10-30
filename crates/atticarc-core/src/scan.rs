//! Fast metadata scan: lists files without reading their contents.
//!
//! Hashing means reading every byte, which is slow on USB disks and network shares.
//! A file can only be a duplicate of another file of exactly the same size, so a
//! metadata scan first lets us hash only the files that share a size with another.

use std::collections::HashMap;
use std::fs;
use std::io;
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
