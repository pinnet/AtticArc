//! Duplicates: files with the same size and BLAKE3 hash across all archives.
//!
//! On the right, a tree of every archive, oldest first. The oldest copy of
//! each file is the original and stays; every later copy is a leaf under
//! its archive, shown with its full path and where its original is. On the
//! left, the details of what is selected.
//!
//! Replace duplicates opens the list of every leaf, none ticked: only the
//! copies ticked there (or in a copy's details) are replaced. From there a preview (a dry run)
//! checks every original against its hash and changes nothing, and Replace
//! works through the archives oldest first, swapping each ticked copy for a
//! small `.atticarcdep` reference file that points to its original. The
//! Catalogue follows those references and can put the files back.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};

use atticarc_core::archive::{self, DEP_EXT, ReplaceReport};
use atticarc_core::catalog::{self, Catalog, TreeArchive, TreeLeaf};

use crate::{App, card, heading, human, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sel {
    Archive(i64),
    /// A leaf, by catalogue entry id.
    Leaf(i64),
}

/// A leaf by archive and path, which stays the same when an archive is
/// rewritten and its entries get new ids.
type Key = (i64, String);

fn key(a: &TreeArchive, l: &TreeLeaf) -> Key {
    (a.id, l.rel_path.clone())
}

#[derive(Default)]
pub struct State {
    loaded: bool,
    tree: Vec<TreeArchive>,
    error: Option<String>,
    selected: Option<Sel>,
    /// Archives opened in the tree.
    expanded: BTreeSet<i64>,
    /// Copies ticked in the list to replace. Nothing is ticked until picked,
    /// so a copy is only ever replaced by choice.
    chosen: BTreeSet<Key>,
    /// Only copies of at least `min_mb` megabytes are replaced.
    min_on: bool,
    min_mb: f64,
    review: bool,
    running: Option<Running>,
    /// The last preview, shown in the list until the ticks change.
    preview: Option<(BTreeSet<Key>, Result<ReplaceReport, String>)>,
    /// What the last run did, and whether it worked.
    note: Option<(bool, String)>,
    /// The tree being read from the catalogue in the background, so a slow
    /// or sleeping disk never freezes the window.
    loading: Option<Receiver<Result<Vec<TreeArchive>, String>>>,
}

struct Running {
    dry_run: bool,
    /// The ticks the run is for.
    ticked: BTreeSet<Key>,
    progress: Arc<archive::Progress>,
    rx: Receiver<Result<ReplaceReport, String>>,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }

    pub fn busy(&self) -> bool {
        self.running.is_some() || self.loading.is_some()
    }

    /// Whether duplicates are being replaced. Reading the list doesn't count
    /// (see `catalogue::State::working`).
    pub fn working(&self) -> bool {
        self.running.is_some()
    }

    /// The copies shown: those passing the size limit, if one is set.
    fn shown(&self) -> impl Iterator<Item = (&TreeArchive, &TreeLeaf)> {
        self.leaves().filter(|(_, l)| self.big_enough(l))
    }

    fn leaves(&self) -> impl Iterator<Item = (&TreeArchive, &TreeLeaf)> {
        self.tree.iter().flat_map(|a| a.leaves.iter().map(move |l| (a, l)))
    }

    /// The copies ticked to replace.
    fn ticked(&self) -> Vec<(&TreeArchive, &TreeLeaf)> {
        self.leaves().filter(|(a, l)| self.big_enough(l) && self.chosen.contains(&key(a, l))).collect()
    }

    /// Whether a copy passes the size limit, if one is set.
    fn big_enough(&self, l: &TreeLeaf) -> bool {
        !self.min_on || l.size as f64 >= self.min_mb * 1_000_000.0
    }

    /// The copies below the size limit, and their total size.
    fn too_small(&self) -> (usize, u64) {
        self.leaves().filter(|(_, l)| !self.big_enough(l)).fold((0, 0), |(n, b), (_, l)| (n + 1, b + l.size))
    }

    fn ticked_keys(&self) -> BTreeSet<Key> {
        self.ticked().into_iter().map(|(a, l)| key(a, l)).collect()
    }

    fn start(&mut self, out: std::path::PathBuf, settings: archive::Settings, dry_run: bool) {
        let ids: Vec<i64> = self.ticked().iter().map(|(_, l)| l.entry_id).collect();
        let ticked = self.ticked_keys();
        let (tx, rx) = channel();
        let progress = Arc::new(archive::Progress::default());
        let p = progress.clone();
        std::thread::spawn(move || {
            let _ = tx.send(archive::replace_duplicates(&out, &ids, dry_run, &settings, &p).map_err(|e| format!("{e:#}")));
        });
        if !dry_run {
            self.note = None;
        }
        self.running = Some(Running { dry_run, ticked, progress, rx });
    }

    fn find_leaf(&self, id: i64) -> Option<(&TreeArchive, &TreeLeaf)> {
        self.leaves().find(|(_, l)| l.entry_id == id)
    }

    /// Opens and selects the archive holding an original.
    fn show_archive(&mut self, id: i64) {
        self.selected = Some(Sel::Archive(id));
        self.expanded.insert(id);
    }
}

