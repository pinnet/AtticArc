//! Clean up: the last step after archiving. Lists every archived folder
//! whose source is still on disk, as a tree by parent folder with tick
//! boxes, and deletes the ticked ones in two separate passes, so nothing is
//! deleted until every ticked folder has been checked:
//!
//! 1. **Check**: each ticked folder against the catalogue and its archive
//!    (every file hashed, the archive decoded). Nothing is deleted. The
//!    result of each is shown, and a quick snapshot of each folder that
//!    passes is kept.
//! 2. **Delete**: only folders that passed, after a yes. Just before each
//!    is deleted its snapshot is taken again; a folder that changed since
//!    its check is left alone.
//!
//! Checks are saved in the catalogue (see `atticarc_core::cleanup`), so they
//! survive a restart: a long run can be checked over several sessions and
//! deleted later. Archived folders that are no longer where they were can
//! be looked for among the folders beside them, by their files: a renamed
//! folder is listed under its new name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};

use atticarc_core::archive::{self, Progress};
use atticarc_core::catalog::{Catalog, SavedCheck, path_key};
use atticarc_core::cleanup::{self as clean, Candidate, Missing};

use crate::{App, card, heading, human, theme};

/// How a folder's delete went.
#[derive(Clone, Debug)]
struct Outcome {
    source: PathBuf,
    discarded: usize,
    result: Result<(), String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Check,
    Delete,
}

struct Running {
    pass: Pass,
    progress: Arc<Progress>,
    /// The folder being worked on, and its place in the run.
    current: Arc<Mutex<String>>,
    rx: Receiver<Msg>,
    total: usize,
    done: usize,
}

enum Msg {
    Checked(SavedCheck),
    Deleted(i64, Outcome),
    Done,
}

type Loaded = (Vec<Candidate>, Vec<Missing>);

