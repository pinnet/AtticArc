//! Where the catalogue is kept, its tables, and the columns added since
//! the first release.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::*;

pub const CATALOG_FILE: &str = "catalog.db";

/// The hidden folder AtticARC keeps its own files in: inside the output
/// folder for the catalogue, and inside each archive for its manifest.
pub const DATA_DIR: &str = ".atticarc";

/// Where the catalogue for the output folder `dir` lives.
pub fn db_path(dir: &Path) -> PathBuf {
    dir.join(DATA_DIR).join(CATALOG_FILE)
}

/// Whether `dir` has a catalogue, in its current or its old place.
pub fn exists(dir: &Path) -> bool {
    db_path(dir).is_file() || dir.join(CATALOG_FILE).is_file()
}

/// Creates `dir/.atticarc` if needed and makes sure it is hidden on Windows
/// (the dot hides it elsewhere).
pub(super) fn make_data_dir(dir: &Path) -> Result<()> {
    let data = dir.join(DATA_DIR);
    if !data.is_dir() {
        std::fs::create_dir_all(&data).with_context(|| format!("create {}", data.display()))?;
    }
    hide(&data);
    Ok(())
}

/// Sets the Hidden attribute on Windows if it isn't set. Best effort: a
/// folder that stays visible does no harm.
#[cfg(windows)]
pub(super) fn hide(path: &Path) {
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

#[cfg(not(windows))]
pub(super) fn hide(_path: &Path) {}

/// Moves a catalogue from where earlier versions kept it, loose in the
/// output folder, into `.atticarc`.
pub(super) fn move_old_catalog(dir: &Path) -> Result<()> {
    let old = dir.join(CATALOG_FILE);
    if !old.is_file() || db_path(dir).is_file() {
        return Ok(());
    }
    make_data_dir(dir)?;
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", old.display()));
        if from.is_file() {
            let to = PathBuf::from(format!("{}{suffix}", db_path(dir).display()));
            std::fs::rename(&from, &to).with_context(|| format!("move {} into {DATA_DIR}", from.display()))?;
        }
    }
    Ok(())
}

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS archives (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL,
    archive_path    TEXT NOT NULL,
    source_path     TEXT NOT NULL,
    created_utc     TEXT NOT NULL,
    engine          TEXT NOT NULL,
    level           INTEGER NOT NULL,
    threads         INTEGER NOT NULL,
    input_bytes     INTEGER NOT NULL,
    archive_bytes   INTEGER NOT NULL,
    file_count      INTEGER NOT NULL,
    dir_count       INTEGER NOT NULL,
    status          TEXT NOT NULL,
    source_deleted_utc TEXT
);
CREATE INDEX IF NOT EXISTS archives_source ON archives(source_path);

CREATE TABLE IF NOT EXISTS entries (
    id              INTEGER PRIMARY KEY,
    archive_id      INTEGER NOT NULL REFERENCES archives(id),
    rel_path        TEXT NOT NULL,
    name            TEXT NOT NULL,
    ext             TEXT NOT NULL,
    kind            TEXT NOT NULL,
    size            INTEGER NOT NULL,
    modified_utc    TEXT,
    created_utc     TEXT,
    blake3          TEXT,
    sha256          TEXT,
    method          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS entries_archive ON entries(archive_id);
CREATE INDEX IF NOT EXISTS entries_blake3 ON entries(blake3);
CREATE INDEX IF NOT EXISTS entries_size ON entries(size);
CREATE INDEX IF NOT EXISTS entries_ext ON entries(ext);
CREATE INDEX IF NOT EXISTS entries_modified ON entries(modified_utc);

CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(name, rel_path, content='entries', content_rowid='id');

CREATE TABLE IF NOT EXISTS runs (
    id              INTEGER PRIMARY KEY,
    archive_id      INTEGER,
    action          TEXT NOT NULL,
    source_path     TEXT NOT NULL,
    started_utc     TEXT NOT NULL,
    finished_utc    TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    detail          TEXT NOT NULL
);

-- Files the exclusions left out of an archive: set aside in a second
-- archive (aside_archive is its file name beside the main archive, or
-- .atticarc/excluded.7z when it is inside) or discarded.
CREATE TABLE IF NOT EXISTS excluded (
    archive_id      INTEGER NOT NULL REFERENCES archives(id),
    rel_path        TEXT NOT NULL,
    size            INTEGER NOT NULL,
    action          TEXT NOT NULL,
    aside_archive   TEXT
);
CREATE INDEX IF NOT EXISTS excluded_archive ON excluded(archive_id);

-- Duplicates replaced inside an archive by a .atticarcdep reference file:
-- the replaced file's path, the reference file's path, and where the
-- original is (archive file name, path inside it). record is the whole
-- manifest record as JSON.
CREATE TABLE IF NOT EXISTS deps (
    archive_id       INTEGER NOT NULL REFERENCES archives(id),
    rel_path         TEXT NOT NULL,
    dep_path         TEXT NOT NULL,
    original_archive TEXT NOT NULL,
    original_path    TEXT NOT NULL,
    record           TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS deps_archive ON deps(archive_id);
CREATE INDEX IF NOT EXISTS deps_original ON deps(original_archive, original_path);
-- Earlier file names of archives renamed outside AtticARC, so reference
-- files that name an original's archive by its old name still resolve.
-- Clean up's checks of archived folders, kept so a check survives a restart:
-- the folder checked, whether it passed (with a snapshot of it then, to
-- compare with just before deleting) or why not, and when.
CREATE TABLE IF NOT EXISTS cleanup_checks (
    archive_id  INTEGER PRIMARY KEY REFERENCES archives(id),
    source_path TEXT NOT NULL,
    passed      INTEGER NOT NULL,
    snapshot    TEXT,
    discarded   INTEGER NOT NULL DEFAULT 0,
    detail      TEXT NOT NULL DEFAULT '',
    checked_utc TEXT NOT NULL
);
-- Folders ticked on the Clean up page, so the ticks survive a restart.
CREATE TABLE IF NOT EXISTS cleanup_ticked (
    archive_id INTEGER PRIMARY KEY REFERENCES archives(id)
);
CREATE TABLE IF NOT EXISTS archive_aliases (
    old_name   TEXT NOT NULL,
    archive_id INTEGER NOT NULL REFERENCES archives(id)
);
CREATE INDEX IF NOT EXISTS archive_aliases_name ON archive_aliases(old_name);
"#;

/// Adds `column` to `table` if it isn't there yet.
pub(super) fn add_column(conn: &Connection, table: &str, column: &str, kind: &str) -> Result<()> {
    let has: bool =
        conn.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"), params![column], |r| {
            r.get::<_, i64>(0).map(|n| n > 0)
        })?;
    if !has {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

impl Catalog {
    pub(super) fn connect(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Columns added after the first release.
        for (table, column, kind) in [
            // The source folder as spelled on disk.
            ("archives", "source_display", "TEXT"),
            // When the archive last decoded and matched its record (a full
            // check or a scrub); NULL means only when it was made.
            ("archives", "decoded_utc", "TEXT"),
            // The archive's size and modified time when the check passed, so
            // a changed archive is caught before deleting.
            ("cleanup_checks", "archive_len", "INTEGER"),
            ("cleanup_checks", "archive_modified", "INTEGER"),
        ] {
            add_column(&conn, table, column, kind)?;
        }
        Ok(Self { conn })
    }
}
