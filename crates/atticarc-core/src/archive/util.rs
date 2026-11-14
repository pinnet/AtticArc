//! Small helpers the other modules share.

use crate::catalog::EntryRecord;
use anyhow::{Result, bail};
use std::fs::File;
use std::io;
use std::path::Path;
use std::time::SystemTime;

pub(super) fn ext_of(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map(|(_, x)| x.to_lowercase()).unwrap_or_default()
}

pub(super) fn utc(t: Option<SystemTime>) -> Option<String> {
    t.map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn summarize(problems: &[String]) -> String {
    let mut s: Vec<String> = problems.iter().take(10).cloned().collect();
    if problems.len() > 10 {
        s.push(format!("and {} more", problems.len() - 10));
    }
    s.join("\n")
}

pub(super) fn parse_utc(t: Option<&str>) -> Option<SystemTime> {
    t.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(SystemTime::from)
}

/// Sets a file's or folder's times where the system allows; best effort.
pub(super) fn set_times(path: &Path, modified: Option<SystemTime>, created: Option<SystemTime>, is_dir: bool) {
    crate::platform::set_times(path, modified, created, is_dir)
}

pub(super) fn check_hash(path: &Path, rec: &EntryRecord) -> Result<()> {
    let mut h = blake3::Hasher::new();
    let mut f = File::open(path)?;
    io::copy(&mut f, &mut h)?;
    if rec.blake3.as_deref() != Some(h.finalize().to_hex().as_str()) {
        bail!("{} doesn't match its recorded hash", rec.rel_path);
    }
    Ok(())
}
