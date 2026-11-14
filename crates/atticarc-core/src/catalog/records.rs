//! What the catalogue holds and hands back: the records of archives and
//! files, and the keys they are known by.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One file or folder as recorded in the catalog and the manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryRecord {
    /// Path inside the archive, `/` separated, relative to the archived folder.
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_utc: Option<String>,
    pub created_utc: Option<String>,
    pub blake3: Option<String>,
    pub sha256: Option<String>,
    /// `lzma2`, `copy` or `none` for folders.
    pub method: String,
}

/// An archive as recorded in the catalog and the manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveRecord {
    pub name: String,
    pub archive_path: String,
    /// Lookup key for the source folder: canonical, and lower case on Windows.
    pub source_path: String,
    /// The source folder as it is spelled on disk, for showing to people.
    /// Missing in manifests written before it was added.
    #[serde(default)]
    pub source_display: String,
    pub created_utc: String,
    pub engine: String,
    pub level: u32,
    pub threads: u32,
    pub input_bytes: u64,
    pub archive_bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
}

/// A verified archive of a folder, as the folder list needs it.
#[derive(Clone, Debug)]
pub struct ArchivedFolder {
    pub archive_id: i64,
    pub archive_path: PathBuf,
    pub created_utc: String,
    pub archive_bytes: u64,
}

/// An archive in the duplicates tree.
#[derive(Clone, Debug)]
pub struct TreeArchive {
    pub id: i64,
    pub name: String,
    pub archive_path: String,
    pub source_path: String,
    pub created_utc: String,
    pub file_count: u64,
    /// A set-aside archive, whose files are never originals or leaves.
    pub set_aside: bool,
    /// Files already replaced by reference files.
    pub replaced: u64,
    /// Files here that reference files in other archives point to.
    pub referenced: u64,
    /// Files whose oldest copy is in this archive.
    pub originals: u64,
    /// Later copies in this archive, by path.
    pub leaves: Vec<TreeLeaf>,
}

/// A later copy of a file, and the original it duplicates.
#[derive(Clone, Debug)]
pub struct TreeLeaf {
    pub entry_id: i64,
    pub rel_path: String,
    pub size: u64,
    pub modified_utc: Option<String>,
    pub blake3: String,
    pub sha256: Option<String>,
    pub original_entry_id: i64,
    pub original_archive_id: i64,
    pub original_archive: String,
    pub original_path: String,
}

/// A duplicate to replace, as [`crate::archive::replace_duplicates`] needs it.
#[derive(Clone, Debug)]
pub struct DedupeLeaf {
    pub archive_id: i64,
    pub archive_name: String,
    pub archive_path: PathBuf,
    pub archive_created: String,
    pub entry: EntryRecord,
    pub original_archive_path: PathBuf,
    pub original_path: String,
}

/// An archive as the catalogue screen lists it.
#[derive(Clone, Debug)]
pub struct ArchiveSummary {
    pub id: i64,
    pub name: String,
    pub archive_path: String,
    pub source_path: String,
    pub created_utc: String,
    pub input_bytes: u64,
    pub archive_bytes: u64,
    pub file_count: u64,
    pub source_deleted_utc: Option<String>,
}

/// A file found in the catalogue.
#[derive(Clone, Debug)]
pub struct FoundFile {
    pub entry_id: i64,
    pub archive_id: i64,
    pub archive_name: String,
    /// The folder that was archived.
    pub source_path: String,
    /// Path inside the archive, `/` separated.
    pub rel_path: String,
    pub size: u64,
    pub modified_utc: Option<String>,
    pub blake3: Option<String>,
    pub sha256: Option<String>,
    /// How many catalogued files share this file's size and hash, itself included.
    pub copies: u64,
}

/// Where a catalogued file is.
#[derive(Clone, Debug)]
pub struct EntryLocation {
    pub archive_path: PathBuf,
    pub rel_path: String,
    pub blake3: Option<String>,
    pub size: u64,
}

