//! Clean up: the last step after archiving. Lists every archived folder
//! whose source is still on disk, as a tree by parent folder with tick
//! boxes, and deletes all the ticked ones in one go. Each folder still goes
//! through the full check first (its files against the catalogue, the
//! archive decoded), and a folder that fails its check is left alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};

use atticarc_core::archive::{self, Progress};
use atticarc_core::catalog::{self, Catalog};

use crate::{App, card, heading, human, theme};

/// An archived folder whose source is still on disk.
#[derive(Clone, Debug)]
struct Item {
    archive_id: i64,
    source: PathBuf,
    archive_path: PathBuf,
    archive_name: String,
    bytes: u64,
    files: u64,
    archived: String,
}

/// How one folder went.
#[derive(Clone, Debug)]
struct Outcome {
    source: PathBuf,
    /// Files a discard rule left out, deleted without being kept.
    discarded: usize,
    result: Result<(), String>,
}

struct Running {
    progress: Arc<Progress>,
    /// The folder being worked on, and its place in the run.
    current: Arc<Mutex<String>>,
    rx: Receiver<Msg>,
    total: usize,
}

enum Msg {
    One(Outcome),
    Done,
}

#[derive(Default)]
pub struct State {
    loaded: bool,
    loading: Option<Receiver<Result<Vec<Item>, String>>>,
    items: Vec<Item>,
    error: Option<String>,
    /// Ticked folders, by archive id. Nothing is ticked to start.
    ticked: BTreeSet<i64>,
    /// Parent folders that are folded shut in the tree.
    folded: BTreeSet<PathBuf>,
    confirm: bool,
    running: Option<Running>,
    /// What the last run did, folder by folder.
    outcomes: Vec<Outcome>,
    /// Set when folders were deleted, so other pages reload.
    pub changed: bool,
}

impl State {
    pub fn invalidate(&mut self) {
        if self.running.is_none() {
            self.loaded = false;
        }
    }

    pub fn busy(&self) -> bool {
        self.running.is_some() || self.loading.is_some()
    }

    fn ticked_items(&self) -> Vec<Item> {
        self.items.iter().filter(|i| self.ticked.contains(&i.archive_id)).cloned().collect()
    }

    /// The items grouped by the folder holding them, in path order.
    fn tree(&self) -> BTreeMap<PathBuf, Vec<&Item>> {
        let mut t: BTreeMap<PathBuf, Vec<&Item>> = BTreeMap::new();
        for i in &self.items {
            let parent = i.source.parent().map(Path::to_path_buf).unwrap_or_default();
            t.entry(parent).or_default().push(i);
        }
        for v in t.values_mut() {
            v.sort_by(|a, b| a.source.cmp(&b.source));
        }
        t
    }

    fn start(&mut self, settings: archive::Settings) {
        let todo = self.ticked_items();
        if todo.is_empty() {
            return;
        }
        let progress = Arc::new(Progress::default());
        let current = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = channel();
        let (p, cur, total) = (progress.clone(), current.clone(), todo.len());
        std::thread::spawn(move || {
            for (n, item) in todo.into_iter().enumerate() {
                if p.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let name = item.source.file_name().unwrap_or_default().to_string_lossy().into_owned();
                *cur.lock().unwrap() = format!("{name} ({} of {total})", n + 1);
                let _ = tx.send(Msg::One(delete_one(&item, &settings, &p)));
            }
            let _ = tx.send(Msg::Done);
        });
        self.outcomes.clear();
        self.running = Some(Running { progress, current, rx, total });
    }
}

