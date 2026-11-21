//! Archiving, verifying and the checks that guard deleting a folder.
//!
//! Engine choice follows the sevenz-bench results: sevenz-turbo, LZMA2,
//! one archive at a time, 32 MiB chunks at level 5. Thread count is a
//! setting (all cores by default).
//!
//! Unlike [`crate::scan`], the scan here fails on anything it can't read:
//! a partial archive would be unsafe to delete from.

use std::path::PathBuf;

use crate::exclude::Rule;

mod again;
mod check;
mod dedupe;
mod extract;
mod identity;
mod manifest;
mod progress;
mod rewrite;
#[cfg(test)]
mod tests;
mod util;
mod verify;
mod walk;
mod write;

pub use again::*;
pub use check::*;
pub use dedupe::*;
pub use extract::*;
pub use identity::*;
pub use manifest::*;
pub use progress::*;
pub use rewrite::*;
use util::*;
pub use verify::*;
pub use walk::*;
pub use write::*;

/// The manifest's path inside every archive: what the archive holds, with
/// every file's hashes, so the record travels with the archive and the
/// catalogue can be rebuilt from it.
pub const MANIFEST_ENTRY: &str = ".atticarc/manifest.json";

/// Where the archive of set-aside files goes inside the main archive, so
/// the destination holds one archive per folder.
pub const EXCLUDED_ENTRY: &str = ".atticarc/excluded.7z";

#[derive(Clone, Debug)]
pub struct Settings {
    pub out_dir: PathBuf,
    pub level: u32,
    pub store_mode: bool,
    pub threads: u32,
    /// Folders the app never deletes from, whatever their state.
    pub protected: Vec<PathBuf>,
    /// Files to leave out of archives.
    pub exclusions: Vec<Rule>,
    /// Store identical files inside a folder once: each later copy goes in
    /// as a small reference file to the first (see [`IDENTICAL_MIN_BYTES`]).
    pub identical_once: bool,
}

/// Identical files smaller than this are archived as they are: a reference
/// file would save next to nothing.
pub const IDENTICAL_MIN_BYTES: u64 = 64 * 1024;

impl Settings {
    pub fn engine_label(&self) -> String {
        format!("sevenz-turbo 0.29.3, LZMA2 level {}", self.level)
    }
}

pub fn default_threads() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4)
}