impl App {
    pub fn duplicates(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let settings = self.settings();
        let busy = self.jobs.is_busy() || self.catalogue.working();
        let st = &mut self.duplicates;
        let mut changed = false;
        if let Some(run) = &st.running
            && let Ok(res) = run.rx.try_recv()
        {
            let run = st.running.take().unwrap();
            if run.dry_run {
                st.preview = Some((run.ticked, res));
            } else {
                st.note = Some(match res {
                    Ok(r) => (
                        true,
                        format!(
                            "Replaced {} ({}) with reference files in {}. Every original passed its hash check first, \
                             and each archive was rewritten and verified.",
                            copies(r.files),
                            human(r.bytes),
                            archives(r.archives.len())
                        ),
                    ),
                    Err(e) => (false, format!("Replace stopped: {e}")),
                });
                st.preview = None;
                st.loaded = false;
                changed = true;
            }
        }
        if st.min_mb == 0.0 && !st.min_on {
            st.min_mb = 200.0;
        }
        if !st.loaded && st.loading.is_none() {
            st.loaded = true;
            st.error = None;
            let (tx, rx) = channel();
            let dir = out.clone();
            std::thread::spawn(move || {
                let res = if catalog::exists(&dir) {
                    // Archives renamed in Explorer are found again first, so they aren't taken for missing originals.
                    let _ = archive::relink_missing(&dir);
                    Catalog::open_existing(&dir).and_then(|c| c.dedupe_tree()).map_err(|e| format!("{e:#}"))
                } else {
                    Ok(Vec::new())
                };
                let _ = tx.send(res);
            });
            st.loading = Some(rx);
        }
        if let Some(rx) = &st.loading
            && let Ok(res) = rx.try_recv()
        {
            st.loading = None;
            match res {
                Ok(t) => st.tree = t,
                Err(e) => {
                    st.tree.clear();
                    st.error = Some(e);
                }
            }
            let keys: BTreeSet<Key> = st.leaves().map(|(a, l)| key(a, l)).collect();
            st.chosen.retain(|k| keys.contains(k));
            let ok = match st.selected {
                Some(Sel::Leaf(id)) => st.find_leaf(id).is_some(),
                Some(Sel::Archive(id)) => st.tree.iter().any(|a| a.id == id),
                None => true,
            };
            if !ok {
                st.selected = None;
            }
        }
        if st.loading.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        // Counts follow the size limit, so changing it updates the page at once.
        let n_leaves = st.shown().count();
        let reclaimable: u64 = st.shown().map(|(_, l)| l.size).sum();
        let (n_small, small_bytes) = st.too_small();
        let replaced: u64 = st.tree.iter().map(|a| a.replaced).sum();
        let running = st.running.as_ref().filter(|r| !r.dry_run).map(|r| r.progress.clone());

        heading(
            ui,
            "Duplicates",
            "Every archive, oldest first. The oldest copy of each file is the original; each later copy is listed under its \
             archive with the space it takes. Copies inside archives are shown, not removed: taking one out means rewriting the \
             whole archive, which takes hours.",
        );
        ui.horizontal(|ui| {
            let stats = [
                ("Archives", st.tree.len().to_string()),
                ("Duplicate copies", n_leaves.to_string()),
                ("Space they take", human(reclaimable)),
                ("Already replaced", replaced.to_string()),
            ];
            for (label, value) in stats {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).size(13.0).color(theme::muted()));
                    ui.label(RichText::new(value).size(19.0).strong());
                });
                ui.add_space(24.0);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let idle = st.running.is_none() && !busy;
                // Replacing copies inside archives is switched off (show only):
                // each archive rewrite took hours. `review` is kept for a later
                // batched mode.
                if ui.add_enabled(idle && st.loading.is_none(), egui::Button::new("Refresh")).clicked() {
                    st.loaded = false;
                }
                ui.add_space(12.0);
                ui.add_enabled_ui(idle, |ui| size_limit(ui, st));
            });
        });
        if st.min_on && n_small > 0 {
            ui.label(
                RichText::new(format!(
                    "{} ({}) smaller than {} MB hidden by the size limit.",
                    copies(n_small),
                    human(small_bytes),
                    st.min_mb
                ))
                .color(theme::muted()),
            );
        }
        if st.loading.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("Reading the catalogue… (a disk that was asleep can take a few seconds to spin up)")
                        .color(theme::muted()),
                );
            });
        }
        ui.add_space(8.0);
        if let Some(p) = &running {
            ui.horizontal(|ui| {
                ui.label(RichText::new(p.phase.lock().unwrap().clone()).color(theme::muted()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let stop = !p.cancel.load(Ordering::Relaxed);
                    let tip = "Stops after the archive being rewritten is left as it was; archives already done stay done";
                    if ui.add_enabled(stop, egui::Button::new(RichText::new("Stop").color(theme::danger()))).on_hover_text(tip).clicked() {
                        p.cancel.store(true, Ordering::Relaxed);
                    }
                });
            });
            ui.add(egui::ProgressBar::new(p.fraction()).desired_height(10.0).fill(theme::accent()));
            ui.add_space(6.0);
        }
        if let Some((ok, text)) = &st.note {
            ui.colored_label(if *ok { theme::ok() } else { theme::danger() }, text);
            ui.add_space(6.0);
        }
        if let Some(e) = &st.error {
            ui.colored_label(theme::danger(), e);
        }
        if st.tree.is_empty() {
            if st.loading.is_none() {
                card(ui, |ui| {
                    ui.label("No archives in the catalogue.");
                });
            }
            return;
        }

        let w = ui.available_width();
        let h = ui.available_height();
        let gap = ui.spacing().item_spacing.x;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(w * 0.40 - gap, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                card(ui, |ui| details(ui, st))
            });
            ui.allocate_ui_with_layout(egui::vec2(w * 0.60, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                card(ui, |ui| tree_view(ui, st))
            });
        });

        if st.review {
            review(ui.ctx(), st, &out, &settings);
        }
        if changed {
            self.catalogue.invalidate();
            self.excluded.invalidate();
        }
    }
}

