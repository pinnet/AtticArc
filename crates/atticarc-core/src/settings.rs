//! Settings kept between runs, shared by the GUI and `atticarc-cli archive`.
//!
//! Stored in `%APPDATA%\atticarc\settings.json`. On first run, settings saved
//! by foldarc (`%APPDATA%\foldarc\settings.json`) are picked up, so the
//! output folder and protected folders carry over.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::archive::{self, Settings};
use crate::exclude::{self, Rule};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Saved {
    /// Last source folder chosen in the GUI.
    pub source_dir: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    pub level: u32,
    pub store_mode: bool,
    pub threads: Threads,
    pub protected: Vec<PathBuf>,
    /// Files to leave out of archives.
    pub exclusions: Vec<Rule>,
    /// GUI colour scheme.
    pub dark_mode: bool,
    /// Source folders used before, most recent first (at most [`RECENT_MAX`]).
    pub recent_sources: Vec<PathBuf>,
    /// Destination folders used before, most recent first.
    pub recent_dests: Vec<PathBuf>,
    /// Text size of the GUI's log, in points ([`LOG_TEXT_MIN`] to [`LOG_TEXT_MAX`]).
    pub log_text_size: f32,
    /// Draw the GUI with OpenGL instead of the default (Direct3D/Vulkan through
    /// wgpu), which some setups handle better when displays sleep and wake.
    /// Takes effect at the next start.
    pub compatible_graphics: bool,
}

/// Smallest and largest log text size the GUI allows, in points.
pub const LOG_TEXT_MIN: f32 = 9.0;
pub const LOG_TEXT_MAX: f32 = 24.0;

/// How many recent source and destination folders are remembered.
pub const RECENT_MAX: usize = 8;

/// Puts `path` at the front of `list`, removing any earlier copy of it
/// (ignoring case and `\\?\`, as Windows does) and keeping at most
/// [`RECENT_MAX`] entries.
pub fn remember(list: &mut Vec<PathBuf>, path: &Path) {
    let key = |p: &Path| strip_verbatim(p).to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    let k = key(path);
    if k.is_empty() {
        return;
    }
    list.retain(|p| key(p) != k);
    list.insert(0, strip_verbatim(path));
    list.truncate(RECENT_MAX);
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Threads {
    #[default]
    All,
    Half,
    One,
}

impl Threads {
    pub fn count(self) -> u32 {
        let all = archive::default_threads();
        match self {
            Threads::All => all,
            Threads::Half => (all / 2).max(1),
            Threads::One => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Threads::All => "Auto (all cores)",
            Threads::Half => "Half",
            Threads::One => "1",
        }
    }
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            source_dir: None,
            out_dir: None,
            level: 5,
            store_mode: true,
            threads: Threads::All,
            protected: vec![r"C:\7ztest".into(), r"D:\7ztest".into(), r"W:\7ztest".into()],
            exclusions: exclude::default_rules(),
            dark_mode: false,
            recent_sources: Vec::new(),
            recent_dests: Vec::new(),
            log_text_size: 13.0,
            compatible_graphics: false,
        }
    }
}

impl Saved {
    /// The engine settings for archiving folders from `source_dir`.
    pub fn settings_for(&self, source_dir: &Path) -> Settings {
        Settings {
            out_dir: self.out_dir.clone().unwrap_or_else(|| default_out_dir(source_dir)),
            level: self.level,
            store_mode: self.store_mode,
            threads: self.threads.count(),
            protected: self.protected.clone(),
            exclusions: self.exclusions.clone(),
        }
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn settings_file() -> PathBuf {
    config_dir().join("atticarc").join("settings.json")
}

pub fn load() -> Saved {
    let read = |p: PathBuf| std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok());
    // Earlier versions saved under "attic", and before that "foldarc".
    read(settings_file())
        .or_else(|| read(config_dir().join("attic").join("settings.json")))
        .or_else(|| read(config_dir().join("foldarc").join("settings.json")))
        .unwrap_or_default()
}

pub fn store(s: &Saved) {
    let path = settings_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let _ = std::fs::write(path, b);
    }
}

/// Beside the source folder, never inside it, in a shared `AtticARC archives`
/// folder: `D:\Photos` → `D:\AtticARC archives\Photos`. Never inside the source,
/// because the source may be protected or deleted later.
pub fn default_out_dir(source: &Path) -> PathBuf {
    match (source.parent(), source.file_name()) {
        (Some(parent), Some(name)) => parent.join(OUT_DIR_NAME).join(name),
        _ => source.join(OUT_DIR_NAME),
    }
}

const OUT_DIR_NAME: &str = "AtticARC archives";

/// `\\?\C:\x` → `C:\x`, the form people expect to see.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_keeps_most_recent_first_without_duplicates() {
        let mut list = Vec::new();
        remember(&mut list, Path::new(r"E:\Source"));
        remember(&mut list, Path::new(r"F:\Other"));
        remember(&mut list, Path::new(r"e:\source\"));
        assert_eq!(list, vec![PathBuf::from(r"e:\source\"), PathBuf::from(r"F:\Other")]);
        for i in 0..20 {
            remember(&mut list, Path::new(&format!(r"G:\f{i}")));
        }
        assert_eq!(list.len(), RECENT_MAX);
        assert_eq!(list[0], PathBuf::from(r"G:\f19"));
        remember(&mut list, Path::new(""));
        assert_eq!(list.len(), RECENT_MAX, "an empty path isn't remembered");
    }

    #[test]
    fn foldarc_settings_still_parse() {
        // What foldarc writes: no source_dir or threads fields.
        let old = r#"{"out_dir":"D:\\x archives","level":9,"store_mode":false,"protected":["D:\\7ztest"]}"#;
        let s: Saved = serde_json::from_str(old).unwrap();
        assert_eq!(s.level, 9);
        assert!(!s.store_mode);
        assert_eq!(s.threads, Threads::All);
        assert_eq!(s.protected, vec![PathBuf::from(r"D:\7ztest")]);
    }

    #[test]
    fn out_dir_sits_next_to_source() {
        let d = default_out_dir(Path::new("/data/Photos"));
        assert_eq!(d, Path::new("/data/AtticARC archives/Photos"));
        assert_eq!(default_out_dir(Path::new("/")), Path::new("/AtticARC archives"));
    }
}
