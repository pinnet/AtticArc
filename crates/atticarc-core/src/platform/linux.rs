//! Linux (and other Unix systems that aren't macOS).

use std::path::PathBuf;

pub use super::unix::{CHANGE_MARKER, CHANGE_MARKER_LABEL, PATHS_IGNORE_CASE, change_marker, free_bytes, hide, set_times};

pub const NAME: &str = "Linux";

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_dir() -> PathBuf {
    let set = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    set("XDG_CONFIG_HOME").or_else(|| set("HOME").map(|h| h.join(".config"))).unwrap_or_else(std::env::temp_dir)
}
