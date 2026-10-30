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

/// Creates `dir/.atticarc`, hidden on Windows (the dot hides it elsewhere).
fn make_data_dir(dir: &Path) -> Result<()> {
    let data = dir.join(DATA_DIR);
    if data.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(&data).with_context(|| format!("create {}", data.display()))?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("attrib")
            .arg("+h")
            .arg(&data)
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    Ok(())
}

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
"#;

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
        Self::connect(&db_path(dir))
    }

    fn connect(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Added after the first release: the source folder as spelled on disk.
        let has_display: bool = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('archives') WHERE name = 'source_display'",
            [],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_display {
            conn.execute_batch("ALTER TABLE archives ADD COLUMN source_display TEXT")?;
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
        {
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
        }
        tx.commit()?;
        Ok(archive_id)
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

    pub fn mark_source_deleted(&self, archive_id: i64, when_utc: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE archives SET source_deleted_utc = ?2 WHERE id = ?1",
            params![archive_id, when_utc],
        )?;
        Ok(())
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
             DELETE FROM entries; DELETE FROM runs; DELETE FROM archives;",
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
                    COALESCE(c.n, 1), e.kind
             FROM entries e JOIN archives a ON a.id = e.archive_id
             LEFT JOIN (SELECT blake3, size, COUNT(*) AS n FROM entries
                        WHERE kind = 'file' AND blake3 IS NOT NULL GROUP BY blake3, size) c
                    ON c.blake3 = e.blake3 AND c.size = e.size
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
                 WHERE e.kind = 'file' AND e.size > 0 AND e.blake3 IS NOT NULL AND a.status = 'verified'
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
             WHERE e.blake3 = ?1 AND e.size = ?2 AND e.kind = 'file' AND a.status = 'verified'
             ORDER BY a.name, e.rel_path",
        )?;
        for g in &mut groups {
            let rows = q.query_map(params![g.blake3, g.size as i64], found_file)?;
            g.copies = rows.collect::<std::result::Result<_, _>>()?;
        }
        Ok(groups)
    }
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

/// Local wall-clock time as `HH:MM`, for logs shown to people.
pub fn local_hhmm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}
