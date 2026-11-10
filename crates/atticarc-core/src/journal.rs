//! NTFS change journal numbers (USNs) for files.
//!
//! Every change to a file on an NTFS volume (data written, renamed,
//! attributes or times changed, even timestamps set back afterwards) gives
//! it a new, higher update sequence number. Comparing a file's number now
//! with the one recorded at a check shows whether it changed in between,
//! without reading it. The number is read per file with
//! `FSCTL_READ_FILE_USN_DATA`, which needs no administrator rights.
//!
//! On other file systems (exFAT, FAT32, network shares) and other operating
//! systems there is no number, and callers fall back to timestamps.

use std::path::Path;

/// The file's current change journal number, if its volume keeps a journal.
#[cfg(windows)]
pub fn file_usn(path: &Path) -> Option<i64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_READ_FILE_USN_DATA;

    let long = long_path(path);
    let wide: Vec<u16> = long.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated path; optional pointers passed as null.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    // A USN record: 8-byte aligned, with room for the file name after it.
    let mut buf = [0u64; 160];
    let mut got = 0u32;
    // SAFETY: an open handle, an output buffer of the size given, and a
    // local for the byte count; no input buffer, no overlapped I/O.
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_READ_FILE_USN_DATA,
            std::ptr::null(),
            0,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: the handle was opened above and is closed once.
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    // SAFETY: viewing the u64 buffer as bytes.
    let b: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), std::mem::size_of_val(&buf)) };
    usn_from_record(&b[..(got as usize).min(b.len())])
}

#[cfg(not(windows))]
pub fn file_usn(_path: &Path) -> Option<i64> {
    None
}

/// The USN in a `USN_RECORD_V2` (NTFS) or `USN_RECORD_V3` (ReFS): after
/// the record length and version, then the file and parent references
/// (8 bytes each in V2, 16 in V3).
pub fn usn_from_record(b: &[u8]) -> Option<i64> {
    let major = u16::from_le_bytes([*b.get(4)?, *b.get(5)?]);
    let at = match major {
        2 => 24,
        3 => 40,
        _ => return None,
    };
    Some(i64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// `path` with the `\\?\` prefix, so paths longer than 260 characters work.
#[cfg(windows)]
fn long_path(path: &Path) -> std::path::PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\?\") || !path.is_absolute() {
        path.to_path_buf()
    } else if let Some(unc) = s.strip_prefix(r"\\") {
        std::path::PathBuf::from(format!(r"\\?\UNC\{unc}"))
    } else {
        std::path::PathBuf::from(format!(r"\\?\{s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_usn_from_v2_and_v3_records() {
        let mut v2 = vec![0u8; 64];
        v2[4..6].copy_from_slice(&2u16.to_le_bytes());
        v2[24..32].copy_from_slice(&123_456i64.to_le_bytes());
        assert_eq!(usn_from_record(&v2), Some(123_456));
        let mut v3 = vec![0u8; 80];
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        v3[40..48].copy_from_slice(&987i64.to_le_bytes());
        assert_eq!(usn_from_record(&v3), Some(987));
        assert_eq!(usn_from_record(&[0u8; 3]), None);
        let mut v9 = vec![0u8; 64];
        v9[4..6].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(usn_from_record(&v9), None);
    }

    #[test]
    #[cfg(windows)]
    fn a_files_usn_rises_when_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.txt");
        std::fs::write(&f, b"one").unwrap();
        // Temp folders are usually on NTFS; nothing to test if not.
        let Some(before) = file_usn(&f) else { return };
        let t = std::fs::metadata(&f).unwrap().modified().unwrap();
        std::fs::write(&f, b"two").unwrap();
        // Put the time back, as a tool hiding an edit might.
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
        assert!(file_usn(&f).unwrap() > before);
    }
}
