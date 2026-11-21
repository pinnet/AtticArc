//! Progress shared between a worker and whoever shows it.

use std::io::{self};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared between a worker and the window, so the window can show progress
/// and ask the worker to stop.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    pub phase: Mutex<String>,
    /// The file being read now (path inside the folder), for "Now:" lines.
    pub current_file: Mutex<String>,
    /// With this set, every phase and every file read is also added to
    /// `events`, for a detailed log; see [`Progress::take_events`].
    pub detailed: AtomicBool,
    events: Mutex<Vec<String>>,
}

/// Events kept for a detailed log the window hasn't collected yet; past
/// this, the oldest go, so a window left closed can't use up memory.
pub(super) const EVENTS_MAX: usize = 50_000;

impl Progress {
    pub fn set_phase(&self, phase: &str, total: u64) {
        *self.phase.lock().unwrap() = phase.to_string();
        self.current_file.lock().unwrap().clear();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        if !phase.is_empty() {
            self.event(|| if total > 0 { format!("{phase} ({})", crate::disk::human(total)) } else { phase.to_string() });
        }
    }

    /// Records the file being read now, and with a detailed log, a line for it.
    pub fn set_file(&self, verb: &str, rel: &str, size: u64) {
        *self.current_file.lock().unwrap() = rel.to_string();
        self.event(|| format!("  {verb} {rel} ({})", crate::disk::human(size)));
    }

    /// Adds a line to the detailed log, if there is one.
    pub fn event(&self, line: impl FnOnce() -> String) {
        if self.detailed.load(Ordering::Relaxed) {
            let mut ev = self.events.lock().unwrap();
            if ev.len() >= EVENTS_MAX {
                ev.drain(..EVENTS_MAX / 10);
            }
            ev.push(line());
        }
    }

    /// The detailed log lines since the last call.
    pub fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 { 0.0 } else { (self.done.load(Ordering::Relaxed) as f64 / t as f64).min(1.0) as f32 }
    }

    pub(super) fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(io::Error::other("cancelled")) } else { Ok(()) }
    }
}
