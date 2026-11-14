//! SQLite catalog of archives and the files inside them.
//!
//! One `catalog.db` lives in a hidden `.atticarc` folder inside the output
//! folder, so the output folder itself shows only the archives. An archive
//! row is only written after the archive has been verified, in one
//! transaction together with its entries. Every archive also carries its own
//! manifest, so the catalogue can be rebuilt from the archives.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

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
fn make_data_dir(dir: &Path) -> Result<()> {
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
fn hide(path: &Path) {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::process::CommandExt;
    const HIDDEN: u32 = 0x2;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if std::fs::metadata(path).is_ok_and(|m| m.file_attributes() & HIDDEN != 0) {
        return;
    }
    // attrib doesn't understand \\?\ paths.
    let _ = std::process::Command::new("attrib")
        .arg("+h")
        .arg(crate::settings::strip_verbatim(path))
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(not(windows))]
fn hide(_path: &Path) {}

/// Moves a catalogue from where earlier versions kept it, loose in the
/// output folder, into `.atticarc`.
fn move_old_catalog(dir: &Path) -> Result<()> {
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

const SCHEMA: &str = r#"
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
fn add_column(conn: &Connection, table: &str, column: &str, kind: &str) -> Result<()> {
    let has: bool = conn.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        params![column],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !has {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

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

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Opens the catalogue in `dir`, creating the folder and the catalogue
    /// if needed. Only archiving should do this; anything that only reads
    /// uses [`Catalog::open_existing`], so it never writes into a folder.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        move_old_catalog(dir)?;
        make_data_dir(dir)?;
        Self::connect(&db_path(dir))
    }

    /// Opens the catalogue in `dir` only if it is already there.
    pub fn open_existing(dir: &Path) -> Result<Self> {
        if !exists(dir) {
            bail!("there is no catalogue in {}", dir.display());
        }
        move_old_catalog(dir)?;
        hide(&dir.join(DATA_DIR));
        Self::connect(&db_path(dir))
    }

    fn connect(path: &Path) -> Result<Self> {
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

    /// Records a verified archive and all its entries in one transaction.
    pub fn insert_archive(&mut self, a: &ArchiveRecord, entries: &[EntryRecord]) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO archives (name, archive_path, source_path, created_utc, engine, level, threads,
                input_bytes, archive_bytes, file_count, dir_count, status, source_display)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'verified', ?12)",
            params![
                a.name,
                a.archive_path,
                a.source_path,
                a.created_utc,
                a.engine,
                a.level,
                a.threads,
                a.input_bytes as i64,
                a.archive_bytes as i64,
                a.file_count as i64,
                a.dir_count as i64,
                a.source_display
            ],
        )?;
        let archive_id = tx.last_insert_rowid();
        insert_entries(&tx, archive_id, entries)?;
        tx.commit()?;
        Ok(archive_id)
    }

    /// Replaces what is recorded for archive `id` after it was rewritten:
    /// its sizes and counts, its entries and what was left out of it.
    pub fn replace_contents(
        &mut self,
        id: i64,
        a: &ArchiveRecord,
        entries: &[EntryRecord],
        ex: &crate::archive::Excluded,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE archives SET input_bytes = ?2, archive_bytes = ?3, file_count = ?4, dir_count = ?5 WHERE id = ?1",
            params![id, a.input_bytes as i64, a.archive_bytes as i64, a.file_count as i64, a.dir_count as i64],
        )?;
        delete_entries(&tx, id)?;
        insert_entries(&tx, id, entries)?;
        tx.execute("DELETE FROM excluded WHERE archive_id = ?1", params![id])?;
        tx.execute("DELETE FROM deps WHERE archive_id = ?1", params![id])?;
        tx.commit()?;
        self.insert_excluded(id, ex)
    }

    /// Forgets archive `id` entirely, for an archive file that was removed.
    pub fn remove_archive(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        delete_entries(&tx, id)?;
        tx.execute("DELETE FROM excluded WHERE archive_id = ?1", params![id])?;
        tx.execute("DELETE FROM deps WHERE archive_id = ?1", params![id])?;
        tx.execute("UPDATE runs SET archive_id = NULL WHERE archive_id = ?1", params![id])?;
        tx.execute("DELETE FROM archives WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// The newest verified archive of `source`, if its source folder has not
    /// been deleted through this app and the archive file still exists.
    pub fn archived_folder(&self, source: &Path) -> Result<Option<ArchivedFolder>> {
        let found = self
            .conn
            .query_row(
                "SELECT id, archive_path, created_utc, archive_bytes FROM archives
                 WHERE source_path = ?1 AND status = 'verified' AND source_deleted_utc IS NULL
                 ORDER BY id DESC LIMIT 1",
                params![path_key(source)],
                |r| {
                    Ok(ArchivedFolder {
                        archive_id: r.get(0)?,
                        archive_path: PathBuf::from(r.get::<_, String>(1)?),
                        created_utc: r.get(2)?,
                        archive_bytes: r.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(found.filter(|f| f.archive_path.is_file()))
    }

    /// Whether an archive at `path` is already in the catalogue.
    pub fn has_archive_path(&self, path: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM archives WHERE archive_path = ?1", params![path], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Points archive `id` at `new_path`, after its file was renamed or moved
    /// outside AtticARC, and remembers its old file name as an alias.
    pub fn relink(&mut self, id: i64, new_path: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let old: String = tx.query_row("SELECT archive_path FROM archives WHERE id = ?1", params![id], |r| r.get(0))?;
        let old_name = Path::new(&old).file_name().unwrap_or_default().to_string_lossy().into_owned();
        tx.execute("UPDATE archives SET archive_path = ?1 WHERE id = ?2", params![new_path, id])?;
        if !old_name.is_empty() {
            tx.execute("INSERT INTO archive_aliases (old_name, archive_id) VALUES (?1, ?2)", params![old_name, id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Marks archive `id` as replaced by a newer archive of the same folder,
    /// its file now at `new_path`. It drops out of every list (they show
    /// verified archives only), but reference files naming it still resolve
    /// through its old name.
    pub fn supersede(&mut self, id: i64, new_path: &str) -> Result<()> {
        self.relink(id, new_path)?;
        self.conn.execute("UPDATE archives SET status = 'superseded' WHERE id = ?1", params![id])?;
        self.conn.execute("DELETE FROM cleanup_checks WHERE archive_id = ?1", params![id])?;
        Ok(())
    }

    /// The current path of the archive once called `old_name` (a file name),
    /// if it was renamed and relinked.
    pub fn path_for_alias(&self, old_name: &str) -> Result<Option<String>> {
        let mut q = self.conn.prepare(
            "SELECT a.archive_path FROM archive_aliases x JOIN archives a ON a.id = x.archive_id
             WHERE x.old_name = ?1 COLLATE NOCASE ORDER BY x.rowid DESC LIMIT 1",
        )?;
        Ok(q.query_row(params![old_name], |r| r.get(0)).optional()?)
    }

    pub fn entries(&self, archive_id: i64) -> Result<Vec<EntryRecord>> {
        let mut q = self.conn.prepare(
            "SELECT rel_path, kind, size, modified_utc, created_utc, blake3, sha256, method
             FROM entries WHERE archive_id = ?1 ORDER BY rel_path",
        )?;
        let rows = q.query_map(params![archive_id], |r| {
            Ok(EntryRecord {
                rel_path: r.get(0)?,
                is_dir: r.get::<_, String>(1)? == "dir",
                size: r.get::<_, i64>(2)? as u64,
                modified_utc: r.get(3)?,
                created_utc: r.get(4)?,
                blake3: r.get(5)?,
                sha256: r.get(6)?,
                method: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Records what the exclusions left out of an archive.
    pub fn insert_excluded(&mut self, archive_id: i64, ex: &crate::archive::Excluded) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut q = tx.prepare(
                "INSERT INTO excluded (archive_id, rel_path, size, action, aside_archive) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            // The archive beside (older archives), or the one inside.
            let aside = ex.set_aside_archive.as_deref().or(ex.set_aside_inside.as_ref().map(|_| crate::archive::EXCLUDED_ENTRY));
            for e in &ex.set_aside {
                q.execute(params![archive_id, e.rel_path, e.size as i64, "set_aside", aside])?;
            }
            for e in &ex.discarded {
                q.execute(params![archive_id, e.rel_path, e.size as i64, "discard", Option::<String>::None])?;
            }
            let mut d = tx.prepare(
                "INSERT INTO deps (archive_id, rel_path, dep_path, original_archive, original_path, record)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for r in &ex.deps {
                d.execute(params![
                    archive_id,
                    r.entry.rel_path,
                    r.dep_path,
                    r.original_archive,
                    r.original_path,
                    serde_json::to_string(r)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every verified archive whose folder had files left out by the
    /// exclusions, newest first.
    pub fn excluded_folders(&self) -> Result<Vec<ExcludedFolder>> {
        let mut q = self.conn.prepare(
            "SELECT a.id, a.name, a.archive_path, COALESCE(a.source_display, a.source_path), a.created_utc,
                    a.archive_bytes, a.source_deleted_utc
             FROM archives a
             WHERE a.status = 'verified' AND EXISTS (SELECT 1 FROM excluded x WHERE x.archive_id = a.id)
             ORDER BY a.id DESC",
        )?;
        let mut out: Vec<ExcludedFolder> = q
            .query_map([], |r| {
                Ok(ExcludedFolder {
                    archive: ArchivedFolder {
                        archive_id: r.get(0)?,
                        archive_path: PathBuf::from(r.get::<_, String>(2)?),
                        created_utc: r.get(4)?,
                        archive_bytes: r.get::<_, i64>(5)? as u64,
                    },
                    name: r.get(1)?,
                    source: PathBuf::from(r.get::<_, String>(3)?),
                    source_deleted_utc: r.get(6)?,
                    aside_archive: None,
                    aside_inside: false,
                    set_aside: Vec::new(),
                    discarded: Vec::new(),
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut files = self.conn.prepare(
            "SELECT rel_path, size, action, aside_archive FROM excluded WHERE archive_id = ?1 ORDER BY rel_path",
        )?;
        for f in &mut out {
            let rows = files.query_map(params![f.archive.archive_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?))
            })?;
            for row in rows {
                let (rel, size, action, aside) = row?;
                if action == "discard" {
                    f.discarded.push((rel, size));
                } else {
                    f.set_aside.push((rel, size));
                    if aside.as_deref() == Some(crate::archive::EXCLUDED_ENTRY) {
                        f.aside_inside = true;
                    } else if f.aside_archive.is_none() {
                        f.aside_archive = aside.map(|n| f.archive.archive_path.with_file_name(n));
                    }
                }
            }
        }
        Ok(out)
    }

    /// The duplicates replaced by reference files in archive `archive_id`.
    pub fn deps(&self, archive_id: i64) -> Result<Vec<crate::archive::Replaced>> {
        let mut q = self.conn.prepare("SELECT record FROM deps WHERE archive_id = ?1 ORDER BY rel_path")?;
        let rows = q.query_map(params![archive_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    /// The replaced file a reference file at `dep_path` in archive `archive_id` stands for.
    pub fn dep_at(&self, archive_id: i64, dep_path: &str) -> Result<Option<crate::archive::Replaced>> {
        let rec: Option<String> = self
            .conn
            .query_row("SELECT record FROM deps WHERE archive_id = ?1 AND dep_path = ?2", params![archive_id, dep_path], |r| r.get(0))
            .optional()?;
        Ok(rec.map(|r| serde_json::from_str(&r)).transpose()?)
    }

    /// Whether the file `rel_path` in the archive at `archive_path` is the
    /// original some reference file points to.
    pub fn is_dep_original(&self, archive_path: &Path, rel_path: &str) -> Result<bool> {
        let name = archive_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM deps d JOIN archives a ON a.id = d.archive_id
             WHERE d.original_archive = ?1 AND d.original_path = ?2",
            params![name, rel_path],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The reference files that point to the file `rel_path` in the archive
    /// at `archive_path`, as (archive name, path of the replaced file).
    pub fn references_to(&self, archive_path: &Path, rel_path: &str) -> Result<Vec<(String, String)>> {
        let name = archive_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut q = self.conn.prepare(
            "SELECT a.name, d.rel_path FROM deps d JOIN archives a ON a.id = d.archive_id
             WHERE d.original_archive = ?1 AND d.original_path = ?2 ORDER BY a.created_utc, a.id, d.rel_path",
        )?;
        let rows = q.query_map(params![name, rel_path], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every verified archive, oldest first, with each file in it that is a
    /// later copy of a file kept in an older archive (or earlier in the same
    /// one). The oldest copy of each file is the original and stays; the
    /// others are the leaves that can be replaced by reference files. Files
    /// in set-aside archives are left out: those are managed on the Excluded
    /// page and are rewritten from there.
    pub fn dedupe_tree(&self) -> Result<Vec<TreeArchive>> {
        let mut out: Vec<TreeArchive> = {
            let mut q = self.conn.prepare(
                "SELECT a.id, a.name, a.archive_path, COALESCE(a.source_display, a.source_path), a.created_utc, a.file_count,
                        a.source_path LIKE '%|excluded',
                        (SELECT COUNT(*) FROM deps d WHERE d.archive_id = a.id)
                 FROM archives a WHERE a.status = 'verified' ORDER BY a.created_utc, a.id",
            )?;
            q.query_map([], |r| {
                Ok(TreeArchive {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    archive_path: r.get(2)?,
                    source_path: r.get(3)?,
                    created_utc: r.get(4)?,
                    file_count: r.get::<_, i64>(5)? as u64,
                    set_aside: r.get(6)?,
                    replaced: r.get::<_, i64>(7)? as u64,
                    referenced: 0,
                    originals: 0,
                    leaves: Vec::new(),
                })
            })?
            .collect::<std::result::Result<_, _>>()?
        };
        {
            // Originals that reference files point to, by the archive's file name.
            let mut q = self.conn.prepare("SELECT original_archive, COUNT(DISTINCT original_path) FROM deps GROUP BY original_archive")?;
            let counts: std::collections::HashMap<String, u64> =
                q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<std::result::Result<_, _>>()?;
            for a in &mut out {
                let name = Path::new(&a.archive_path).file_name().unwrap_or_default().to_string_lossy().into_owned();
                a.referenced = counts.get(&name).copied().unwrap_or(0);
            }
        }
        let mut q = self.conn.prepare(&format!(
            "SELECT e.id, a.id, e.rel_path, e.size, e.modified_utc, e.blake3, e.sha256
             FROM entries e JOIN archives a ON a.id = e.archive_id
             JOIN (SELECT e2.blake3 AS b, e2.size AS s FROM entries e2 JOIN archives a2 ON a2.id = e2.archive_id
                   WHERE {COPY} GROUP BY e2.blake3, e2.size HAVING COUNT(*) > 1) g ON g.b = e.blake3 AND g.s = e.size
             WHERE {}
             ORDER BY e.blake3, e.size, a.created_utc, a.id, e.rel_path",
            COPY.replace("e2.", "e.").replace("a2.", "a.")
        ))?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let at: std::collections::HashMap<i64, usize> = out.iter().enumerate().map(|(i, a)| (a.id, i)).collect();
        let mut original: Option<(String, u64, i64, i64, String)> = None;
        for row in rows {
            let (entry_id, archive_id, rel_path, size, modified_utc, blake3, sha256) = row?;
            let Some(&i) = at.get(&archive_id) else { continue };
            match &original {
                Some((b, s, ..)) if *b == blake3 && *s == size => {}
                _ => {
                    out[i].originals += 1;
                    original = Some((blake3, size, entry_id, archive_id, rel_path));
                    continue;
                }
            }
            let (_, _, original_entry_id, original_archive_id, original_path) = original.clone().unwrap();
            let original_archive = out[at[&original_archive_id]].name.clone();
            out[i].leaves.push(TreeLeaf {
                entry_id,
                rel_path,
                size,
                modified_utc,
                blake3,
                sha256,
                original_entry_id,
                original_archive_id,
                original_archive,
                original_path,
            });
        }
        for a in &mut out {
            a.leaves.sort_by(|x, y| x.rel_path.cmp(&y.rel_path));
        }
        Ok(out)
    }

    /// Catalogued file `entry_id` as a duplicate to replace, with its
    /// original, if it is a later copy of a file kept elsewhere.
    pub fn dedupe_leaf(&self, entry_id: i64) -> Result<Option<DedupeLeaf>> {
        let Some((size, blake3)) = self
            .conn
            .query_row(
                &format!(
                    "SELECT e.size, e.blake3 FROM entries e JOIN archives a ON a.id = e.archive_id WHERE e.id = ?1 AND {}",
                    COPY.replace("e2.", "e.").replace("a2.", "a.")
                ),
                params![entry_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        // The oldest copy is the original.
        let first: i64 = self.conn.query_row(
            &format!("SELECT e2.id FROM entries e2 JOIN archives a2 ON a2.id = e2.archive_id
              WHERE e2.blake3 = ?1 AND e2.size = ?2 AND {COPY} ORDER BY a2.created_utc, a2.id, e2.rel_path LIMIT 1"),
            params![blake3, size],
            |r| r.get(0),
        )?;
        if first == entry_id {
            return Ok(None);
        }
        let row = |id: i64| {
            self.conn.query_row(
                "SELECT a.id, a.name, a.archive_path, a.created_utc, e.rel_path, e.size, e.modified_utc, e.created_utc, e.blake3, e.sha256, e.method
                 FROM entries e JOIN archives a ON a.id = e.archive_id WHERE e.id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        PathBuf::from(r.get::<_, String>(2)?),
                        r.get::<_, String>(3)?,
                        EntryRecord {
                            rel_path: r.get(4)?,
                            is_dir: false,
                            size: r.get::<_, i64>(5)? as u64,
                            modified_utc: r.get(6)?,
                            created_utc: r.get(7)?,
                            blake3: r.get(8)?,
                            sha256: r.get(9)?,
                            method: r.get(10)?,
                        },
                    ))
                },
            )
        };
        let (archive_id, archive_name, archive_path, archive_created, entry) = row(entry_id)?;
        let (_, _, original_archive_path, _, original) = row(first)?;
        Ok(Some(DedupeLeaf {
            archive_id,
            archive_name,
            archive_path,
            archive_created,
            entry,
            original_archive_path,
            original_path: original.rel_path,
        }))
    }

    /// Where catalogued file `entry_id` is: its archive and path inside it.
    pub fn entry_location(&self, entry_id: i64) -> Result<Option<EntryLocation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT a.archive_path, e.rel_path, e.blake3, e.size
                 FROM entries e JOIN archives a ON a.id = e.archive_id
                 WHERE e.id = ?1 AND e.kind = 'file'",
                params![entry_id],
                |r| {
                    Ok(EntryLocation {
                        archive_path: PathBuf::from(r.get::<_, String>(0)?),
                        rel_path: r.get(1)?,
                        blake3: r.get(2)?,
                        size: r.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?)
    }

    /// Every catalogued copy of a file in a verified archive, as entry id
    /// and archive path.
    pub fn copies_of(&self, blake3: &str, size: u64) -> Result<Vec<(i64, PathBuf)>> {
        let mut q = self.conn.prepare(
            "SELECT e.id, a.archive_path FROM entries e JOIN archives a ON a.id = e.archive_id
             WHERE e.blake3 = ?1 AND e.size = ?2 AND e.kind = 'file' AND a.status = 'verified'",
        )?;
        let rows = q.query_map(params![blake3, size as i64], |r| Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?))))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The catalogue id of the archive at `path`, if it is catalogued.
    pub fn archive_id_for(&self, path: &Path) -> Result<Option<i64>> {
        let key = crate::settings::strip_verbatim(path).to_string_lossy().into_owned();
        Ok(self
            .conn
            .query_row("SELECT id FROM archives WHERE archive_path = ?1 ORDER BY id DESC LIMIT 1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn mark_source_deleted(&self, archive_id: i64, when_utc: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE archives SET source_deleted_utc = ?2 WHERE id = ?1",
            params![archive_id, when_utc],
        )?;
        self.conn.execute("DELETE FROM cleanup_checks WHERE archive_id = ?1", params![archive_id])?;
        self.conn.execute("DELETE FROM cleanup_ticked WHERE archive_id = ?1", params![archive_id])?;
        Ok(())
    }

    /// The folders ticked on the Clean up page, by archive id.
    pub fn cleanup_ticked(&self) -> Result<Vec<i64>> {
        let mut q = self.conn.prepare("SELECT archive_id FROM cleanup_ticked ORDER BY archive_id")?;
        let ids = q.query_map([], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces the ticked folders with `ids`.
    pub fn set_cleanup_ticked(&mut self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM cleanup_ticked", [])?;
        for id in ids {
            tx.execute("INSERT OR IGNORE INTO cleanup_ticked (archive_id) VALUES (?1)", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records a Clean up check of an archived folder, replacing any earlier one.
    pub fn save_cleanup_check(&self, c: &SavedCheck) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cleanup_checks
                (archive_id, source_path, passed, snapshot, discarded, detail, checked_utc, archive_len, archive_modified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                c.archive_id,
                c.source_path,
                c.passed,
                c.snapshot,
                c.discarded as i64,
                c.detail,
                c.checked_utc,
                c.archive_len.map(|n| n as i64),
                c.archive_modified
            ],
        )?;
        Ok(())
    }

    /// Forgets the Clean up check of an archive, so its folder needs a full
    /// check before it can be deleted.
    pub fn forget_cleanup_check(&self, archive_id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM cleanup_checks WHERE archive_id = ?1", params![archive_id])?;
        Ok(())
    }

    /// Records that the archive decoded and matched its record at `when_utc`.
    pub fn set_decoded(&self, archive_id: i64, when_utc: &str) -> Result<()> {
        self.conn.execute("UPDATE archives SET decoded_utc = ?2 WHERE id = ?1", params![archive_id, when_utc])?;
        Ok(())
    }

    /// When the archive last decoded and matched its record: its latest full
    /// check or scrub, or else when it was made (it is verified then).
    pub fn decoded_utc(&self, archive_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(decoded_utc, created_utc) FROM archives WHERE id = ?1",
                params![archive_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every verified archive, for a scrub: oldest decode first.
    pub fn scrub_list(&self) -> Result<Vec<ScrubItem>> {
        let mut q = self.conn.prepare(
            "SELECT id, name, archive_path, COALESCE(decoded_utc, created_utc), archive_bytes
             FROM archives WHERE status = 'verified' ORDER BY COALESCE(decoded_utc, created_utc), id",
        )?;
        let rows = q.query_map([], |r| {
            Ok(ScrubItem {
                archive_id: r.get(0)?,
                name: r.get(1)?,
                archive_path: PathBuf::from(r.get::<_, String>(2)?),
                decoded_utc: r.get(3)?,
                archive_bytes: r.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every Clean up check recorded, for folders not yet deleted.
    pub fn cleanup_checks(&self) -> Result<Vec<SavedCheck>> {
        let mut q = self.conn.prepare(
            "SELECT archive_id, source_path, passed, snapshot, discarded, detail, checked_utc, archive_len, archive_modified
             FROM cleanup_checks",
        )?;
        let rows = q.query_map([], |r| {
            Ok(SavedCheck {
                archive_id: r.get(0)?,
                source_path: r.get(1)?,
                passed: r.get(2)?,
                snapshot: r.get(3)?,
                discarded: r.get::<_, i64>(4)? as usize,
                detail: r.get(5)?,
                checked_utc: r.get(6)?,
                archive_len: r.get::<_, Option<i64>>(7)?.map(|n| n as u64),
                archive_modified: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn log_run(
        &self,
        archive_id: Option<i64>,
        action: &str,
        source: &Path,
        started_utc: &str,
        outcome: &str,
        detail: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (archive_id, action, source_path, started_utc, finished_utc, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![archive_id, action, path_key(source), started_utc, now_utc(), outcome, detail],
        )?;
        Ok(())
    }

    /// Forgets every archive, entry and run, leaving an empty catalogue.
    /// The archives and manifests on disk are not touched. Returns how
    /// many archives were forgotten.
    pub fn clear(&mut self) -> Result<u64> {
        let tx = self.conn.transaction()?;
        let n: i64 = tx.query_row("SELECT COUNT(*) FROM archives", [], |r| r.get(0))?;
        tx.execute_batch(
            "INSERT INTO entries_fts(entries_fts) VALUES('delete-all');
             DELETE FROM entries; DELETE FROM runs; DELETE FROM excluded; DELETE FROM deps; DELETE FROM archive_aliases; DELETE FROM cleanup_checks; DELETE FROM cleanup_ticked; DELETE FROM archives;",
        )?;
        tx.commit()?;
        Ok(n as u64)
    }

    /// Every verified archive, newest first.
    pub fn archives(&self) -> Result<Vec<ArchiveSummary>> {
        let mut q = self.conn.prepare(
            "SELECT id, name, archive_path, COALESCE(source_display, source_path), created_utc, input_bytes, archive_bytes, file_count,
                    source_deleted_utc
             FROM archives WHERE status = 'verified' ORDER BY id DESC",
        )?;
        let rows = q.query_map([], |r| {
            Ok(ArchiveSummary {
                id: r.get(0)?,
                name: r.get(1)?,
                archive_path: r.get(2)?,
                source_path: r.get(3)?,
                created_utc: r.get(4)?,
                input_bytes: r.get::<_, i64>(5)? as u64,
                archive_bytes: r.get::<_, i64>(6)? as u64,
                file_count: r.get::<_, i64>(7)? as u64,
                source_deleted_utc: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Files whose name or path matches `text`, using the full-text index.
    /// Each word must match the start of a word in the name or path, so
    /// `holi 2014` finds `Holiday-Cornwall/2014/IMG_1.jpg`.
    /// `archive_id` limits the search to one archive.
    pub fn search(&self, text: &str, archive_id: Option<i64>, limit: usize) -> Result<Vec<FoundFile>> {
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| format!("\"{w}\"*"))
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut q = self.conn.prepare(
            "SELECT e.id, a.id, a.name, COALESCE(a.source_display, a.source_path), e.rel_path, e.size, e.modified_utc, e.blake3, e.sha256,
                    (SELECT COUNT(*) FROM entries d WHERE d.blake3 = e.blake3 AND d.size = e.size)
             FROM entries_fts f
             JOIN entries e ON e.id = f.rowid
             JOIN archives a ON a.id = e.archive_id
             WHERE entries_fts MATCH ?1 AND e.kind = 'file' AND a.status = 'verified'
               AND (?2 IS NULL OR a.id = ?2)
             ORDER BY e.size DESC
             LIMIT ?3",
        )?;
        let rows = q.query_map(params![words.join(" "), archive_id, limit as i64], found_file)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every file and folder recorded for one archive, for browsing it
    /// without unpacking. Files carry their copy counts like [`Catalog::search`].
    pub fn contents(&self, archive_id: i64) -> Result<ArchiveContents> {
        let mut q = self.conn.prepare(
            "SELECT e.id, a.id, a.name, COALESCE(a.source_display, a.source_path), e.rel_path, e.size, e.modified_utc, e.blake3, e.sha256,
                    CASE WHEN e.kind = 'file' AND e.blake3 IS NOT NULL
                         THEN (SELECT COUNT(*) FROM entries d
                               WHERE d.blake3 = e.blake3 AND d.size = e.size AND d.kind = 'file')
                         ELSE 1 END,
                    e.kind
             FROM entries e JOIN archives a ON a.id = e.archive_id
             WHERE e.archive_id = ?1
             ORDER BY e.rel_path",
        )?;
        let mut out = ArchiveContents::default();
        let rows = q.query_map(params![archive_id], |r| Ok((found_file(r)?, r.get::<_, String>(10)?)))?;
        for row in rows {
            let (f, kind) = row?;
            if kind == "dir" {
                out.folders.push(f.rel_path);
            } else {
                out.files.push(f);
            }
        }
        Ok(out)
    }

    /// Groups of files with the same size and BLAKE3 hash across all
    /// archives, the most space-wasting groups first.
    pub fn duplicate_groups(&self, limit: usize) -> Result<Vec<DuplicateGroup>> {
        let mut groups = Vec::new();
        {
            let mut q = self.conn.prepare(
                "SELECT e.blake3, e.size, COUNT(*) AS n
                 FROM entries e JOIN archives a ON a.id = e.archive_id
                 WHERE e.kind = 'file' AND e.size > 0 AND e.blake3 IS NOT NULL AND a.status = 'verified' AND e.method != 'dep'
                 GROUP BY e.blake3, e.size HAVING n > 1
                 ORDER BY e.size * (n - 1) DESC
                 LIMIT ?1",
            )?;
            let rows = q.query_map(params![limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?;
            for row in rows {
                let (blake3, size) = row?;
                groups.push(DuplicateGroup { blake3, size, copies: Vec::new() });
            }
        }
        let mut q = self.conn.prepare(
            "SELECT e.id, a.id, a.name, COALESCE(a.source_display, a.source_path), e.rel_path, e.size, e.modified_utc, e.blake3, e.sha256, 0
             FROM entries e JOIN archives a ON a.id = e.archive_id
             WHERE e.blake3 = ?1 AND e.size = ?2 AND e.kind = 'file' AND a.status = 'verified' AND e.method != 'dep'
             ORDER BY a.name, e.rel_path",
        )?;
        for g in &mut groups {
            let rows = q.query_map(params![g.blake3, g.size as i64], found_file)?;
            g.copies = rows.collect::<std::result::Result<_, _>>()?;
        }
        Ok(groups)
    }
}

/// The conditions for a file to count as a copy in the duplicates tree:
/// a file with content and a hash, not a reference file, in a verified
/// archive that isn't a set-aside archive.
const COPY: &str = "e2.kind = 'file' AND e2.size > 0 AND e2.blake3 IS NOT NULL AND e2.method != 'dep'
     AND a2.status = 'verified' AND a2.source_path NOT LIKE '%|excluded'";

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

fn insert_entries(tx: &rusqlite::Transaction<'_>, archive_id: i64, entries: &[EntryRecord]) -> Result<()> {
    let mut ins = tx.prepare(
        "INSERT INTO entries (archive_id, rel_path, name, ext, kind, size, modified_utc,
            created_utc, blake3, sha256, method)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let mut fts = tx.prepare("INSERT INTO entries_fts (rowid, name, rel_path) VALUES (?1, ?2, ?3)")?;
    for e in entries {
        let name = e.rel_path.rsplit('/').next().unwrap_or(&e.rel_path).to_string();
        let ext = if e.is_dir {
            String::new()
        } else {
            name.rsplit_once('.').map(|(_, x)| x.to_lowercase()).unwrap_or_default()
        };
        ins.execute(params![
            archive_id,
            e.rel_path,
            name,
            ext,
            if e.is_dir { "dir" } else { "file" },
            e.size as i64,
            e.modified_utc,
            e.created_utc,
            e.blake3,
            e.sha256,
            e.method
        ])?;
        fts.execute(params![tx.last_insert_rowid(), name, e.rel_path])?;
    }
    Ok(())
}

/// Removes an archive's entries and their search index rows.
fn delete_entries(tx: &rusqlite::Transaction<'_>, archive_id: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO entries_fts (entries_fts, rowid, name, rel_path)
         SELECT 'delete', id, name, rel_path FROM entries WHERE archive_id = ?1",
        params![archive_id],
    )?;
    tx.execute("DELETE FROM entries WHERE archive_id = ?1", params![archive_id])?;
    Ok(())
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

fn found_file(r: &rusqlite::Row<'_>) -> rusqlite::Result<FoundFile> {
    Ok(FoundFile {
        entry_id: r.get(0)?,
        archive_id: r.get(1)?,
        archive_name: r.get(2)?,
        source_path: r.get(3)?,
        rel_path: r.get(4)?,
        size: r.get::<_, i64>(5)? as u64,
        modified_utc: r.get(6)?,
        blake3: r.get(7)?,
        sha256: r.get(8)?,
        copies: r.get::<_, i64>(9)? as u64,
    })
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