/// One row of the tree: an archive, or a leaf as (archive, leaf) indexes.
#[derive(Clone, Copy)]
enum Row {
    Archive(usize),
    Leaf(usize, usize),
}

const ROW_H: f32 = 46.0;

/// The right pane: every archive, oldest first, each opening to the later
/// copies it holds. Only the rows in view are drawn, so long lists stay quick.
fn tree_view(ui: &mut egui::Ui, st: &mut State) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Archives, oldest first").strong());
        ui.label(RichText::new("· open one to see its duplicates").size(12.5).color(theme::muted()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close all").clicked() {
                st.expanded.clear();
            }
            if ui.button("Open all").clicked() {
                st.expanded = st.tree.iter().filter(|a| a.leaves.iter().any(|l| st.big_enough(l))).map(|a| a.id).collect();
            }
        });
    });
    ui.add_space(2.0);
    let mut rows = Vec::new();
    for (i, a) in st.tree.iter().enumerate() {
        rows.push(Row::Archive(i));
        if st.expanded.contains(&a.id) {
            rows.extend(a.leaves.iter().enumerate().filter(|(_, l)| st.big_enough(l)).map(|(j, _)| Row::Leaf(i, j)));
        }
    }
    let mut toggle = None;
    let mut select = None;
    egui::ScrollArea::vertical().id_salt("dup-tree").auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (n, row) in rows[range.clone()].iter().enumerate() {
            let stripe = (range.start + n) % 2 == 1;
            match *row {
                Row::Archive(i) => {
                    let a = &st.tree[i];
                    let on = st.selected == Some(Sel::Archive(a.id));
                    let visible = a.leaves.iter().any(|l| st.big_enough(l));
                    let resp = row_frame(ui, stripe, on, 0.0, |ui| {
                        let open = st.expanded.contains(&a.id);
                        let arrow = if !visible { " " } else if open { "⏷" } else { "⏵" };
                        ui.add_sized([18.0, 20.0], egui::Label::new(RichText::new(arrow).color(theme::muted())).selectable(false));
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{}.7z", a.name)).strong().color(theme::ink()));
                                ui.label(RichText::new(date(&a.created_utc)).size(12.5).color(theme::muted()));
                            });
                            ui.label(RichText::new(archive_summary(a, st)).size(12.5).color(if !visible {
                                theme::muted()
                            } else {
                                theme::warn()
                            }));
                        });
                    });
                    // Clicking an archive selects it and opens or closes it.
                    if resp.clicked() {
                        select = Some(Sel::Archive(a.id));
                        if visible {
                            toggle = Some(a.id);
                        }
                    }
                }
                Row::Leaf(i, j) => {
                    let (a, l) = (&st.tree[i], &st.tree[i].leaves[j]);
                    let on = st.selected == Some(Sel::Leaf(l.entry_id));
                    let chosen = st.chosen.contains(&key(a, l));
                    let resp = row_frame(ui, stripe, on, 44.0, |ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                path_label(ui, &l.rel_path, false);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(human(l.size)).size(12.5).color(theme::muted()));
                                });
                            });
                            let text = format!("original: {}.7z › {}", l.original_archive, l.original_path);
                            let colour = if chosen { theme::accent_text() } else { theme::muted() };
                            ui.add(egui::Label::new(RichText::new(text).size(12.5).color(colour)).truncate())
                                .on_hover_text(format!("The original stays in {}.7z at\n{}", l.original_archive, l.original_path));
                        });
                    });
                    if resp.clicked() {
                        select = Some(Sel::Leaf(l.entry_id));
                    }
                }
            }
        }
    });
    if let Some(id) = toggle
        && !st.expanded.remove(&id)
    {
        st.expanded.insert(id);
    }
    if let Some(s) = select {
        st.selected = Some(s);
    }
}