#[derive(Default)]
pub struct State {
    loaded: bool,
    loading: Option<Receiver<Result<Loaded, String>>>,
    items: Vec<Candidate>,
    /// Archived folders no longer where they were.
    gone: Vec<Missing>,
    /// Renamed folders found by "Look for renamed folders", by archive id.
    renamed: BTreeMap<i64, PathBuf>,
    searching: Option<Receiver<BTreeMap<i64, PathBuf>>>,
    /// What the last search found, to show.
    search_note: Option<String>,
    error: Option<String>,
    /// Ticked folders, by archive id. Nothing is ticked to start.
    ticked: BTreeSet<i64>,
    /// Parent folders folded shut in the tree.
    folded: BTreeSet<PathBuf>,
    /// The latest check of each folder, by archive id: read back from the
    /// catalogue when the page loads, so checks survive a restart.
    checks: BTreeMap<i64, SavedCheck>,
    confirm: bool,
    running: Option<Running>,
    /// What the last delete pass did, folder by folder.
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
        self.running.is_some() || self.loading.is_some() || self.searching.is_some()
    }

    fn ticked_items(&self) -> Vec<Candidate> {
        self.items.iter().filter(|i| self.ticked.contains(&i.archive_id)).cloned().collect()
    }

    /// Ticked folders that passed their check, each with that check.
    fn ready(&self) -> Vec<Candidate> {
        self.ticked_items()
            .into_iter()
            .filter_map(|mut i| {
                let c = self.checks.get(&i.archive_id).filter(|c| c.passed)?.clone();
                i.check = Some(c);
                Some(i)
            })
            .collect()
    }

    /// The items grouped by the folder holding them, in path order.
    fn tree(&self) -> BTreeMap<PathBuf, Vec<Candidate>> {
        let mut t: BTreeMap<PathBuf, Vec<Candidate>> = BTreeMap::new();
        for i in &self.items {
            let parent = i.source.parent().map(Path::to_path_buf).unwrap_or_default();
            t.entry(parent).or_default().push(i.clone());
        }
        for v in t.values_mut() {
            v.sort_by(|a, b| a.source.cmp(&b.source));
        }
        t
    }

    fn new_run(pass: Pass, total: usize, rx: Receiver<Msg>) -> Running {
        Running { pass, progress: Arc::new(Progress::default()), current: Arc::new(Mutex::new(String::new())), rx, total, done: 0 }
    }

    /// Pass 1: checks every ticked folder. Deletes nothing.
    fn start_check(&mut self, settings: archive::Settings) {
        let todo = self.ticked_items();
        if todo.is_empty() {
            return;
        }
        for i in &todo {
            self.checks.remove(&i.archive_id);
        }
        let (tx, rx) = channel();
        let run = Self::new_run(Pass::Check, todo.len(), rx);
        let (p, cur) = (run.progress.clone(), run.current.clone());
        std::thread::spawn(move || {
            let total = todo.len();
            for (n, item) in todo.into_iter().enumerate() {
                if p.cancel.load(Ordering::Relaxed) {
                    break;
                }
                *cur.lock().unwrap() = format!("{} ({} of {total})", short(&item.source), n + 1);
                let _ = tx.send(Msg::Checked(clean::check(&item, &settings, &p)));
            }
            let _ = tx.send(Msg::Done);
        });
        self.running = Some(run);
    }

    /// Pass 2: deletes the ticked folders that passed their check, each only
    /// if it is unchanged since.
    fn start_delete(&mut self, settings: archive::Settings) {
        let todo = self.ready();
        if todo.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        let run = Self::new_run(Pass::Delete, todo.len(), rx);
        let (p, cur) = (run.progress.clone(), run.current.clone());
        std::thread::spawn(move || {
            let total = todo.len();
            for (n, item) in todo.into_iter().enumerate() {
                if p.cancel.load(Ordering::Relaxed) {
                    break;
                }
                *cur.lock().unwrap() = format!("{} ({} of {total})", short(&item.source), n + 1);
                let discarded = item.check.as_ref().map_or(0, |c| c.discarded);
                let result = clean::delete(&item, &settings, &p).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Deleted(item.archive_id, Outcome { source: item.source.clone(), discarded, result }));
            }
            let _ = tx.send(Msg::Done);
        });
        self.outcomes.clear();
        self.running = Some(run);
    }

    /// Looks, in the background, for the folders no longer where they were.
    fn start_search(&mut self, out: PathBuf) {
        let gone = self.gone.clone();
        let known: BTreeSet<String> = self.items.iter().map(|i| path_key(&i.source)).collect();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut found = BTreeMap::new();
            let mut finder = archive::RenameFinder::default();
            if let Ok(cat) = Catalog::open_existing(&out) {
                // Folders already accounted for are never taken for another's.
                let mut taken = known;
                for g in &gone {
                    let Ok(entries) = cat.entries(g.archive_id) else { continue };
                    let hit = {
                        let skip = |p: &Path| taken.contains(&path_key(p));
                        finder.find(&g.source, &g.archive_path, &entries, &skip)
                    };
                    if let Some(p) = hit {
                        taken.insert(path_key(&p));
                        found.insert(g.archive_id, p);
                    }
                }
            }
            let _ = tx.send(found);
        });
        self.searching = Some(rx);
        self.search_note = None;
    }
}

