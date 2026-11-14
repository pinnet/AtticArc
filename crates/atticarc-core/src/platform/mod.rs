//! Everything that differs between Windows, macOS and Linux, in one place.
//!
//! The rest of the code never asks which system it is on: it calls these
//! functions, and each system answers by its own means (`windows.rs`,
//! `macos.rs`, `linux.rs`; the last two share `unix.rs`). The three builds
//! are meant to behave the same, so anything added here needs an answer on
//! all three. docs/platforms.md says what each does underneath.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(all(unix, not(target_os = "macos")))]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;
#[cfg(windows)]
use windows as sys;

/// The system's name, for messages: `Windows`, `macOS` or `Linux`.
pub const NAME: &str = sys::NAME;

/// What [`change_marker`] reads on this system, as a folder snapshot's
/// prefix: `usn` (NTFS change journal numbers) or `ctime` (status change
/// times). A snapshot made with one kind never equals one made with another.
pub const CHANGE_MARKER: &str = sys::CHANGE_MARKER;

/// [`CHANGE_MARKER`] in words, for showing beside a check.
pub const CHANGE_MARKER_LABEL: &str = sys::CHANGE_MARKER_LABEL;

/// Whether folder names are compared ignoring case when a source folder is
/// looked up in the catalogue.
pub const PATHS_IGNORE_CASE: bool = sys::PATHS_IGNORE_CASE;

/// Free space, in bytes, on the drive holding `dir` (which must exist).
pub fn free_bytes(dir: &Path) -> Option<u64> {
    sys::free_bytes(dir)
}

/// A number that moves whenever `file` is written or its details change,
/// even if its modified time is put back afterwards, without reading the
/// file; `None` where the drive keeps no such number (exFAT, FAT32 and
/// network shares on Windows).
pub fn change_marker(file: &Path) -> Option<i64> {
    sys::change_marker(file)
}

/// Where AtticARC keeps its settings and log (a folder for all programs;
/// AtticARC makes its own `atticarc` folder inside).
pub fn config_dir() -> PathBuf {
    sys::config_dir()
}

/// Hides a file or folder from ordinary folder listings, where the system
/// needs telling (names starting with a dot are already hidden elsewhere).
/// Best effort: a folder that stays visible does no harm.
pub fn hide(path: &Path) {
    sys::hide(path)
}

/// Sets a file's or folder's times where the system allows; best effort.
pub fn set_times(path: &Path, modified: Option<SystemTime>, created: Option<SystemTime>, is_dir: bool) {
    sys::set_times(path, modified, created, is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_system_answers() {
        assert!(["Windows", "macOS", "Linux"].contains(&NAME));
        assert!(["usn", "ctime"].contains(&CHANGE_MARKER));
        assert!(!CHANGE_MARKER_LABEL.is_empty());
        assert!(!config_dir().as_os_str().is_empty());
        let tmp = tempfile::tempdir().unwrap();
        assert!(free_bytes(tmp.path()).is_some_and(|b| b > 0));
        // Hiding is best effort and never fails.
        hide(tmp.path());
        hide(&tmp.path().join("not there"));
    }

    #[test]
    fn a_change_marker_moves_when_a_file_is_written_even_with_its_time_put_back() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, b"one").unwrap();
        // A drive without markers (exFAT, say) has nothing to compare.
        let Some(before) = change_marker(&f) else { return };
        assert_eq!(change_marker(&f), Some(before), "reading it changes nothing");
        let modified = std::fs::metadata(&f).unwrap().modified().unwrap();
        // Status change times tick with the system clock; let it move on.
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&f, b"two").unwrap();
        set_times(&f, Some(modified), None, false);
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), modified, "the modified time was put back");
        assert!(change_marker(&f).unwrap() > before);
    }

    #[test]
    fn times_are_set_on_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let (f, d) = (tmp.path().join("a.txt"), tmp.path().join("sub"));
        std::fs::write(&f, b"x").unwrap();
        std::fs::create_dir(&d).unwrap();
        let then = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        set_times(&f, Some(then), Some(then), false);
        set_times(&d, Some(then), Some(then), true);
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), then);
        assert_eq!(std::fs::metadata(&d).unwrap().modified().unwrap(), then);
    }
}
