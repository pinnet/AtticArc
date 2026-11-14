//! macOS.

use std::path::PathBuf;

pub use super::unix::{CHANGE_MARKER, CHANGE_MARKER_LABEL, PATHS_IGNORE_CASE, change_marker, free_bytes, hide, set_times};

pub const NAME: &str = "macOS";

/// `~/Library/Application Support`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
        .unwrap_or_else(std::env::temp_dir)
}
