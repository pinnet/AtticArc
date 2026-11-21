//! Archiving a folder that was archived before.

use crate::catalog::{Catalog, DATA_DIR, now_utc};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;

/// Inside the destination's `.atticarc` folder: archives replaced by a newer
/// archive of the same folder ([`archive_again`]).
pub const SUPERSEDED_DIR: &str = "superseded";

/// Archives `source` like [`archive_folder`], for a folder that may have
/// been archived before (to apply changed exclusions or settings, say).
/// The new archive gets a name of its own (`Name (2).7z`); only once it is
/// verified, the folder's older archive is moved into
/// `.atticarc\superseded` and marked so in the catalogue, so every list
/// shows the new one only. Nothing is deleted: the older archive can be
/// removed by hand. If it can't be moved, it stays as it was, and the new
/// archive still counts.
pub fn archive_again(source: &Path, settings: &Settings, progress: &Arc<Progress>) -> Result<ArchiveOutcome> {
    let canonical = std::fs::canonicalize(source).with_context(|| format!("can't open {}", source.display()))?;
    let old = if crate::catalog::exists(&settings.out_dir) {
        Catalog::open_existing(&settings.out_dir)?.archived_folder(&canonical)?
    } else {
        None
    };
    let mut o = archive_folder(source, settings, progress)?;
    let Some(old) = old else { return Ok(o) };
    let moved = (|| -> Result<PathBuf> {
        let out = std::fs::canonicalize(&settings.out_dir)?;
        let dir = out.join(DATA_DIR).join(SUPERSEDED_DIR);
        std::fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
        let stem = old.archive_path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let day = chrono::Local::now().format("%Y-%m-%d");
        let to = std::iter::once(dir.join(format!("{stem}.7z")))
            .chain((1..).map(|n| {
                dir.join(if n == 1 { format!("{stem} (superseded {day}).7z") } else { format!("{stem} (superseded {day}, {n}).7z") })
            }))
            .find(|p| !p.exists())
            .expect("a free name");
        std::fs::rename(&old.archive_path, &to)
            .with_context(|| format!("can't move {} to {}", old.archive_path.display(), to.display()))?;
        let to_key = crate::settings::strip_verbatim(&to).to_string_lossy().into_owned();
        let mut cat = Catalog::open_existing(&settings.out_dir)?;
        cat.supersede(old.archive_id, &to_key)?;
        let _ = cat.log_run(
            Some(old.archive_id),
            "supersede",
            &canonical,
            &now_utc(),
            "moved",
            &format!("replaced by {}", o.record.archive_path),
        );
        Ok(to)
    })();
    match moved {
        Ok(p) => o.superseded = Some(p),
        Err(e) => o.superseded_problem = Some(format!("{e:#}")),
    }
    Ok(o)
}