/// Checks one folder against its archive and, if the check passes, deletes
/// it and marks it deleted in the catalogue. Never deletes after a failed
/// check, and never deletes a protected folder.
fn delete_one(item: &Item, settings: &archive::Settings, progress: &Arc<Progress>) -> Outcome {
    let started = catalog::now_utc();
    let mut discarded = 0;
    let result = (|| -> anyhow::Result<()> {
        if archive::is_protected(&item.source, settings) {
            anyhow::bail!("it is protected");
        }
        let cat = Catalog::open_existing(&settings.out_dir)?;
        let recorded = cat.entries(item.archive_id)?;
        let check = archive::check_before_delete(&item.source, &item.archive_path, &recorded, settings.threads, progress)?;
        discarded = check.discarded.len();
        progress.set_phase("Deleting the folder", 0);
        archive::delete_folder(&item.source)?;
        cat.mark_source_deleted(item.archive_id, &catalog::now_utc())?;
        Ok(())
    })()
    .map_err(|e| format!("{e:#}"));
    if let Ok(cat) = Catalog::open_existing(&settings.out_dir) {
        let outcome = if result.is_ok() { "deleted" } else { "failed" };
        let detail = result.clone().err().unwrap_or_default();
        let _ = cat.log_run(Some(item.archive_id), "delete", &item.source, &started, outcome, &detail);
    }
    Outcome { source: item.source.clone(), discarded, result }
}

/// Reads, in the background, every archive whose source folder is still on
/// disk and not marked deleted.
fn load(out: PathBuf) -> Receiver<Result<Vec<Item>, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let res = (|| -> anyhow::Result<Vec<Item>> {
            if !catalog::exists(&out) {
                return Ok(Vec::new());
            }
            let cat = Catalog::open_existing(&out)?;
            let mut items = Vec::new();
            for a in cat.archives()? {
                let source = PathBuf::from(&a.source_path);
                if a.source_deleted_utc.is_some() || !source.is_dir() {
                    continue;
                }
                let archive_path = PathBuf::from(&a.archive_path);
                items.push(Item {
                    archive_id: a.id,
                    archive_name: archive_path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    archive_path,
                    source,
                    bytes: a.input_bytes,
                    files: a.file_count,
                    archived: a.created_utc.get(..10).unwrap_or(&a.created_utc).to_string(),
                });
            }
            Ok(items)
        })()
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(res);
    });
    rx
}

impl App {
    pub fn cleanup(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let settings = self.settings();
        let others_busy = self.jobs.is_busy() || self.duplicates.busy() || self.excluded.busy() || self.catalogue.busy();
        let st = &mut self.cleanup;

        // A run in progress: collect what it reports.
        let mut finished = false;
        if let Some(run) = &st.running {
            while let Ok(m) = run.rx.try_recv() {
                match m {
                    Msg::One(o) => st.outcomes.push(o),
                    Msg::Done => finished = true,
                }
            }
        }
        if finished {
            st.running = None;
            let gone: BTreeSet<PathBuf> = st.outcomes.iter().filter(|o| o.result.is_ok()).map(|o| o.source.clone()).collect();
            st.ticked.retain(|id| st.items.iter().any(|i| i.archive_id == *id && !gone.contains(&i.source)));
            if !gone.is_empty() {
                st.changed = true;
            }
            st.loaded = false;
        }

        if !st.loaded && st.loading.is_none() && st.running.is_none() {
            st.loaded = true;
            st.error = None;
            st.loading = Some(load(out.clone()));
        }
        if let Some(rx) = &st.loading
            && let Ok(res) = rx.try_recv()
        {
            st.loading = None;
            match res {
                Ok(items) => {
                    let ids: BTreeSet<i64> = items.iter().map(|i| i.archive_id).collect();
                    st.ticked.retain(|id| ids.contains(id));
                    st.items = items;
                }
                Err(e) => {
                    st.items.clear();
                    st.error = Some(e);
                }
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            heading(
                ui,
                "Clean up",
                "Archived folders still on disk. Tick the ones to delete, then delete them all in one go. Each is checked against its archive first, and one that fails its check is left alone.",
            );
            ui.add_space(10.0);
            if let Some(e) = &st.error {
                ui.colored_label(theme::danger(), e);
            }
            if st.loading.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Reading the catalogue… (a disk that was asleep can take a few seconds to spin up)").color(theme::muted()));
                });
            }

