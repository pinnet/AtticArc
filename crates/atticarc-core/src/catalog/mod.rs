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

mod records;
mod schema;

pub use records::*;
pub use schema::*;

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
    pub fn replace_contents(&mut self, id: i64, a: &ArchiveRecord, entries: &[EntryRecord], ex: &crate::archive::Excluded) -> Result<()> {
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
            let mut q =
                tx.prepare("INSERT INTO excluded (archive_id, rel_path, size, action, aside_archive) VALUES (?1, ?2, ?3, ?4, ?5)")?;
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
        let mut files =
            self.conn.prepare("SELECT rel_path, size, action, aside_archive FROM excluded WHERE archive_id = ?1 ORDER BY rel_path")?;
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
            &format!(
                "SELECT e2.id FROM entries e2 JOIN archives a2 ON a2.id = e2.archive_id
              WHERE e2.blake3 = ?1 AND e2.size = ?2 AND {COPY} ORDER BY a2.created_utc, a2.id, e2.rel_path LIMIT 1"
            ),
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
        self.conn.execute("UPDATE archives SET source_deleted_utc = ?2 WHERE id = ?1", params![archive_id, when_utc])?;
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
            .query_row("SELECT COALESCE(decoded_utc, created_utc) FROM archives WHERE id = ?1", params![archive_id], |r| r.get(0))
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
        let words: Vec<String> =
            text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| format!("\"{w}\"*")).collect();
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
            let rows = q.query_map(params![limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
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

fn insert_entries(tx: &rusqlite::Transaction<'_>, archive_id: i64, entries: &[EntryRecord]) -> Result<()> {
    let mut ins = tx.prepare(
        "INSERT INTO entries (archive_id, rel_path, name, ext, kind, size, modified_utc,
            created_utc, blake3, sha256, method)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let mut fts = tx.prepare("INSERT INTO entries_fts (rowid, name, rel_path) VALUES (?1, ?2, ?3)")?;
    for e in entries {
        let name = e.rel_path.rsplit('/').next().unwrap_or(&e.rel_path).to_string();
        let ext = if e.is_dir { String::new() } else { name.rsplit_once('.').map(|(_, x)| x.to_lowercase()).unwrap_or_default() };
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