/// A striped, clickable tree row of fixed height, indented by `indent`.
fn row_frame(ui: &mut egui::Ui, stripe: bool, selected: bool, indent: f32, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let fill = if selected {
        theme::accent().gamma_multiply(0.28)
    } else if stripe {
        theme::neutral_soft().gamma_multiply(0.6)
    } else {
        egui::Color32::TRANSPARENT
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
    ui.painter().rect_filled(rect, egui::CornerRadius::same(4), fill);
    if selected {
        let bar = egui::Rect::from_min_size(rect.min + egui::vec2(indent, 6.0), egui::vec2(3.0, ROW_H - 12.0));
        ui.painter().rect_filled(bar, egui::CornerRadius::same(2), theme::accent_text());
    }
    let inner = rect.shrink2(egui::vec2(8.0, 3.0));
    let inner = egui::Rect::from_min_max(inner.min + egui::vec2(indent, 0.0), inner.max);
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
        // Labels that can't be selected let the click through to the row.
        ui.style_mut().interaction.selectable_labels = false;
        add(ui)
    });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A path inside an archive: its folders muted, the file name stronger,
/// cut short from the left to fit, with the whole path in the hover.
fn path_label(ui: &mut egui::Ui, rel: &str, struck: bool) {
    let (folder, name) = rel.rsplit_once('/').map(|(f, n)| (format!("{f}/"), n)).unwrap_or((String::new(), rel));
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::monospace(13.0);
    let fmt = |colour: egui::Color32| egui::TextFormat {
        font_id: font.clone(),
        color: colour,
        strikethrough: if struck { egui::Stroke::new(1.0, colour) } else { egui::Stroke::NONE },
        ..Default::default()
    };
    job.append(&folder, 0.0, fmt(theme::muted()));
    job.append(name, 0.0, fmt(if struck { theme::muted() } else { theme::ink() }));
    job.wrap = egui::text::TextWrapping { max_rows: 1, break_anywhere: true, ..Default::default() };
    // Leave room for the size on the right.
    job.wrap.max_width = (ui.available_width() - 100.0).max(60.0);
    job.halign = egui::Align::LEFT;
    let tip = if struck { format!("{rel}\nUnticked: stays as it is") } else { rel.to_string() };
    ui.add(egui::Label::new(job)).on_hover_text(tip);
}

