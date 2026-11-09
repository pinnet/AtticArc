//! Free disk space, so a job refuses to start when the destination can't
//! hold what it will write, instead of filling the disk halfway through.
//!
//! Estimates are worst case: nothing is assumed to compress. A big folder of
//! photos, video or installers barely shrinks, and running out of space hours
//! into a job is far worse than being told up front.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Kept free on top of every estimate, for the catalogue, manifests and the
/// file system itself. A disk filled to the last byte is slow and fragile.
pub const MARGIN: u64 = 256 * 1024 * 1024;

/// Worst-case size of a .7z holding `input` bytes: as if nothing compresses,
/// plus 1% for headers, block overhead and the manifest.
pub fn archive_worst_case(input: u64) -> u64 {
    input.saturating_add(input / 100)
}

/// Free space available to this user on the drive holding `dir`. If `dir`
/// doesn't exist yet, the nearest folder above it that does is asked
/// instead. `None` if it can't be found out.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let dir = existing_ancestor(dir)?;
    free_bytes_at(&crate::settings::strip_verbatim(&dir))
}

/// Fails with a plain message if the drive holding `dir` has less than
/// `needed` bytes plus [`MARGIN`] free. `what` says what the space is for,
/// such as "archiving Photos". If the free space can't be found out, the
/// check passes: the write itself will still fail cleanly if the disk fills.
pub fn ensure_free(dir: &Path, needed: u64, what: &str) -> Result<()> {
    let need = needed.saturating_add(MARGIN);
    if let Some(free) = free_bytes(dir) {
        if free < need {
            bail!(
                "not enough free space for {what}: needs about {} on {}, but only {} is free",
                human(need),
                crate::settings::strip_verbatim(dir).display(),
                human(free)
            );
        }
    }
    Ok(())
}

fn existing_ancestor(dir: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(dir).ok()?;
    abs.ancestors().find(|p| p.is_dir()).map(Path::to_path_buf)
}

#[cfg(windows)]
fn free_bytes_at(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path and an out-pointer to a local; the other
    // out-pointers are optional and passed as null.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn free_bytes_at(dir: &Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-Pk").arg(dir).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1)?.split_whitespace().nth(3)?.parse::<u64>().ok().map(|k| k * 1024)
}

/// Bytes as KB, MB, GB or TB (powers of 1024), for messages.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_case_adds_one_percent_and_never_overflows() {
        assert_eq!(archive_worst_case(0), 0);
        assert_eq!(archive_worst_case(1000), 1010);
        assert_eq!(archive_worst_case(u64::MAX), u64::MAX);
    }

    #[test]
    fn free_space_is_found_for_a_real_folder_and_a_missing_one_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let free = free_bytes(dir.path()).expect("free space of the temp folder");
        assert!(free > 0);
        let missing = dir.path().join("not").join("made").join("yet");
        assert!(free_bytes(&missing).is_some(), "falls back to the nearest existing folder");
    }

    #[test]
    fn ensure_free_passes_small_needs_and_refuses_impossible_ones() {
        let dir = tempfile::tempdir().unwrap();
        ensure_free(dir.path(), 1, "a test").unwrap();
        let err = ensure_free(dir.path(), u64::MAX - MARGIN, "a test").unwrap_err().to_string();
        assert!(err.contains("not enough free space for a test"), "{err}");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