/// A folder some of whose files the exclusions left out of its archive.
#[derive(Clone, Debug)]
pub struct ExcludedFolder {
    /// The folder's main archive.
    pub archive: ArchivedFolder,
    pub name: String,
    /// The folder that was archived, as spelled on disk.
    pub source: PathBuf,
    pub source_deleted_utc: Option<String>,
    /// The second archive beside the main one holding the set-aside files,
    /// for archives made before those went inside.
    pub aside_archive: Option<PathBuf>,
    /// Whether the set-aside files are in an archive inside the main one.
    pub aside_inside: bool,
    /// (path inside the folder, size) of each set-aside file.
    pub set_aside: Vec<(String, u64)>,
    /// (path inside the folder, size) of each discarded file.
    pub discarded: Vec<(String, u64)>,
}

/// What one archive holds, as recorded in the catalogue.
#[derive(Clone, Debug, Default)]
pub struct ArchiveContents {
    pub files: Vec<FoundFile>,
    /// Folder paths inside the archive, `/` separated.
    pub folders: Vec<String>,
}

/// Catalogued files that share one size and BLAKE3 hash.
#[derive(Clone, Debug)]
pub struct DuplicateGroup {
    pub blake3: String,
    pub size: u64,
    pub copies: Vec<FoundFile>,
}

impl DuplicateGroup {
    /// Space that removing all but one copy would free.
    pub fn reclaimable(&self) -> u64 {
        self.size * (self.copies.len() as u64).saturating_sub(1)
    }
}

/// The form a source folder's path is stored and looked up in.
pub fn path_key(p: &Path) -> String {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = p.to_string_lossy().to_string();
    // canonicalize gives \\?\C:\... on Windows; store the plain form.
    let s = s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s);
    if cfg!(windows) { s.to_lowercase() } else { s }
}

pub fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Local wall-clock time as `HH:MM:SS`, for logs shown to people.
pub fn local_hhmmss() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Local wall-clock time as `HH:MM`, for logs shown to people.
pub fn local_hhmm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// An archive's identity, independent of its file name: a BLAKE3 hash of
/// its creation time and every entry's path, kind, size and hash. The
/// catalogue and the manifest inside the archive give the same value, so a
/// renamed archive can be matched to its record without hashing the file.
pub fn fingerprint(created_utc: &str, entries: &[EntryRecord]) -> String {
    let mut rows: Vec<&EntryRecord> = entries.iter().collect();
    rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let mut h = blake3::Hasher::new();
    h.update(created_utc.as_bytes());
    h.update(b"\n");
    for e in rows {
        h.update(e.rel_path.as_bytes());
        h.update(if e.is_dir { b"\0d\0" } else { b"\0f\0" });
        h.update(e.size.to_string().as_bytes());
        h.update(b"\0");
        h.update(e.blake3.as_deref().unwrap_or("").as_bytes());
        h.update(b"\n");
    }
    h.finalize().to_hex().to_string()
}

/// A Clean up check of an archived folder, as recorded in the catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedCheck {
    pub archive_id: i64,
    /// The folder that was checked (it may have been renamed since archiving).
    pub source_path: String,
    pub passed: bool,
    /// For a folder that passed: its snapshot then, from `archive::folder_snapshot`.
    pub snapshot: Option<String>,
    /// Files a discard rule left out of the archive, deleted with the folder.
    pub discarded: usize,
    /// Why it failed; empty if it passed.
    pub detail: String,
    pub checked_utc: String,
    /// The archive's size and modified time (Unix nanoseconds) when the check
    /// passed. Missing for checks saved before these were recorded.
    pub archive_len: Option<u64>,
    pub archive_modified: Option<i64>,
}

/// An archive due for a scrub (decoding it again to catch damage).
#[derive(Clone, Debug)]
pub struct ScrubItem {
    pub archive_id: i64,
    pub name: String,
    pub archive_path: PathBuf,
    /// When it last decoded and matched its record.
    pub decoded_utc: String,
    pub archive_bytes: u64,
}