/// An archive's line in the tree; duplicates count only copies passing the size limit.
fn archive_summary(a: &TreeArchive, st: &State) -> String {
    if a.set_aside {
        return "set-aside files · managed on the Excluded page".into();
    }
    let mut parts = Vec::new();
    let shown: Vec<&TreeLeaf> = a.leaves.iter().filter(|l| st.big_enough(l)).collect();
    if shown.is_empty() {
        let hidden = a.leaves.len();
        parts.push(if hidden == 0 { "no duplicates".to_string() } else { format!("no duplicates over the size limit ({hidden} smaller)") });
    } else {
        let bytes: u64 = shown.iter().map(|l| l.size).sum();
        parts.push(format!("{} · {}", copies(shown.len()), human(bytes)));
    }
    if a.originals > 0 {
        parts.push(format!("{} kept here", if a.originals == 1 { "1 original".into() } else { format!("{} originals", a.originals) }));
    }
    if a.replaced > 0 {
        parts.push(format!("{} already replaced", a.replaced));
    }
    if a.referenced > 0 {
        parts.push(format!("{} referenced from later archives", if a.referenced == 1 { "1 original".into() } else { format!("{} originals", a.referenced) }));
    }
    parts.join(" · ")
}

/// The left pane: what is selected in the tree.
fn details(ui: &mut egui::Ui, st: &mut State) {
    let grid = |ui: &mut egui::Ui, id: &str, rows: Vec<(&str, String)>| {
        egui::Grid::new(id).num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            for (k, v) in rows {
                ui.label(RichText::new(k).color(theme::muted()));
                ui.add(egui::Label::new(RichText::new(v).monospace().size(13.0)).wrap());
                ui.end_row();
            }
        });
    };
    match st.selected {
        Some(Sel::Leaf(id)) => {
            let Some((a, l)) = st.find_leaf(id).map(|(a, l)| (a.clone(), l.clone())) else { return };
            let name = l.rel_path.rsplit('/').next().unwrap_or(&l.rel_path).to_string();
            ui.label(RichText::new(&name).monospace().strong().size(16.0));
            ui.label(RichText::new("A later copy, which can be replaced").color(theme::muted()));
            ui.add_space(6.0);
            let mut rows = vec![
                ("This copy", format!("{}.7z › {}", a.name, l.rel_path)),
                ("Original, stays", format!("{}.7z › {}", l.original_archive, l.original_path)),
                ("Becomes", format!("{}.{DEP_EXT}", l.rel_path)),
                ("Size", human(l.size)),
                ("Modified", l.modified_utc.clone().unwrap_or_default().replace('T', " ").replace('Z', " UTC")),
                ("BLAKE3", l.blake3.clone()),
            ];
            if let Some(sha) = &l.sha256 {
                rows.push(("SHA-256", sha.clone()));
            }
            grid(ui, "dup-leaf", rows);
            ui.add_space(10.0);
            let k = key(&a, &l);
            let mut on = st.chosen.contains(&k);
            let locked = st.running.is_some();
            if ui
                .add_enabled(!locked, egui::Checkbox::new(&mut on, "Replace this copy"))
                .on_hover_text("Tick to replace this copy with a reference file when the duplicates are replaced; unticked, it stays as it is")
                .changed()
            {
                if on {
                    st.chosen.insert(k);
                } else {
                    st.chosen.remove(&k);
                }
            }
            if ui.button(format!("Show {}.7z", l.original_archive)).on_hover_text("Open the archive holding the original in the tree").clicked() {
                st.show_archive(l.original_archive_id);
            }
        }
        Some(Sel::Archive(id)) => {
            let Some(a) = st.tree.iter().find(|a| a.id == id).cloned() else { return };
            ui.label(RichText::new(format!("{}.7z", a.name)).monospace().strong().size(16.0));
            ui.add_space(6.0);
            grid(
                ui,
                "dup-archive",
                vec![
                    ("Archive file", a.archive_path.clone()),
                    ("Original folder", a.source_path.clone()),
                    ("Archived", a.created_utc.replace('T', " ").replace('Z', " UTC")),
                    ("Files", a.file_count.to_string()),
                    ("Originals kept here", a.originals.to_string()),
                    ("Later copies here", format!("{} · {}", a.leaves.len(), human(a.leaves.iter().map(|l| l.size).sum()))),
                    ("Already replaced", a.replaced.to_string()),
                    ("Referenced originals", a.referenced.to_string()),
                ],
            );
            if a.set_aside {
                ui.add_space(8.0);
                ui.label(RichText::new("This archive holds files an exclusion set aside. They are managed on the Excluded page, so they are never originals or replaced here.").color(theme::muted()));
            }
        }
        None => {
            ui.label(RichText::new("How this works").strong().size(16.0));
            ui.add_space(4.0);
            for line in [
                "The archives on the right run from the oldest to the newest.",
                "The first copy of a file, in the oldest archive, is the original. It always stays.",
                "Every later copy is listed under its archive, with its path inside it and where its original is. Click one to see its details.",
                "Replace duplicates lists every copy with none ticked; tick the ones to replace (or Tick all). Preview checks every original and changes nothing.",
                "Replace checks every original against its hash, then rewrites each archive, oldest first, putting a small .atticarcdep file where each copy was. It names the original's archive and path, and the Catalogue can follow it or put the file back.",
            ] {
                ui.label(RichText::new(format!("•  {line}")).color(theme::ink()));
            }
        }
    }
}

