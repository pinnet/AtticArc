//! Settings kept between runs, shared by the GUI and `attic archive`.
//!
//! Stored in `%APPDATA%\attic\settings.json`. On first run, settings saved
//! by foldarc (`%APPDATA%\foldarc\settings.json`) are picked up, so the
//! output folder and protected folders carry over.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::archive::{self, Settings};

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
    /// GUI colour scheme.
    pub dark_mode: bool,
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
            dark_mode: false,
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
        }
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn settings_file() -> PathBuf {
    config_dir().join("attic").join("settings.json")
}

pub fn load() -> Saved {
    let read = |p: PathBuf| std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok());
    read(settings_file())
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

/// Beside the source folder, never inside it, in a shared `Attic archives`
/// folder: `D:\Photos` → `D:\Attic archives\Photos`. Never inside the source,
/// because the source may be protected or deleted later.
pub fn default_out_dir(source: &Path) -> PathBuf {
    match (source.parent(), source.file_name()) {
        (Some(parent), Some(name)) => parent.join(OUT_DIR_NAME).join(name),
        _ => source.join(OUT_DIR_NAME),
    }
}

const OUT_DIR_NAME: &str = "Attic archives";

/// `\\?\C:\x` → `C:\x`, the form people expect to see.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(d, Path::new("/data/Attic archives/Photos"));
        assert_eq!(default_out_dir(Path::new("/")), Path::new("/Attic archives"));
    }
}