/// Reads, in the background, the archived folders and their saved checks.
fn load(out: PathBuf, renamed: BTreeMap<i64, PathBuf>) -> Receiver<Result<Loaded, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(clean::candidates(&out, &renamed).map_err(|e| format!("{e:#}")));
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
        let mut finished = None;
        let mut msgs = Vec::new();
        if let Some(run) = &st.running {
            while let Ok(m) = run.rx.try_recv() {
                msgs.push(m);
            }
        }
        for m in msgs {
            match m {
                Msg::Checked(c) => {
                    st.checks.insert(c.archive_id, c);
                    if let Some(r) = &mut st.running {
                        r.done += 1;
                    }
                }
                Msg::Deleted(id, o) => {
                    if o.result.is_ok() {
                        st.checks.remove(&id);
                        st.ticked.remove(&id);
                    }
                    st.outcomes.push(o);
                    if let Some(r) = &mut st.running {
                        r.done += 1;
                    }
                }
                Msg::Done => finished = st.running.as_ref().map(|r| r.pass),
            }
        }
        if let Some(pass) = finished {
            st.running = None;
            if pass == Pass::Delete {
                if st.outcomes.iter().any(|o| o.result.is_ok()) {
                    st.changed = true;
                }
                st.loaded = false;
            }
        }
        if let Some(rx) = &st.searching
            && let Ok(found) = rx.try_recv()
        {
            st.searching = None;
            st.search_note = Some(match found.len() {
                0 => "No renamed folders found: none of the folders beside them holds exactly their files.".to_string(),
                n => format!("Found {n} renamed, listed under their new names. Check them before deleting, as usual."),
            });
            st.renamed.extend(found);
            st.loaded = false;
        }

        if !st.loaded && st.loading.is_none() && st.running.is_none() {
            st.loaded = true;
            st.error = None;
            st.loading = Some(load(out.clone(), st.renamed.clone()));
        }
        if let Some(rx) = &st.loading
            && let Ok(res) = rx.try_recv()
        {
            st.loading = None;
            match res {
                Ok((items, gone)) => {
                    let ids: BTreeSet<i64> = items.iter().map(|i| i.archive_id).collect();
                    st.ticked.retain(|id| ids.contains(id));
                    // The catalogue's saved checks are the record: they survive a restart.
                    st.checks = items.iter().filter_map(|i| i.check.clone().map(|c| (i.archive_id, c))).collect();
                    st.items = items;
                    st.gone = gone;
                }
                Err(e) => {
                    st.items.clear();
                    st.gone.clear();
                    st.error = Some(e);
                }
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            heading(
                ui,
                "Clean up",
                "Archived folders still on disk. Tick the ones to delete, check them, then delete the ones that pass. Nothing is deleted until every ticked folder has been checked.",
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
                running(ui, run);
                ui.add_space(10.0);
            }
            if !st.outcomes.is_empty() {
                outcomes(ui, &st.outcomes, st.running.is_none());
                ui.add_space(10.0);
            }
            gone(ui, st, &out, others_busy);

            if st.items.is_empty() {
                if st.loading.is_none() && st.error.is_none() {
                    card(ui, |ui| {
                        ui.label("No archived folders are left on disk. Everything archived here has been cleaned up.");
                    });
                }
                return;
            }

            let idle = st.running.is_none() && st.searching.is_none() && !others_busy;
            let ticked = st.ticked_items();
            let ticked_bytes: u64 = ticked.iter().map(|i| i.bytes).sum();
            let unchecked = ticked.iter().filter(|i| !st.checks.contains_key(&i.archive_id)).count();
            let ready = st.ready().len();
            let failed = ticked.iter().filter(|i| st.checks.get(&i.archive_id).is_some_and(|c| !c.passed)).count();
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Tick all")).clicked() {
                    st.ticked = st.items.iter().map(|i| i.archive_id).collect();
                }
                if ui.add_enabled(idle, egui::Button::new("Untick all")).clicked() {
                    st.ticked.clear();
                }
                if ui.add_enabled(st.running.is_none() && st.loading.is_none(), egui::Button::new("Refresh")).clicked() {
                    st.loaded = false;
                }
                ui.label(RichText::new(format!("{} of {} ticked · {}", ticked.len(), st.items.len(), human(ticked_bytes))).strong());
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let tip = if others_busy {
                    "Wait for the job that is running to finish"
                } else {
                    "Step 1: checks each ticked folder against the catalogue and its archive. Deletes nothing."
                };
                let label = if unchecked == ticked.len() { format!("1. Check {} ticked", folders(ticked.len())) } else { format!("1. Check {} ticked again", folders(ticked.len())) };
                if ui.add_enabled(idle && !ticked.is_empty(), egui::Button::new(label)).on_hover_text(tip).clicked() {
                    st.start_check(settings.clone());
                }
                let go = egui::Button::new(RichText::new(format!("2. Delete {} that passed…", folders(ready))).color(egui::Color32::WHITE)).fill(theme::danger_fill());
                let tip = "Step 2: deletes only ticked folders that passed their check, each only if it hasn't changed since. Checks are saved, so this can be done in a later session.";
                if ui.add_enabled(idle && ready > 0, go).on_hover_text(tip).clicked() {
                    st.confirm = true;
                }
                let mut status = Vec::new();
                if unchecked > 0 {
                    status.push(format!("{unchecked} not checked yet"));
                }
                if failed > 0 {
                    status.push(format!("{failed} failed their check and won't be deleted"));
                }
                if !status.is_empty() {
                    ui.label(RichText::new(status.join(" · ")).color(theme::muted()));
                }
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

/// The run in progress: which pass, which folder, and how far.
fn running(ui: &mut egui::Ui, run: &Running) {
    card(ui, |ui| {
        let what = match run.pass {
            Pass::Check => "Checking (nothing is deleted)",
            Pass::Delete => "Deleting",
        };
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(format!("{what}: {}", run.current.lock().unwrap())).strong());
        });
        ui.label(RichText::new(run.progress.phase.lock().unwrap().clone()).color(theme::muted()));
        ui.add(egui::ProgressBar::new(run.progress.fraction()).desired_height(8.0).fill(theme::accent()));
        ui.add(
            egui::ProgressBar::new(run.done as f32 / run.total.max(1) as f32)
                .desired_height(8.0)
                .fill(theme::accent())
                .text(format!("{} of {} folders", run.done, run.total)),
        );
        let stopping = run.progress.cancel.load(Ordering::Relaxed);
        let label = if stopping { "Stopping after this folder…" } else { "Stop after this folder" };
        if ui.add_enabled(!stopping, egui::Button::new(label)).clicked() {
            run.progress.cancel.store(true, Ordering::Relaxed);
        }
    });
}

