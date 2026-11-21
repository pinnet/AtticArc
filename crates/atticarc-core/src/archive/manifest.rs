//! What an archive says about itself: the manifest inside it, the files its
//! exclusions left out, and the reference files that stand in for duplicates.

use crate::catalog::{ArchiveRecord, DATA_DIR, EntryRecord};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sevenz_turbo::{ArchiveReader, Password};
use std::path::{Path, PathBuf};

use super::*;

/// What an archive's manifest holds. Written inside the archive before it
/// is finished, so `archive.archive_path` and `archive.archive_bytes` are
/// empty there; [`read_manifest`] fills them in.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub archive: ArchiveRecord,
    pub entries: Vec<EntryRecord>,
    /// Files the exclusions left out of this archive.
    #[serde(default)]
    pub excluded: Excluded,
}

/// Files left out of an archive by the exclusions, recorded so the folder
/// can still be checked before it is deleted.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Excluded {
    /// File name of the archive beside this one holding the set-aside
    /// files. Only archives made before the set-aside files went inside
    /// have one.
    pub set_aside_archive: Option<String>,
    /// The archive of set-aside files kept inside this one, stored without
    /// compressing it again at [`EXCLUDED_ENTRY`], with its size and hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_aside_inside: Option<EntryRecord>,
    pub set_aside: Vec<EntryRecord>,
    /// Files archived nowhere, deleted with the folder.
    pub discarded: Vec<EntryRecord>,
    /// Files removed from this archive because another archive holds an
    /// identical copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duplicates: Vec<EntryRecord>,
    /// Duplicates replaced by a reference file pointing to their original.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<Replaced>,
}

/// The extension of the small reference file that stands in for a
/// duplicate: `IMG_1.jpg` becomes `IMG_1.jpg.atticarcdep`. Not a Windows
/// shortcut, so it isn't mistaken for one.
pub const DEP_EXT: &str = "atticarcdep";

/// Where the reference file for the file at `rel` goes.
pub fn dep_path(rel: &str) -> String {
    format!("{rel}.{DEP_EXT}")
}

/// A duplicate replaced by a reference file: the file as it was archived
/// here, and where the original is kept.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Replaced {
    /// The file as it was archived here.
    pub entry: EntryRecord,
    /// Path inside this archive of the reference file standing in for it.
    pub dep_path: String,
    /// File name of the archive that holds the original, in the same folder.
    pub original_archive: String,
    /// Path of the original inside that archive.
    pub original_path: String,
    pub replaced_utc: String,
}

/// What a `.atticarcdep` file holds: plain JSON, so it can be read in any
/// text editor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DepFile {
    /// Format version.
    pub atticarc_dep: u32,
    pub note: String,
    #[serde(flatten)]
    pub replaced: Replaced,
}

impl DepFile {
    pub fn new(replaced: Replaced) -> Self {
        let note = format!(
            "AtticARC replaced this file with a reference because an identical copy (same size and BLAKE3 hash) \
             is kept in {} at {}. AtticARC can extract the original or put the file back.",
            replaced.original_archive, replaced.original_path
        );
        DepFile { atticarc_dep: 1, note, replaced }
    }
}

/// `entries` with the inner archive of set-aside files, if `excluded` has
/// one: everything a check of the main archive must find.
pub(super) fn with_inner(entries: &[EntryRecord], excluded: &Excluded) -> Vec<EntryRecord> {
    entries.iter().cloned().chain(excluded.set_aside_inside.clone()).collect()
}

/// Where earlier versions wrote an archive's manifest: a file beside it.
pub(super) fn old_manifest_path(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.manifest.json", archive.display()))
}

/// Reads the manifest inside `archive`, or for archives made before
/// manifests went inside, the `.manifest.json` file beside it. Only the
/// manifest's own small block is decoded.
pub fn read_manifest(archive: &Path) -> Result<Manifest> {
    let mut reader = ArchiveReader::open(archive, Password::empty()).with_context(|| format!("can't open {}", archive.display()))?;
    let bytes = match reader.read_file(MANIFEST_ENTRY) {
        Ok(b) => b,
        Err(_) => {
            // Beside the archive, or moved into the .atticarc folder with the catalogue.
            let beside = old_manifest_path(archive);
            let moved = archive.parent().zip(beside.file_name()).map(|(d, n)| d.join(DATA_DIR).join(n));
            std::fs::read(&beside)
                .or_else(|e| moved.map_or(Err(e), std::fs::read))
                .map_err(|_| anyhow!("{} has no AtticARC manifest", archive.display()))?
        }
    };
    let mut m: Manifest = serde_json::from_slice(&bytes).with_context(|| format!("the manifest in {} can't be read", archive.display()))?;
    m.archive.archive_path = crate::settings::strip_verbatim(archive).to_string_lossy().into_owned();
    m.archive.archive_bytes = std::fs::metadata(archive)?.len();
    Ok(m)
}
