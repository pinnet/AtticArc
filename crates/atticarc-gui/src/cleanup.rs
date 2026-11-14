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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};

use atticarc_core::archive::{self, Progress};
use atticarc_core::catalog::{self, Catalog, SavedCheck, path_key};
use atticarc_core::cleanup::{self as clean, Candidate, Missing};

use crate::{App, card, heading, human, log_file, theme};

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
    /// Decoding archives again to catch damage (reads only the archives).
    Scrub,
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
    /// A line for the log.
    Log(String),
    Checked(SavedCheck),
    Deleted(i64, Outcome),
    /// How many archives a scrub will decode, once it has listed them.
    Total(usize),
    /// One archive decoded again: its name and what was found.
    Scrubbed(String, Result<(), String>),
    Done,
}

/// The archived folders on disk, those that aren't, and the saved ticks.
type Loaded = (Vec<Candidate>, Vec<Missing>, Vec<i64>);

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
    /// Ticked folders, by archive id. Nothing is ticked to start; ticks are
    /// saved in the catalogue, so they survive a restart.
    ticked: BTreeSet<i64>,
    /// The ticks as last saved, to know when to save them again.
    saved_ticks: BTreeSet<i64>,
    /// Parent folders folded shut in the tree.
    folded: BTreeSet<PathBuf>,
    /// The latest check of each folder, by archive id: read back from the
    /// catalogue when the page loads, so checks survive a restart.
    checks: BTreeMap<i64, SavedCheck>,
    confirm: bool,
    running: Option<Running>,
    /// What the last delete pass did, folder by folder.
    outcomes: Vec<Outcome>,
    /// What the runs this session did, line by line, for the log window.
    log: Vec<String>,
    show_log: bool,
    /// Set when folders were deleted, so other pages reload.
    pub changed: bool,
    /// Archives a scrub found damaged this session: name and what was wrong.
    damaged: Vec<(String, String)>,
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

    /// Ticked folders; a protected folder can't be ticked, and doesn't count if it was.
    fn ticked_items(&self) -> Vec<Candidate> {
        self.items.iter().filter(|i| !i.protected && self.ticked.contains(&i.archive_id)).cloned().collect()
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

    /// Pass 1: checks ticked folders: those not yet passed (so a run carries
    /// on where it stopped), or with `all`, every one again. Deletes nothing.
    fn start_check(&mut self, settings: archive::Settings, all: bool) {
        let todo: Vec<Candidate> =
            self.ticked_items().into_iter().filter(|i| all || !self.checks.get(&i.archive_id).is_some_and(|c| c.passed)).collect();
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
            say(&tx, format!("Checking {} (nothing is deleted)", folders(total)));
            for (n, item) in todo.into_iter().enumerate() {
                if p.cancel.load(Ordering::Relaxed) {
                    say(&tx, "Stopped, as asked".to_string());
                    break;
                }
                *cur.lock().unwrap() = format!("{} ({} of {total})", short(&item.source), n + 1);
                say(&tx, format!("{} of {total}: checking {}", n + 1, item.source.display()));
                let c = clean::check(&item, &settings, &p);
                say(&tx, if c.passed { "   ✓ passed".to_string() } else { format!("   ✗ failed: {}", c.detail) });
                let _ = tx.send(Msg::Checked(c));
            }
            say(&tx, "Check finished".to_string());
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
            say(&tx, format!("Deleting {}", folders(total)));
            for (n, item) in todo.into_iter().enumerate() {
                if p.cancel.load(Ordering::Relaxed) {
                    say(&tx, "Stopped, as asked".to_string());
                    break;
                }
                *cur.lock().unwrap() = format!("{} ({} of {total})", short(&item.source), n + 1);
                say(&tx, format!("{} of {total}: {} ({})", n + 1, item.source.display(), human(item.bytes)));
                let discarded = item.check.as_ref().map_or(0, |c| c.discarded);
                let result = clean::delete(&item, &settings, &p).map_err(|e| format!("{e:#}"));
                say(&tx, match &result {
                    Ok(()) => "   ✓ deleted".to_string(),
                    Err(e) => format!("   ✗ left alone: {e}"),
                });
                let _ = tx.send(Msg::Deleted(item.archive_id, Outcome { source: item.source.clone(), discarded, result }));
            }
            say(&tx, "Delete finished".to_string());
            let _ = tx.send(Msg::Done);
        });
        self.outcomes.clear();
        self.running = Some(run);
    }

    /// Decodes archives again to catch damage: those last decoded more than
    /// [`clean::DECODE_AGE_DAYS`] days ago, or with `all`, every one. Reads
    /// only the archives; each one's date is saved as it passes, so a stopped
    /// run carries on where it left off.
    fn start_scrub(&mut self, settings: archive::Settings, all: bool) {
        let (tx, rx) = channel();
        let run = Self::new_run(Pass::Scrub, 0, rx);
        let (p, cur) = (run.progress.clone(), run.current.clone());
        std::thread::spawn(move || {
            let todo = match clean::scrub_list(&settings.out_dir, !all) {
                Ok(t) => t,
                Err(e) => {
                    say(&tx, format!("Can't read the catalogue: {e:#}"));
                    let _ = tx.send(Msg::Done);
                    return;
                }
            };
            let total = todo.len();
            let _ = tx.send(Msg::Total(total));
            if total == 0 {
                say(&tx, format!("No archive is due: all decoded within {} days", clean::DECODE_AGE_DAYS));
            } else {
                say(&tx, format!("Decoding {} again (reads only the archives)", archives(total)));
            }
            for (n, item) in todo.into_iter().enumerate() {
                if p.cancel.load(Ordering::Relaxed) {
                    say(&tx, "Stopped, as asked".to_string());
                    break;
                }
                *cur.lock().unwrap() = format!("{} ({} of {total})", item.name, n + 1);
                say(&tx, format!("{} of {total}: {} ({})", n + 1, item.archive_path.display(), human(item.archive_bytes)));
                let result = clean::scrub_one(&item, &settings, &p).map_err(|e| format!("{e:#}"));
                if result.is_err() && p.cancel.load(Ordering::Relaxed) {
                    say(&tx, "Stopped, as asked (that archive wasn't finished, so its date is unchanged)".to_string());
                    break;
                }
                say(&tx, match &result {
                    Ok(()) => "   ✓ decoded and matched".to_string(),
                    Err(e) => format!("   ✗ DAMAGED: {e}"),
                });
                let _ = tx.send(Msg::Scrubbed(item.name.clone(), result));
            }
            say(&tx, "Archive check finished".to_string());
            let _ = tx.send(Msg::Done);
        });
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
fn load(out: PathBuf, renamed: BTreeMap<i64, PathBuf>, protected: Vec<PathBuf>) -> Receiver<Result<Loaded, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let res = clean::candidates(&out, &renamed, &protected).and_then(|(c, m)| Ok((c, m, clean::ticked(&out)?)));
        let _ = tx.send(res.map_err(|e| format!("{e:#}")));
    });
    rx
}

