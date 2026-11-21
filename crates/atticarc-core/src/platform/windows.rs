//! Windows: Win32 calls for free space and hiding, and the NTFS change
//! journal for change markers.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const NAME: &str = "Windows";
pub const CHANGE_MARKER: &str = "usn";
pub const CHANGE_MARKER_LABEL: &str = "change journal";
pub const PATHS_IGNORE_CASE: bool = true;

pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path and an out-pointer to a local; the other
    // out-pointers are optional and passed as null.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

/// The file's NTFS (or ReFS) change journal number: see [`crate::journal`].
pub fn change_marker(file: &Path) -> Option<i64> {
    crate::journal::file_usn(file)
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn hide(path: &Path) {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::process::CommandExt;
    const HIDDEN: u32 = 0x2;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if std::fs::metadata(path).is_ok_and(|m| m.file_attributes() & HIDDEN != 0) {
        return;
    }
    // attrib doesn't understand \\?\ paths.
    let _ =
        std::process::Command::new("attrib").arg("+h").arg(crate::settings::strip_verbatim(path)).creation_flags(CREATE_NO_WINDOW).status();
}

pub fn set_times(path: &Path, modified: Option<SystemTime>, created: Option<SystemTime>, is_dir: bool) {
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    let mut times = std::fs::FileTimes::new();
    if let Some(m) = modified {
        times = times.set_modified(m);
    }
    if let Some(c) = created {
        times = times.set_created(c);
    }
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    if is_dir {
        // A folder can only be opened for this with backup semantics.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        open.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    if let Ok(f) = open.open(path) {
        let _ = f.set_times(times);
    }
}