/// Archived folders no longer where they were, with a search for renamed ones.
fn gone(ui: &mut egui::Ui, st: &mut State, out: &Path, others_busy: bool) {
    if st.gone.is_empty() && st.search_note.is_none() {
        return;
    }
    card(ui, |ui| {
        if !st.gone.is_empty() {
            let n = st.gone.len();
            ui.label(
                RichText::new(format!(
                    "{} archived {} no longer where {} when archived, and not marked deleted.",
                    folders(n),
                    if n == 1 { "is" } else { "are" },
                    if n == 1 { "it was" } else { "they were" }
                ))
                .strong(),
            );
            ui.label(
                RichText::new(
                    "Renamed? AtticARC can look among the folders beside each one for a folder holding exactly its files. \
                     This lists every file in those folders (no contents are read), so it can take a while on a big disk.",
                )
                .color(theme::muted()),
            );
            ui.horizontal(|ui| {
                let searching = st.searching.is_some();
                let can = !searching && st.running.is_none() && !others_busy;
                if ui.add_enabled(can, egui::Button::new("Look for renamed folders")).clicked() {
                    st.start_search(out.to_path_buf());
                }
                if searching {
                    ui.spinner();
                    ui.label(RichText::new("Looking…").color(theme::muted()));
                }
            });
            egui::CollapsingHeader::new("Show them").id_salt("cleanup-gone").show(ui, |ui| {
                for g in &st.gone {
                    ui.label(RichText::new(g.source.display().to_string()).monospace().color(theme::muted()));
                }
            });
        }
        if let Some(n) = &st.search_note {
            ui.label(RichText::new(n).color(theme::muted()));
        }
    });
    ui.add_space(10.0);
}

