//! A small log file beside the settings (`%APPDATA%\atticarc\atticarc.log`),
//! so a window that freezes or closes leaves a record: which renderer and
//! graphics card were used, display changes, and any warnings from the
//! renderer. Kept short: a log over 1 MB is moved to `atticarc.log.old`
//! when AtticARC starts.

use std::io::Write;
use std::sync::Mutex;

use atticarc_core::{catalog, settings};

struct FileLog(Mutex<std::fs::File>);

/// Info from AtticARC and the parts of eframe that report the renderer and
/// graphics card; only warnings and errors from everything else.
fn wanted(m: &log::Metadata<'_>) -> bool {
    let t = m.target();
    m.level() <= log::Level::Warn
        || (m.level() <= log::Level::Info && (t.starts_with("atticarc") || t.starts_with("eframe") || t.starts_with("egui_wgpu")))
}

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        wanted(m)
    }

    fn log(&self, r: &log::Record<'_>) {
        if wanted(r.metadata())
            && let Ok(mut f) = self.0.lock()
        {
            let _ = writeln!(f, "{} {:<5} {}: {}", catalog::now_utc(), r.level(), r.target(), r.args());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.0.lock() {
            let _ = f.flush();
        }
    }
}

/// Starts logging to the file. Does nothing if it can't be opened.
pub fn start() {
    let path = settings::settings_file().with_file_name("atticarc.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    if log::set_boxed_logger(Box::new(FileLog(Mutex::new(file)))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}