/// The list before a run: every copy, none ticked unless ticked before,
/// grouped by archive oldest first, with a preview and the run itself.
fn review(ctx: &egui::Context, st: &mut State, out: &std::path::Path, settings: &archive::Settings) {
    let mut close = false;
    let mut start = None;
    let screen = ctx.content_rect();
    let width = (screen.width() - 120.0).clamp(480.0, 1100.0);
    let list_h = (screen.height() - 330.0).max(160.0);
    let ticked = st.ticked();
    let (n, bytes) = (ticked.len(), ticked.iter().map(|(_, l)| l.size).sum::<u64>());
    let n_archives = ticked.iter().map(|(a, _)| a.id).collect::<BTreeSet<_>>().len();
    let total = st.leaves().filter(|(_, l)| st.big_enough(l)).count();
    let previewing = st.running.as_ref().filter(|r| r.dry_run).map(|r| r.progress.clone());
    let keys = st.ticked_keys();
    egui::Modal::new(egui::Id::unique("replace-duplicates")).show(ctx, |ui| {
        ui.set_width(width);
        ui.heading("Replace duplicates with reference files");
        ui.label(RichText::new(
            "Each ticked copy is replaced inside its archive by a small .atticarcdep file naming the archive and path of its \
             original, which stays. Nothing is ticked to start: tick each copy to replace, or Tick all.",
        ).color(theme::muted()));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let locked = previewing.is_some();
            ui.add_enabled_ui(!locked, |ui| size_limit(ui, st));
            let (small, small_bytes) = st.too_small();
            if small > 0 {
                ui.label(RichText::new(format!("{} smaller ({}) not shown, left as they are", copies(small), human(small_bytes))).color(theme::muted()));
            }
        });
        ui.horizontal(|ui| {
            let locked = previewing.is_some();
            if ui.add_enabled(!locked, egui::Button::new("Tick all")).clicked() {
                let all: Vec<Key> = st.leaves().filter(|(_, l)| st.big_enough(l)).map(|(a, l)| key(a, l)).collect();
                st.chosen.extend(all);
            }
            if ui.add_enabled(!locked, egui::Button::new("Untick all")).clicked() {
                st.chosen.clear();
            }
            ui.label(RichText::new(format!("{n} of {total} ticked · {} · {}", human(bytes), archives(n_archives))).strong());
        });
        ui.add_space(4.0);
        egui::Frame::NONE.stroke(egui::Stroke::new(1.0, theme::border())).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
            review_list(ui, st, list_h, previewing.is_some());
        });
        ui.add_space(8.0);
        // The preview, while it runs and once it is done for these ticks.
        if let Some(p) = &previewing {
            ui.label(RichText::new(p.phase.lock().unwrap().clone()).color(theme::muted()));
            ui.add(egui::ProgressBar::new(p.fraction()).desired_height(10.0).fill(theme::accent()));
        } else if let Some((for_keys, res)) = &st.preview
            && *for_keys == keys
        {
            match res {
                Ok(r) => ui.colored_label(
                    theme::ok(),
                    format!(
                        "Preview: all {} originals were found and passed their hash check. Replacing would free {} across {}. Nothing was changed.",
                        r.originals_checked,
                        human(r.bytes),
                        archives(r.archives.len())
                    ),
                ),
                Err(e) => ui.colored_label(theme::danger(), format!("Preview found a problem: {e}")),
            };
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let idle = st.running.is_none();
            let go = egui::Button::new(RichText::new(format!("Replace {}", copies(n))).color(egui::Color32::WHITE)).fill(theme::danger_fill());
            let tip = "Checks every original, then rewrites each archive oldest first. Each archive is verified before it replaces the old one, so this needs free space for the largest archive.";
            if ui.add_enabled(idle && n > 0, go).on_hover_text(tip).clicked() {
                start = Some(false);
                close = true;
            }
            let tip = "Decodes every original from its archive and checks its hash. Nothing is changed.";
            if ui.add_enabled(idle && n > 0, egui::Button::new("Preview (dry run)")).on_hover_text(tip).clicked() {
                start = Some(true);
            }
            if let Some(p) = &previewing {
                if ui.button("Stop the preview").clicked() {
                    p.cancel.store(true, Ordering::Relaxed);
                }
            } else if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if let Some(dry_run) = start {
        st.start(out.to_path_buf(), settings.clone(), dry_run);
    }
    if close {
        st.review = false;
    }
}