/// The folders as a tree: each parent folder with a tick box for all of its
/// archived folders, which can be folded, then each archived folder with its
/// check.
fn tree(ui: &mut egui::Ui, st: &mut State, idle: bool) {
    let groups = st.tree();
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
                egui::Grid::new(("cleanup", parent.clone())).num_columns(4).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
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
                            ui.label(RichText::new(short(&i.source)).monospace()).on_hover_text(i.source.display().to_string());
                            if let Some(old) = &i.renamed_from {
                                ui.label(RichText::new(format!("(renamed from {old})")).color(theme::warn()));
                            }
                        });
                        ui.label(human(i.bytes));
                        ui.label(RichText::new(format!("→ {} · archived {}", i.archive_name(), i.archived)).color(theme::muted()))
                            .on_hover_text(i.archive_path.display().to_string());
                        match st.checks.get(&i.archive_id) {
                            Some(c) if c.passed => {
                                let mut t = format!("✓ passed {}", when(&c.checked_utc));
                                if c.discarded > 0 {
                                    t.push_str(&format!(" · {} discarded files go with it", c.discarded));
                                }
                                ui.colored_label(theme::ok(), t);
                            }
                            Some(c) => {
                                let first = c.detail.lines().next().unwrap_or(&c.detail).to_string();
                                ui.colored_label(theme::danger(), format!("✗ {first}")).on_hover_text(format!("Checked {}\n{}", when(&c.checked_utc), c.detail));
                            }
                            None => {
                                let t = if st.ticked.contains(&i.archive_id) { "not checked yet" } else { "" };
                                ui.label(RichText::new(t).color(theme::muted()));
                            }
                        }
                        ui.end_row();
                    }
                });
            }
            ui.add_space(6.0);
        }
    });
}

fn confirm(ctx: &egui::Context, st: &mut State, settings: &archive::Settings) {
    let ready = st.ready();
    let bytes: u64 = ready.iter().map(|i| i.bytes).sum();
    let discarded: usize = ready.iter().filter_map(|i| i.check.as_ref().map(|c| c.discarded)).sum();
    let skipped = st.ticked_items().len().saturating_sub(ready.len());
    let mut go = false;
    let mut close = false;
    egui::Modal::new(egui::Id::unique("cleanup-confirm")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        ui.heading(format!("Delete {}?", folders(ready.len())));
        ui.label(format!(
            "These passed their check. {} of original folders will be deleted permanently: they don't go to the Recycle Bin.",
            human(bytes)
        ));
        if skipped > 0 {
            ui.label(RichText::new(format!("{} ticked but not passed (not checked, or failed) won't be touched.", folders(skipped))).color(theme::muted()));
        }
        if discarded > 0 {
            ui.label(RichText::new(format!("{discarded} files a discard rule left out of their archives go with them, without being kept.")).color(theme::warn()));
        }
        ui.label(
            RichText::new("Just before each folder is deleted it is compared with how it was when checked; if anything has changed, it is left alone.")
                .color(theme::muted()),
        );
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            for i in &ready {
                ui.label(RichText::new(i.source.display().to_string()).monospace());
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let b = egui::Button::new(RichText::new(format!("Delete {}", folders(ready.len()))).color(egui::Color32::WHITE)).fill(theme::danger_fill());
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
        st.start_delete(settings.clone());
    } else if close {
        st.confirm = false;
    }
}

/// What the delete pass did, folder by folder.
fn outcomes(ui: &mut egui::Ui, list: &[Outcome], finished: bool) {
    let ok = list.iter().filter(|o| o.result.is_ok()).count();
    let failed = list.len() - ok;
    card(ui, |ui| {
        let title = if finished { format!("Deleted {} · {failed} left alone", folders(ok)) } else { format!("So far: deleted {} · {failed} left alone", folders(ok)) };
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
                    ui.colored_label(theme::danger(), format!("✗ {name}: left as it is, {e}"));
                }
            }
        }
    });
}

/// A folder's own name.
fn short(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn folders(n: usize) -> String {
    if n == 1 { "1 folder".into() } else { format!("{n} folders") }
}

/// When a check was made, for showing: "on 2026-11-10 at 21:40" (UTC as recorded).
fn when(utc: &str) -> String {
    match (utc.get(..10), utc.get(11..16)) {
        (Some(d), Some(t)) => format!("on {d} at {t}"),
        _ => utc.to_string(),
    }
}