impl App {
    pub fn cleanup(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let settings = self.settings();
        // Only work that changes things elsewhere holds Clean up back, and the
        // page says which.
        let mut waiting = Vec::new();
        if self.jobs.is_busy() {
            waiting.push("the archive job");
        }
        if self.duplicates.working() {
            waiting.push("replacing duplicates");
        }
        if self.excluded.busy() {
            waiting.push("the Excluded page");
        }
        if self.catalogue.working() {
            waiting.push("the Catalogue page (extracting or rebuilding)");
        }
        let others_busy = !waiting.is_empty();
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
                Msg::Log(line) => st.log.push(line),
                Msg::Checked(c) => {
                    st.checks.insert(c.archive_id, c);
                    if let Some(r) = &mut st.running {
                        r.done += 1;
                    }
                }
                Msg::Total(n) => {
                    if let Some(r) = &mut st.running {
                        r.total = n;
                    }
                }
                Msg::Scrubbed(name, result) => {
                    if let Err(e) = result {
                        st.damaged.push((name, e));
                    }
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
            if pass == Pass::Scrub {
                // Dates changed: the "decoded again before deleting" notes follow.
                st.loaded = false;
            }
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
            st.loading = Some(load(out.clone(), st.renamed.clone(), settings.protected.clone()));
        }
        if let Some(rx) = &st.loading
            && let Ok(res) = rx.try_recv()
        {
            st.loading = None;
            match res {
                Ok((items, gone, saved_ticks)) => {
                    let ids: BTreeSet<i64> = items.iter().map(|i| i.archive_id).collect();
                    // The saved ticks are the record: they survive a restart.
                    let protected: BTreeSet<i64> = items.iter().filter(|i| i.protected).map(|i| i.archive_id).collect();
                    st.ticked = saved_ticks.into_iter().filter(|id| ids.contains(id) && !protected.contains(id)).collect();
                    st.saved_ticks = st.ticked.clone();
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

        // Ticks changed last frame: save them, in the background.
        if st.loading.is_none() && st.loaded && st.ticked != st.saved_ticks {
            st.saved_ticks = st.ticked.clone();
            let (dir, ids): (PathBuf, Vec<i64>) = (out.clone(), st.ticked.iter().copied().collect());
            std::thread::spawn(move || {
                let _ = clean::set_ticked(&dir, &ids);
            });
        }

        // The run in progress stays in view above the list, however far down
        // it is scrolled.
        if let Some(run) = &st.running {
            running(ui, run, &mut st.show_log);
            ui.add_space(8.0);
        } else if !st.log.is_empty() {
            ui.horizontal(|ui| {
                if ui.button("Show log").clicked() {
                    st.show_log = true;
                }
            });
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            heading(
                ui,
                "Clean up",
                "Archived folders still on disk. Folders archived on NTFS are checked as they are archived, so they can be deleted \
                 straight away: just before each delete, AtticARC makes sure the folder and its archive haven't changed (seconds). \
                 Others need step 1, the full check. Nothing is deleted without a passed check.",
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
            if !st.outcomes.is_empty() {
                outcomes(ui, &st.outcomes, st.running.is_none());
                ui.add_space(10.0);
            }
            gone(ui, st, &out, others_busy);
            scrub_row(ui, st, &settings, others_busy);

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
            let to_check = ticked.iter().filter(|i| !st.checks.get(&i.archive_id).is_some_and(|c| c.passed)).count();
            let ready = st.ready().len();
            let failed = ticked.iter().filter(|i| st.checks.get(&i.archive_id).is_some_and(|c| !c.passed)).count();
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Tick all")).clicked() {
                    st.ticked = st.items.iter().filter(|i| !i.protected).map(|i| i.archive_id).collect();
                }
                if ui.add_enabled(idle, egui::Button::new("Untick all")).clicked() {
                    st.ticked.clear();
                }
                if ui.add_enabled(st.running.is_none() && st.loading.is_none(), egui::Button::new("Refresh")).clicked() {
                    st.loaded = false;
                }
                let protected = st.items.iter().filter(|i| i.protected).count();
                ui.label(RichText::new(format!("{} of {} ticked · {}", ticked.len(), st.items.len() - protected, human(ticked_bytes))).strong());
                if protected > 0 {
                    ui.label(RichText::new(format!("· {protected} protected")).color(theme::muted()))
                        .on_hover_text("Folders in the protected list (New job page) are never checked or deleted.");
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let tip = if others_busy {
                    "Wait for what is running elsewhere to finish"
                } else {
                    "Step 1: checks each ticked folder against the catalogue and its archive. Deletes nothing."
                };
                let label = if to_check == ticked.len() {
                    format!("1. Check {} ticked", folders(to_check))
                } else {
                    format!("1. Check the {} not yet passed", folders(to_check))
                };
                if ui.add_enabled(idle && to_check > 0, egui::Button::new(label)).on_hover_text(tip).clicked() {
                    st.start_check(settings.clone(), false);
                }
                let again = to_check < ticked.len();
                if again
                    && ui
                        .add_enabled(idle, egui::Button::new("Check all ticked again"))
                        .on_hover_text("Checks every ticked folder again, including those that already passed")
                        .clicked()
                {
                    st.start_check(settings.clone(), true);
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
            if others_busy {
                ui.colored_label(theme::warn(), format!("Waiting for {} to finish before checking or deleting.", waiting.join(", ")));
            } else if st.searching.is_some() {
                ui.colored_label(theme::warn(), "Waiting for the search for renamed folders to finish.");
            }
            ui.add_space(8.0);
            tree(ui, st, idle);
        });

        if st.confirm {
            confirm(ui.ctx(), st, &settings);
        }
        if st.show_log {
            log_window(ui.ctx(), st);
        }
        if st.busy() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}

/// Checking the archives themselves for damage, and what earlier runs found.
fn scrub_row(ui: &mut egui::Ui, st: &mut State, settings: &archive::Settings, others_busy: bool) {
    let idle = st.running.is_none() && st.searching.is_none() && !others_busy;
    ui.horizontal(|ui| {
        let tip = format!(
            "Decodes again every archive last decoded more than {} days ago and checks each file against the catalogue, \
             to catch damage since it was made (a bad sector, bit rot). Reads only the archives; can be stopped and carried on.",
            clean::DECODE_AGE_DAYS
        );
        if ui.add_enabled(idle, egui::Button::new("Check archives for damage")).on_hover_text(tip).clicked() {
            st.start_scrub(settings.clone(), false);
        }
        if ui
            .add_enabled(idle, egui::Button::new("…all of them"))
            .on_hover_text("Decodes every archive again, however recently it was decoded")
            .clicked()
        {
            st.start_scrub(settings.clone(), true);
        }
    });
    if !st.damaged.is_empty() {
        card(ui, |ui| {
            ui.colored_label(theme::danger(), format!("{} damaged: keep the folders they came from.", archives(st.damaged.len())));
            for (name, e) in &st.damaged {
                ui.label(format!("{name}: {}", e.lines().next().unwrap_or("")));
            }
        });
    }
    ui.add_space(8.0);
}

/// The run in progress: which pass, which folder, and how far.
fn running(ui: &mut egui::Ui, run: &Running, show_log: &mut bool) {
    card(ui, |ui| {
        let what = match run.pass {
            Pass::Check => "Checking (nothing is deleted)",
            Pass::Delete => "Deleting",
            Pass::Scrub => "Checking archives for damage (reads only the archives)",
        };
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(format!("{what}: {}", run.current.lock().unwrap())).strong());
        });
        let phase = run.progress.phase.lock().unwrap().clone();
        let (d, t) = (run.progress.done.load(Ordering::Relaxed), run.progress.total.load(Ordering::Relaxed));
        let phase = if t > 0 { format!("{phase}: {d} of {t}") } else { format!("{phase}…") };
        ui.label(RichText::new(phase).color(theme::muted()));
        ui.add(egui::ProgressBar::new(run.progress.fraction()).desired_height(8.0).fill(theme::accent()));
        ui.add(
            egui::ProgressBar::new(run.done as f32 / run.total.max(1) as f32)
                .desired_height(8.0)
                .fill(theme::accent())
                .text(if run.pass == Pass::Scrub {
                    format!("{} of {}", run.done, archives(run.total))
                } else {
                    format!("{} of {} folders", run.done, run.total)
                }),
        );
        let stopping = run.progress.cancel.load(Ordering::Relaxed);
        let label = match (stopping, run.pass == Pass::Scrub) {
            (true, true) => "Stopping…",
            (false, true) => "Stop",
            (true, false) => "Stopping after this folder…",
            (false, false) => "Stop after this folder",
        };
        ui.horizontal(|ui| {
            if ui.add_enabled(!stopping, egui::Button::new(label)).clicked() {
                run.progress.cancel.store(true, Ordering::Relaxed);
            }
            if ui.button("Show log").clicked() {
                *show_log = true;
            }
        });
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
            // A protected folder has no tick box, so the parent's box leaves it out.
            let ids: Vec<i64> = items.iter().filter(|i| !i.protected).map(|i| i.archive_id).collect();
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
                let mut on = !ids.is_empty() && n_on == ids.len();
                let cb = egui::Checkbox::without_text(&mut on).indeterminate(n_on > 0 && n_on < ids.len());
                if ui.add_enabled(idle && !ids.is_empty(), cb).on_hover_text("Tick or untick every folder in here").changed() {
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
                            if i.protected {
                                // Same width as a tick box, so the names line up.
                                ui.add_enabled(false, egui::Button::new("🔒").frame(false))
                                    .on_disabled_hover_text("Protected: never checked or deleted. Change the list on the New job page.");
                            } else {
                                let mut on = st.ticked.contains(&i.archive_id);
                                if ui.add_enabled(idle, egui::Checkbox::without_text(&mut on)).changed() {
                                    if on {
                                        st.ticked.insert(i.archive_id);
                                    } else {
                                        st.ticked.remove(&i.archive_id);
                                    }
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
                            _ if i.protected => {
                                ui.label(RichText::new("protected · never deleted").color(theme::muted()))
                                    .on_hover_text("In one of the protected folders (New job → Protected folders). Take it off that list to check and delete it.");
                            }
                            Some(c) if c.passed => {
                                let mut t = format!("✓ passed {}", when(&c.checked_utc));
                                if c.snapshot.as_deref().is_some_and(archive::snapshot_used_journal) {
                                    t.push_str(" · change journal");
                                }
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

/// Sends a line for the log window, and writes it to the log file.
fn say(tx: &Sender<Msg>, line: String) {
    log::info!(target: "atticarc::cleanup", "{line}");
    let _ = tx.send(Msg::Log(format!("{}  {line}", catalog::local_hhmmss())));
}

/// The log of this session's checks and deletes, newest at the bottom.
fn log_window(ctx: &egui::Context, st: &mut State) {
    let mut open = true;
    egui::Window::new("Clean up log").open(&mut open).default_size([640.0, 420.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(st.log.join("\n"));
            }
            if ui.button("Open the log file").on_hover_text(log_file::path().display().to_string()).clicked() {
                log_file::open();
            }
            if let Some(run) = &st.running {
                ui.spinner();
                ui.label(RichText::new(run.current.lock().unwrap().clone()).color(theme::muted()));
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if st.log.is_empty() {
                ui.label(RichText::new("Nothing has been checked or deleted yet this session.").color(theme::muted()));
            }
            for line in &st.log {
                let colour = if line.contains('✗') {
                    theme::danger()
                } else if line.contains('✓') {
                    theme::ok()
                } else {
                    ui.visuals().text_color()
                };
                ui.label(RichText::new(line).monospace().color(colour));
            }
        });
    });
    if !open {
        st.show_log = false;
    }
}

/// A folder's own name.
fn short(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn archives(n: usize) -> String {
    if n == 1 { "1 archive".to_string() } else { format!("{n} archives") }
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