            if let Some(run) = &st.running {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(format!("Checking and deleting {}", run.current.lock().unwrap())).strong());
                    });
                    ui.label(RichText::new(run.progress.phase.lock().unwrap().clone()).color(theme::muted()));
                    ui.add(egui::ProgressBar::new(run.progress.fraction()).desired_height(8.0).fill(theme::accent()));
                    let done = st.outcomes.len();
                    ui.add(
                        egui::ProgressBar::new(done as f32 / run.total.max(1) as f32)
                            .desired_height(8.0)
                            .fill(theme::accent())
                            .text(format!("{done} of {} folders", run.total)),
                    );
                    let stopping = run.progress.cancel.load(Ordering::Relaxed);
                    let label = if stopping { "Stopping after this folder…" } else { "Stop after this folder" };
                    if ui.add_enabled(!stopping, egui::Button::new(label)).clicked() {
                        run.progress.cancel.store(true, Ordering::Relaxed);
                    }
                });
                ui.add_space(10.0);
            }

            if !st.outcomes.is_empty() {
                outcomes(ui, &st.outcomes, st.running.is_none());
                ui.add_space(10.0);
            }

            if st.items.is_empty() {
                if st.loading.is_none() && st.error.is_none() {
                    card(ui, |ui| {
                        ui.label("No archived folders are left on disk. Everything archived here has been cleaned up.");
                    });
                }
                return;
            }

            let idle = st.running.is_none() && !others_busy;
            let ticked = st.ticked_items();
            let ticked_bytes: u64 = ticked.iter().map(|i| i.bytes).sum();
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Tick all")).clicked() {
                    st.ticked = st.items.iter().map(|i| i.archive_id).collect();
                }
                if ui.add_enabled(idle, egui::Button::new("Untick all")).clicked() {
                    st.ticked.clear();
                }
                if ui.add_enabled(st.running.is_none(), egui::Button::new("Refresh")).clicked() {
                    st.loaded = false;
                }
                ui.label(RichText::new(format!("{} of {} ticked · {}", ticked.len(), st.items.len(), human(ticked_bytes))).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let go = egui::Button::new(RichText::new(format!("Delete {} ticked…", folders(ticked.len()))).color(egui::Color32::WHITE))
                        .fill(theme::danger_fill());
                    let tip = if others_busy { "Wait for the job that is running to finish" } else { "Checks each ticked folder against its archive, then deletes it" };
                    if ui.add_enabled(idle && !ticked.is_empty(), go).on_hover_text(tip).clicked() {
                        st.confirm = true;
                    }
                });
            });
            ui.add_space(8.0);
            tree(ui, st, idle);
        });

        if st.confirm {
            confirm(ui.ctx(), st, &settings);
        }
        if st.busy() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}

