//! The folder list and the background work on it: sizing, archiving a batch,
//! and the check-then-delete flow. Ported from foldarc.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};

use atticarc_core::archive::{self, DeleteCheck, Progress, Settings};
use atticarc_core::catalog::{self, ArchivedFolder, Catalog};

use crate::{human, theme};

#[derive(Clone, PartialEq)]
pub enum RowState {
    Idle,
    Queued,
    Working,
    Done(String),
    Failed(String),
}

pub struct Row {
    pub path: PathBuf,
    pub name: String,
    /// Bytes and file count, filled in by a background thread.
    pub size: Option<(u64, u64)>,
    pub ticked: bool,
    pub archived: Option<ArchivedFolder>,
    pub state: RowState,
}

enum Msg {
    Size(PathBuf, u64, u64),
    Started(PathBuf),
    Archived(PathBuf, Result<String, String>),
    BatchDone,
    Checked(PathBuf, Result<DeleteCheck, String>),
    Deleted(PathBuf, Result<(), String>),
}

pub enum Dialog {
    ConfirmDelete(PathBuf),
    Checking(PathBuf),
    CheckPassed(PathBuf, DeleteCheck),
    /// The delete is running; it can't be cancelled once started.
    Deleting(PathBuf, Instant),
    Message(String, String),
}

/// A batch of folders being archived one after another.
pub struct Batch {
    pub progress: Arc<Progress>,
    /// Finish the current folder, then stop.
    pub stop_after: Arc<AtomicBool>,
    pub current: Arc<Mutex<Option<PathBuf>>>,
    pub started: Instant,
    /// When the batch finished, so its time and throughput stop counting.
    pub ended: Option<Instant>,
    pub folders: Vec<(PathBuf, u64)>,
    pub finished: usize,
    pub finished_bytes: u64,
    pub verified: usize,
    pub failed: usize,
    pub settings_label: String,
    pub done: bool,
}

impl Batch {
    /// Seconds the batch has run, or ran in total once it has ended.
    pub fn elapsed_secs(&self) -> f64 {
        self.ended.unwrap_or_else(Instant::now).duration_since(self.started).as_secs_f64()
    }

    pub fn total_bytes(&self) -> u64 {
        self.folders.iter().map(|f| f.1).sum()
    }

    /// Overall fraction, counting compression as 80% of a folder's work
    /// and the verify pass as the other 20%.
    pub fn fraction(&self) -> f32 {
        if self.done && self.finished == self.folders.len() {
            return 1.0;
        }
        let total = self.total_bytes().max(1) as f64;
        let mut done = self.finished_bytes as f64;
        if !self.done {
            let current = self.current.lock().unwrap().clone();
            if let Some(size) = current.and_then(|c| self.folders.iter().find(|f| f.0 == c).map(|f| f.1)) {
                let phase = self.progress.phase.lock().unwrap().clone();
                let f = self.progress.fraction() as f64;
                let within = match phase.as_str() {
                    "Compressing" => 0.8 * f,
                    "Verifying" => 0.8 + 0.2 * f,
                    _ => 0.0,
                };
                done += within * size as f64;
            }
        }
        (done / total).min(1.0) as f32
    }
}

/// A single job that isn't a batch: the check before a delete, or the delete.
struct Task {
    progress: Arc<Progress>,
}