/// The rows of the list before a run: an archive heading with a tick box for
/// all its copies, then each copy with its own.
fn review_list(ui: &mut egui::Ui, st: &mut State, height: f32, locked: bool) {
    let mut rows = Vec::new();
    for (i, a) in st.tree.iter().enumerate() {
        let shown: Vec<usize> = (0..a.leaves.len()).filter(|&j| st.big_enough(&a.leaves[j])).collect();
        if !shown.is_empty() {
            rows.push(Row::Archive(i));
            rows.extend(shown.into_iter().map(|j| Row::Leaf(i, j)));
        }
    }
    let mut flip: Option<(Vec<Key>, bool)> = None;
    let row_h = 26.0;
    egui::ScrollArea::vertical().id_salt("dup-review").max_height(height).auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
        for row in &rows[range] {
            ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                match *row {
                    Row::Archive(i) => {
                        let a = &st.tree[i];
                        let keys: Vec<Key> = a.leaves.iter().filter(|l| st.big_enough(l)).map(|l| key(a, l)).collect();
                        let n_on = keys.iter().filter(|k| st.chosen.contains(*k)).count();
                        let mut on = n_on == keys.len();
                        let cb = egui::Checkbox::without_text(&mut on).indeterminate(n_on > 0 && n_on < keys.len());
                        if ui.add_enabled(!locked, cb).on_hover_text("Tick or untick every copy in this archive").changed() {
                            flip = Some((keys.clone(), on));
                        }
                        ui.label(RichText::new(format!("{}.7z", a.name)).strong());
                        ui.label(RichText::new(format!("{} · {} of {} ticked", date(&a.created_utc), n_on, keys.len())).size(12.5).color(theme::muted()));
                    }
                    Row::Leaf(i, j) => {
                        let (a, l) = (&st.tree[i], &st.tree[i].leaves[j]);
                        let k = key(a, l);
                        let mut on = st.chosen.contains(&k);
                        ui.add_space(22.0);
                        if ui.add_enabled(!locked, egui::Checkbox::without_text(&mut on)).changed() {
                            flip = Some((vec![k.clone()], on));
                        }
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width() * 0.55, row_h),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                path_label(ui, &l.rel_path, !on);
                            },
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(human(l.size)).size(12.5).color(theme::muted()));
                            let text = format!("→ {}.7z › {}", l.original_archive, l.original_path);
                            ui.add(egui::Label::new(RichText::new(text).size(12.5).color(if on { theme::accent_text() } else { theme::muted() })).truncate())
                                .on_hover_text(format!("Original: {}.7z › {}", l.original_archive, l.original_path));
                        });
                    }
                }
            });
        }
    });
    if let Some((keys, on)) = flip {
        for k in keys {
            if on {
                st.chosen.insert(k);
            } else {
                st.chosen.remove(&k);
            }
        }
    }
}

/// The size limit: a tick box and the smallest size, in MB, of a copy to replace.
fn size_limit(ui: &mut egui::Ui, st: &mut State) {
    // Laid out right to left on the page, so the parts go in reverse there.
    let rtl = ui.layout().prefer_right_to_left();
    let mut parts: [&dyn Fn(&mut egui::Ui, &mut State); 2] = [
        &|ui, st| {
            ui.checkbox(&mut st.min_on, "Only copies of at least")
                .on_hover_text("Smaller copies stay as they are; only copies this big or bigger are replaced");
        },
        &|ui, st| {
            ui.add_enabled(st.min_on, egui::DragValue::new(&mut st.min_mb).range(0.0..=100_000.0).speed(1.0).max_decimals(1).suffix(" MB"));
        },
    ];
    if rtl {
        parts.reverse();
    }
    for p in parts {
        p(ui, st);
    }
}

fn date(utc: &str) -> String {
    utc.get(..16).unwrap_or(utc).replace('T', " ")
}

fn copies(n: usize) -> String {
    if n == 1 { "1 copy".into() } else { format!("{n} copies") }
}

fn archives(n: usize) -> String {
    if n == 1 { "1 archive".into() } else { format!("{n} archives") }
}