/// The folders as a tree: each parent folder with a tick box for all of its
/// archived folders, which can be folded, then each archived folder.
fn tree(ui: &mut egui::Ui, st: &mut State, idle: bool) {
    let groups: Vec<(PathBuf, Vec<Item>)> = st.tree().into_iter().map(|(p, v)| (p, v.into_iter().cloned().collect())).collect();
    card(ui, |ui| {
        for (parent, items) in groups {
            let ids: Vec<i64> = items.iter().map(|i| i.archive_id).collect();
            let n_on = ids.iter().filter(|id| st.ticked.contains(id)).count();
            let open = !st.folded.contains(&parent);
            ui.horizontal(|ui| {
                let arrow = if open { "⏷" } else { "⏵" };
                if ui.add(egui::Button::new(arrow).frame(false)).on_hover_text(if open { "Fold" } else { "Unfold" }).clicked() {
                    if open {
                        st.folded.insert(parent.clone());
                    } else {
                        st.folded.remove(&parent);
                    }
                }
                let mut on = n_on == ids.len();
                let cb = egui::Checkbox::without_text(&mut on).indeterminate(n_on > 0 && n_on < ids.len());
                if ui.add_enabled(idle, cb).on_hover_text("Tick or untick every folder in here").changed() {
                    for id in &ids {
                        if on {
                            st.ticked.insert(*id);
                        } else {
                            st.ticked.remove(id);
                        }
                    }
                }
                ui.label(RichText::new(parent.display().to_string()).monospace().strong());
                let bytes: u64 = items.iter().map(|i| i.bytes).sum();
                ui.label(RichText::new(format!("{} · {} · {n_on} ticked", folders(items.len()), human(bytes))).size(12.5).color(theme::muted()));
            });
            if open {
                egui::Grid::new(("cleanup", parent.clone())).num_columns(5).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                    for i in &items {
                        ui.horizontal(|ui| {
                            ui.add_space(34.0);
                            let mut on = st.ticked.contains(&i.archive_id);
                            if ui.add_enabled(idle, egui::Checkbox::without_text(&mut on)).changed() {
                                if on {
                                    st.ticked.insert(i.archive_id);
                                } else {
                                    st.ticked.remove(&i.archive_id);
                                }
                            }
                            let name = i.source.file_name().unwrap_or_default().to_string_lossy().into_owned();
                            ui.label(RichText::new(name).monospace()).on_hover_text(i.source.display().to_string());
                        });
                        ui.label(human(i.bytes));
                        ui.label(format!("{} files", i.files));
                        ui.label(RichText::new(format!("→ {}", i.archive_name)).color(theme::muted()))
                            .on_hover_text(i.archive_path.display().to_string());
                        ui.label(RichText::new(format!("archived {}", i.archived)).color(theme::muted()));
                        ui.end_row();
                    }
                });
            }
            ui.add_space(6.0);
        }
    });
}

fn confirm(ctx: &egui::Context, st: &mut State, settings: &archive::Settings) {
    let ticked = st.ticked_items();
    let bytes: u64 = ticked.iter().map(|i| i.bytes).sum();
    let mut go = false;
    let mut close = false;
    egui::Modal::new(egui::Id::new("cleanup-confirm")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        ui.heading(format!("Delete {}?", folders(ticked.len())));
        ui.label(format!("{} of original folders will be deleted, permanently: they don't go to the Recycle Bin.", human(bytes)));
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Each folder is checked first: its files against the catalogue, and its archive decoded and checked. \
                 A folder that fails is left as it is and the rest carry on. Files a discard rule left out of an \
                 archive are deleted with their folder, without being kept.",
            )
            .color(theme::muted()),
        );
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            for i in &ticked {
                ui.label(RichText::new(i.source.display().to_string()).monospace());
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let b = egui::Button::new(RichText::new(format!("Check and delete {}", folders(ticked.len()))).color(egui::Color32::WHITE))
                .fill(theme::danger_fill());
            if ui.add(b).clicked() {
                go = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if go {
        st.confirm = false;
        st.start(settings.clone());
    } else if close {
        st.confirm = false;
    }
}

/// What a run did, folder by folder.
fn outcomes(ui: &mut egui::Ui, list: &[Outcome], finished: bool) {
    let ok = list.iter().filter(|o| o.result.is_ok()).count();
    let failed = list.len() - ok;
    card(ui, |ui| {
        let title = if finished {
            format!("Deleted {} · {failed} left alone", folders(ok))
        } else {
            format!("So far: deleted {} · {failed} left alone", folders(ok))
        };
        ui.label(RichText::new(title).strong());
        for o in list {
            let name = o.source.display().to_string();
            match &o.result {
                Ok(()) => {
                    let mut t = format!("✓ {name}");
                    if o.discarded > 0 {
                        t.push_str(&format!(" ({} discarded files deleted with it)", o.discarded));
                    }
                    ui.colored_label(theme::ok(), t);
                }
                Err(e) => {
                    ui.colored_label(theme::danger(), format!("✗ {name}: left as it is. {e}"));
                }
            }
        }
    });
}

fn folders(n: usize) -> String {
    if n == 1 { "1 folder".into() } else { format!("{n} folders") }
}