pub struct Jobs {
    pub rows: Vec<Row>,
    pub batch: Option<Batch>,
    pub log: Vec<String>,
    pub dialog: Option<Dialog>,
    pub load_error: Option<String>,
    /// Folders outside the source folder list that a delete was asked for,
    /// from the Excluded page.
    targets: Vec<(PathBuf, ArchivedFolder)>,
    /// Set when a folder was deleted, so other pages can refresh.
    pub deleted: bool,
    task: Option<Task>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Jobs {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            rows: Vec::new(),
            batch: None,
            log: Vec::new(),
            dialog: None,
            load_error: None,
            targets: Vec::new(),
            deleted: false,
            task: None,
            tx,
            rx,
        }
    }

    /// Starts the check-then-delete flow for a folder archived as `archived`,
    /// whether or not it is in the current source folder.
    pub fn ask_delete(&mut self, path: PathBuf, archived: ArchivedFolder) {
        self.targets.retain(|t| t.0 != path);
        self.targets.push((path.clone(), archived));
        self.dialog = Some(Dialog::ConfirmDelete(path));
    }

    fn archived_for(&self, path: &Path) -> Option<ArchivedFolder> {
        self.rows
            .iter()
            .find(|r| r.path == path)
            .and_then(|r| r.archived.clone())
            .or_else(|| self.targets.iter().find(|t| t.0 == path).map(|t| t.1.clone()))
    }

    pub fn is_busy(&self) -> bool {
        self.task.is_some() || self.batch.as_ref().is_some_and(|b| !b.done)
    }

    pub fn needs_repaint(&self) -> bool {
        self.is_busy() || self.rows.iter().any(|r| r.size.is_none())
    }

    fn log(&mut self, line: String) {
        let t = catalog::local_hhmm();
        self.log.push(format!("{t}  {line}"));
    }

    /// Lists the folders in `source_dir` and looks each one up in the catalogue.
    pub fn reload(&mut self, source_dir: &Path, out_dir: &Path) {
        let previous: std::collections::HashMap<PathBuf, bool> =
            self.rows.iter().map(|r| (r.path.clone(), r.ticked)).collect();
        self.rows.clear();
        self.load_error = None;
        let out_key = catalog::path_key(out_dir);
        let cat = if catalog::exists(out_dir) {
            Catalog::open_existing(out_dir).map(Some).map_err(|e| format!("{e:#}"))
        } else {
            Ok(None)
        };
        let dirs = match std::fs::read_dir(source_dir) {
            Ok(d) => d,
            Err(e) => {
                self.load_error = Some(format!("Can't list {}: {e}", source_dir.display()));
                return;
            }
        };
        for d in dirs.flatten() {
            let Ok(ft) = d.file_type() else { continue };
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            let path = d.path();
            if catalog::path_key(&path) == out_key {
                continue;
            }
            let archived = match &cat {
                Ok(Some(c)) => c.archived_folder(&path).ok().flatten(),
                _ => None,
            };
            let ticked = previous.get(&path).copied().unwrap_or(true);
            self.rows.push(Row {
                name: d.file_name().to_string_lossy().into_owned(),
                path,
                size: None,
                ticked,
                archived,
                state: RowState::Idle,
            });
        }
        if let Err(e) = cat {
            self.load_error = Some(format!("Can't open the catalogue in the destination folder: {e}"));
        }
        self.rows.sort_by_key(|r| r.name.to_lowercase());
        let paths: Vec<PathBuf> = self.rows.iter().map(|r| r.path.clone()).collect();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for p in paths {
                let (bytes, files) = archive::quick_size(&p);
                if tx.send(Msg::Size(p, bytes, files)).is_err() {
                    break;
                }
            }
        });
    }

    fn row_mut(&mut self, path: &Path) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.path == path)
    }

    /// Applies finished background work. Returns true when a batch has just
    /// ended, so catalogue views can refresh.
    pub fn handle_messages(&mut self, out_dir: &Path) -> bool {
        let mut batch_ended = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Size(p, b, f) => {
                    if let Some(r) = self.row_mut(&p) {
                        r.size = Some((b, f));
                    }
                    // A batch started before this folder was sized.
                    if let Some(entry) = self.batch.as_mut().and_then(|batch| batch.folders.iter_mut().find(|e| e.0 == p)) {
                        entry.1 = b;
                    }
                }
                Msg::Started(p) => {
                    if let Some(r) = self.row_mut(&p) {
                        r.state = RowState::Working;
                    }
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    self.log(format!("{name}: compressing"));
                }
                Msg::Archived(p, res) => {
                    let archived = Catalog::open_existing(out_dir).ok().and_then(|c| c.archived_folder(&p).ok().flatten());
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let line = match &res {
                        Ok(note) => format!("{name}.7z verified, {note}"),
                        Err(e) => format!("{name} failed: {}", e.lines().next().unwrap_or(e)),
                    };
                    if let Some(b) = &mut self.batch {
                        b.finished += 1;
                        b.finished_bytes += b.folders.iter().find(|f| f.0 == p).map(|f| f.1).unwrap_or(0);
                        if res.is_ok() { b.verified += 1 } else { b.failed += 1 }
                    }
                    if let Some(r) = self.row_mut(&p) {
                        match res {
                            Ok(note) => {
                                r.state = RowState::Done(note);
                                r.archived = archived;
                            }
                            Err(e) => r.state = RowState::Failed(e),
                        }
                    }
                    self.log(line);
                }
                Msg::BatchDone => {
                    for r in &mut self.rows {
                        if r.state == RowState::Queued {
                            r.state = RowState::Idle;
                        }
                    }
                    if let Some(b) = &mut self.batch {
                        b.done = true;
                        b.ended = Some(Instant::now());
                        let (v, f, n) = (b.verified, b.failed, b.folders.len());
                        self.log(format!("Finished: {v} verified, {f} failed, {} not started", n - v - f));
                    }
                    batch_ended = true;
                }
                Msg::Checked(p, res) => {
                    self.task = None;
                    self.dialog = Some(match res {
                        Ok(check) => Dialog::CheckPassed(p, check),
                        Err(e) => Dialog::Message("Not deleted".into(), format!("{} was not deleted.\n\n{e}", p.display())),
                    });
                }
                Msg::Deleted(p, res) => {
                    self.task = None;
                    match res {
                        Ok(()) => {
                            self.rows.retain(|r| r.path != p);
                            self.targets.retain(|t| t.0 != p);
                            self.deleted = true;
                            self.log(format!("{} deleted after a full check", p.display()));
                            self.dialog = Some(Dialog::Message(
                                "Deleted".into(),
                                format!("{} was deleted. Its archive and catalogue entry are kept.", p.display()),
                            ));
                        }
                        Err(e) => self.dialog = Some(Dialog::Message("Delete failed".into(), e)),
                    }
                }
            }
        }
        batch_ended
    }

    /// Ticked folders first, then unticked ones (greyed) at the bottom, each in
    /// name order. Archived folders are listed separately.
    pub fn ordered(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].archived.is_none()).collect();
        v.sort_by_key(|&i| (!self.rows[i].ticked, self.rows[i].name.to_lowercase()));
        v
    }

    pub fn archived_rows(&self) -> Vec<usize> {
        (0..self.rows.len()).filter(|&i| self.rows[i].archived.is_some()).collect()
    }

    /// Folders that "Start archiving" would archive, and their total size.
    pub fn selection(&self) -> (usize, u64) {
        self.rows
            .iter()
            .filter(|r| r.ticked && r.archived.is_none())
            .fold((0, 0), |(c, b), r| (c + 1, b + r.size.map(|s| s.0).unwrap_or(0)))
    }

    pub fn archive_selected(&mut self, settings: Settings) {
        let todo: Vec<(PathBuf, u64)> = self
            .ordered()
            .into_iter()
            .filter_map(|i| {
                let r = &self.rows[i];
                (r.ticked && r.archived.is_none()).then(|| (r.path.clone(), r.size.map(|s| s.0).unwrap_or(0)))
            })
            .collect();
        if todo.is_empty() {
            return;
        }
        for r in &mut self.rows {
            if todo.iter().any(|t| t.0 == r.path) {
                r.state = RowState::Queued;
            }
        }
        let batch = Batch {
            progress: Arc::new(Progress::default()),
            stop_after: Arc::new(AtomicBool::new(false)),
            current: Arc::new(Mutex::new(None)),
            started: Instant::now(),
            ended: None,
            folders: todo.clone(),
            finished: 0,
            finished_bytes: 0,
            verified: 0,
            failed: 0,
            settings_label: format!("{} · {} threads", settings.engine_label(), settings.threads),
            done: false,
        };
        self.log.clear();
        self.log(format!("Archiving {} folders to {}", todo.len(), settings.out_dir.display()));
        // Each folder is also checked on its own before it starts; this warns
        // up front when the whole selection may not fit (nothing compressing).
        let selected: u64 = todo.iter().map(|t| t.1).sum();
        let needed = atticarc_core::disk::archive_worst_case(selected).saturating_add(atticarc_core::disk::MARGIN);
        if let Some(free) = atticarc_core::disk::free_bytes(&settings.out_dir) {
            if free < needed {
                self.log(format!(
                    "Warning: {} free on the destination, but the selection is {} before compression. \
                     A folder that doesn't fit fails with a not-enough-space message and the rest carry on.",
                    human(free),
                    human(selected)
                ));
            }
        }
        let (progress, stop_after, current) = (batch.progress.clone(), batch.stop_after.clone(), batch.current.clone());
        self.batch = Some(batch);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for (p, _) in todo {
                if progress.cancel.load(Ordering::Relaxed) || stop_after.load(Ordering::Relaxed) {
                    break;
                }
                *current.lock().unwrap() = Some(p.clone());
                let _ = tx.send(Msg::Started(p.clone()));
                let res = archive::archive_folder(&p, &settings, &progress)
                    .map(|o| {
                        let mut note = format!(
                            "{} {}, {} → {}",
                            o.record.file_count,
                            if o.record.file_count == 1 { "file" } else { "files" },
                            human(o.record.input_bytes),
                            human(o.record.archive_bytes)
                        );
                        if !o.skipped_links.is_empty() {
                            note.push_str(&format!(", {} links skipped", o.skipped_links.len()));
                        }
                        if !o.excluded.set_aside.is_empty() {
                            note.push_str(&format!(", {} set aside", o.excluded.set_aside.len()));
                        }
                        if !o.excluded.discarded.is_empty() {
                            note.push_str(&format!(", {} discarded", o.excluded.discarded.len()));
                        }
                        note
                    })
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Archived(p, res));
            }
            *current.lock().unwrap() = None;
            let _ = tx.send(Msg::BatchDone);
        });
    }

    fn start_check(&mut self, path: PathBuf, settings: &Settings) {
        let Some(archived) = self.archived_for(&path) else {
            return;
        };
        let progress = Arc::new(Progress::default());
        self.task = Some(Task { progress: progress.clone() });
        self.dialog = Some(Dialog::Checking(path.clone()));
        let tx = self.tx.clone();
        let settings = settings.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let cat = Catalog::open_existing(&settings.out_dir)?;
                let recorded = cat.entries(archived.archive_id)?;
                archive::check_before_delete(&path, &archived.archive_path, &recorded, settings.threads, &progress)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Checked(path, res));
        });
    }

    fn start_delete(&mut self, path: PathBuf, settings: &Settings) {
        let Some(archived) = self.archived_for(&path) else {
            return;
        };
        // Protected folders never reach here (their X is disabled), but check again.
        if archive::is_protected(&path, settings) {
            self.dialog = Some(Dialog::Message("Not deleted".into(), format!("{} is protected.", path.display())));
            return;
        }
        self.task = Some(Task { progress: Arc::new(Progress::default()) });
        // Stays up, with a spinner, until the delete finishes: a big folder on
        // a USB disk can take a minute or more, and Explorer shows it going
        // away bit by bit until then.
        self.dialog = Some(Dialog::Deleting(path.clone(), Instant::now()));
        let tx = self.tx.clone();
        let out_dir = settings.out_dir.clone();
        std::thread::spawn(move || {
            let started = catalog::now_utc();
            let res = archive::delete_folder(&path).map_err(|e| format!("{e:#}"));
            if let Ok(cat) = Catalog::open_existing(&out_dir) {
                if res.is_ok() {
                    let _ = cat.mark_source_deleted(archived.archive_id, &catalog::now_utc());
                }
                let outcome = if res.is_ok() { "deleted" } else { "failed" };
                let detail = res.clone().err().unwrap_or_default();
                let _ = cat.log_run(Some(archived.archive_id), "delete", &path, &started, outcome, &detail);
            }
            let _ = tx.send(Msg::Deleted(path, res));
        });
    }

    pub fn dialogs(&mut self, ctx: &egui::Context, settings: &Settings) {
        let Some(dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let mut check: Option<PathBuf> = None;
        let mut delete: Option<PathBuf> = None;
        egui::Modal::new(egui::Id::unique("atticarc-dialog")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            match &dialog {
                Dialog::ConfirmDelete(p) => {
                    ui.heading("Delete this folder?");
                    ui.label(RichText::new(p.display().to_string()).monospace());
                    ui.add_space(6.0);
                    ui.label(
                        "Before anything is deleted, every file in the folder is hashed and compared with the \
                         catalogue, and the archive is decoded and checked against the same hashes. You'll be \
                         asked once more after the check.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(crate::primary_button("Check the archive")).clicked() {
                            check = Some(p.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Checking(p) => {
                    ui.heading("Checking…");
                    ui.label(RichText::new(p.display().to_string()).monospace());
                    if let Some(task) = &self.task {
                        ui.label(task.progress.phase.lock().unwrap().clone());
                        ui.add(egui::ProgressBar::new(task.progress.fraction()).show_percentage());
                        if ui.button("Cancel").clicked() {
                            task.progress.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                }
                Dialog::CheckPassed(p, c) => {
                    ui.heading("The archive checks out");
                    ui.label(format!(
                        "All {} files ({}) in {} match the catalogue, and the archive decodes to the same hashes.",
                        c.files_checked,
                        human(c.bytes_checked),
                        p.display()
                    ));
                    if let Some(a) = &c.set_aside_archive {
                        ui.add_space(6.0);
                        let name = a.file_name().unwrap_or_default().to_string_lossy();
                        ui.label(format!("Files the exclusions set aside are safe in {name}, which was checked too."));
                    } else if c.set_aside_inside {
                        ui.add_space(6.0);
                        ui.label("Files the exclusions set aside are safe in the archive inside it, which was unpacked and checked too.");
                    }
                    if !c.discarded.is_empty() {
                        ui.add_space(6.0);
                        let bytes: u64 = c.discarded.iter().map(|e| e.size).sum();
                        ui.label(
                            RichText::new(format!(
                                "{} discarded files ({}) are in no archive and will be gone for good:",
                                c.discarded.len(),
                                human(bytes)
                            ))
                            .color(theme::warn()),
                        );
                        egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                            for e in &c.discarded {
                                ui.label(RichText::new(format!("{}  ({})", e.rel_path, human(e.size))).monospace().size(12.0));
                            }
                        });
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Deleting is permanent: the folder does not go to the Recycle Bin.").strong());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let del = egui::Button::new(RichText::new("Delete the folder").color(Color32::WHITE)).fill(theme::danger_fill());
                        if ui.add(del).clicked() {
                            delete = Some(p.clone());
                            keep = false;
                        }
                        if ui.button("Keep it").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Deleting(p, started) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(22.0));
                        ui.heading("Deleting…");
                    });
                    ui.label(RichText::new(p.display().to_string()).monospace());
                    ui.add_space(6.0);
                    ui.label(format!("Still deleting after {}. This can take a while for a large folder.", crate::duration(started.elapsed().as_secs_f64())));
                    ui.label(
                        RichText::new("Leave the drive connected and AtticARC open until this says it's done.").color(theme::muted()),
                    );
                }
                Dialog::Message(title, body) => {
                    ui.heading(title);
                    ui.label(body);
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        keep = false;
                    }
                }
            }
        });
        if let Some(p) = check {
            self.start_check(p, settings);
            return;
        }
        if let Some(p) = delete {
            self.start_delete(p, settings);
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}
