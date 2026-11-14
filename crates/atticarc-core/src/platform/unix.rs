//! What macOS and Linux share: `df` for free space, status change times
//! for change markers, and Unix file calls.

use std::path::Path;
use std::time::SystemTime;

pub const CHANGE_MARKER: &str = "ctime";
pub const CHANGE_MARKER_LABEL: &str = "change time";
pub const PATHS_IGNORE_CASE: bool = false;

pub fn free_bytes(dir: &Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-Pk").arg(dir).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1)?.split_whitespace().nth(3)?.parse::<u64>().ok().map(|k| k * 1024)
}

/// The file's status change time (`ctime`), in nanoseconds. The system sets
/// it at every write and every change to the file's details (name, times,
/// permissions), and ordinary tools can't set it back, unlike the modified
/// time; so it stands in for the NTFS change journal number.
pub fn change_marker(file: &Path) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(file).ok()?;
    m.ctime().checked_mul(1_000_000_000)?.checked_add(m.ctime_nsec())
}

/// Names starting with a dot (`.atticarc`) are hidden already.
pub fn hide(_path: &Path) {}

/// The created time can't be set here; a folder is opened for reading, as
/// one can't be opened for writing.
pub fn set_times(path: &Path, modified: Option<SystemTime>, _created: Option<SystemTime>, is_dir: bool) {
    let mut times = std::fs::FileTimes::new();
    if let Some(m) = modified {
        times = times.set_modified(m);
    }
    let mut open = std::fs::OpenOptions::new();
    if is_dir {
        open.read(true);
    } else {
        open.write(true);
    }
    if let Ok(f) = open.open(path) {
        let _ = f.set_times(times);
    }
}
