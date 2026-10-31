//! Duplicates: files with the same size and BLAKE3 hash across all archives.
//! A split pane: the duplicated files on the left, the copies of the one
//! picked on the right, each with a tick box to delete it. Ticks stay while
//! moving between files, so copies of many files are deleted together.
//! Deleting removes a copy from its archive, which is rewritten, verified
//! and swapped in; one copy of every file always stays.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};

use atticarc_core::archive;
use atticarc_core::catalog::{self, Catalog, DuplicateGroup, FoundFile};

use crate::{App, card, heading, human, pill, theme};

const MAX_GROUPS: usize = 300;

#[derive(Default)]
pub struct State {
    loaded: bool,
    groups: Vec<DuplicateGroup>,
    error: Option<String>,
    /// The group shown on the right, by hash.
    selected: Option<String>,
    /// Copies ticked for deleting, by catalogue entry id.
    ticked: BTreeSet<i64>,
    confirm: bool,
    running: Option<Running>,
    /// What the last delete did, and whether it worked.
    note: Option<(bool, String)>,
}

struct Running {
    progress: Arc<archive::Progress>,
    rx: Receiver<Result<archive::RemovedDuplicates, String>>,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }

    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// The ticked copies, with the size of each.
    fn ticked_copies(&self) -> Vec<&FoundFile> {
        self.groups.iter().flat_map(|g| &g.copies).filter(|c| self.ticked.contains(&c.entry_id)).collect()
    }

    fn start(&mut self, out: std::path::PathBuf, settings: archive::Settings) {
        let ids: Vec<i64> = self.ticked.iter().copied().collect();
        let (tx, rx) = channel();
        let progress = Arc::new(archive::Progress::default());
        let p = progress.clone();
        std::thread::spawn(move || {
            let _ = tx.send(archive::remove_duplicates(&out, &ids, &settings, &p).map_err(|e| format!("{e:#}")));
        });
        self.note = None;
        self.running = Some(Running { progress, rx });
    }
}

/// Copies in a set-aside archive are managed on the Excluded page.
fn in_aside(c: &FoundFile) -> bool {
    c.archive_name.ends_with(" (excluded)")
}

fn file_name(g: &DuplicateGroup) -> &str {
    g.copies[0].rel_path.rsplit('/').next().unwrap_or("")
}

impl App {
    pub fn duplicates(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let settings = self.settings();
        let busy = self.jobs.is_busy();
        let st = &mut self.duplicates;
        let mut changed = false;
        if let Some(run) = &st.running
            && let Ok(res) = run.rx.try_recv()
        {
            st.note = Some(match res {
                Ok(r) => (
                    true,
                    format!(
                        "Deleted {} ({}) from {}; each archive was rewritten and verified.",
                        copies(r.files),
                        human(r.bytes),
                        if r.archives == 1 { "1 archive".to_string() } else { format!("{} archives", r.archives) }
                    ),
                ),
                Err(e) => (false, format!("Delete stopped: {e}")),
            });
            st.running = None;
            st.ticked.clear();
            st.loaded = false;
            changed = true;
        }
        if !st.loaded {
            st.loaded = true;
            st.error = None;
            st.groups.clear();
            if catalog::exists(&out) {
                match Catalog::open_existing(&out).and_then(|c| c.duplicate_groups(MAX_GROUPS)) {
                    Ok(g) => st.groups = g,
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
            let ids: BTreeSet<i64> = st.groups.iter().flat_map(|g| &g.copies).map(|c| c.entry_id).collect();
            st.ticked.retain(|id| ids.contains(id));
            if !st.selected.as_ref().is_some_and(|h| st.groups.iter().any(|g| &g.blake3 == h)) {
                st.selected = st.groups.first().map(|g| g.blake3.clone());
            }
        }
        let reclaimable: u64 = st.groups.iter().map(|g| g.reclaimable()).sum();
        let ticked = st.ticked_copies();
        let (n_ticked, ticked_bytes) = (ticked.len(), ticked.iter().map(|c| c.size).sum::<u64>());
        let running = st.running.as_ref().map(|r| r.progress.clone());

        heading(
            ui,
            "Duplicates",
            "Files with identical size and BLAKE3 hash, across every archive in the catalogue. Tick the copies to delete; \
             a deleted copy is removed from its archive, and one copy of every file always stays.",
        );
        ui.horizontal(|ui| {
            let stats = [
                ("Duplicated files", st.groups.len().to_string()),
                ("Reclaimable", human(reclaimable)),
                ("Ticked to delete", format!("{} · {}", copies(n_ticked), human(ticked_bytes))),
            ];
            for (label, value) in stats {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).size(13.0).color(theme::muted()));
                    ui.label(RichText::new(value).size(19.0).strong());
                });
                ui.add_space(24.0);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let idle = running.is_none() && !busy;
                let del = egui::Button::new(RichText::new(format!("Delete {}…", copies(n_ticked))).color(egui::Color32::WHITE))
                    .fill(theme::danger_fill());
                if ui.add_enabled(idle && n_ticked > 0, del).clicked() {
                    st.confirm = true;
                }
                if ui.add_enabled(idle && n_ticked > 0, egui::Button::new("Clear ticks")).clicked() {
                    st.ticked.clear();
                }
                if ui.add_enabled(idle, egui::Button::new("Refresh")).clicked() {
                    st.loaded = false;
                }
            });
        });
        ui.add_space(8.0);
        if let Some(p) = &running {
            ui.label(RichText::new(p.phase.lock().unwrap().clone()).color(theme::muted()));
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
        if st.groups.is_empty() {
            card(ui, |ui| {
                ui.label("No duplicates found in the catalogue.");
            });
            return;
        }

        let w = ui.available_width();
        let h = ui.available_height();
        let gap = ui.spacing().item_spacing.x;
        let locked = running.is_some();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(w * 0.4 - gap, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                card(ui, |ui| file_list(ui, st))
            });
            ui.allocate_ui_with_layout(egui::vec2(w * 0.6, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                card(ui, |ui| copies_view(ui, st, locked))
            });
        });

        if st.confirm {
            confirm_delete(ui.ctx(), st, &out, &settings);
        }
        if changed {
            self.catalogue.invalidate();
            self.excluded.invalidate();
        }
    }
}

/// The left pane: one row per duplicated file.
fn file_list(ui: &mut egui::Ui, st: &mut State) {
    ui.label(RichText::new("Duplicated files").strong());
    ui.add_space(4.0);
    let row_h = 58.0;
    egui::ScrollArea::vertical().id_salt("dup-files").auto_shrink([false, false]).show_rows(ui, row_h, st.groups.len(), |ui, range| {
        for g in &st.groups[range] {
            let on = st.selected.as_deref() == Some(g.blake3.as_str());
            let marked = g.copies.iter().filter(|c| st.ticked.contains(&c.entry_id)).count();
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h - 4.0), egui::Sense::click());
            let fill = if on { theme::accent().gamma_multiply(0.35) } else if resp.hovered() { theme::neutral_soft() } else { egui::Color32::TRANSPARENT };
            ui.painter().rect_filled(rect, 6.0, fill);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(10.0, 4.0))));
            child.horizontal(|ui| {
                ui.label(RichText::new(file_name(g)).monospace().strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if marked > 0 {
                        pill(ui, &format!("{marked} to delete"), theme::danger(), theme::neutral_soft());
                    }
                });
            });
            child.label(RichText::new(format!("{} · {}", human(g.size), copies(g.copies.len()))).size(12.5).color(theme::muted()));
            if resp.clicked() {
                st.selected = Some(g.blake3.clone());
            }
            ui.add_space(4.0);
        }
    });
}

/// The right pane: every copy of the file picked, each with a tick box.
fn copies_view(ui: &mut egui::Ui, st: &mut State, locked: bool) {
    let Some(g) = st.groups.iter().find(|g| Some(&g.blake3) == st.selected.as_ref()) else {
        ui.label("Pick a file on the left to see its copies.");
        return;
    };
    ui.label(RichText::new(file_name(g)).monospace().strong().size(16.0));
    let short = format!("{}…{}", &g.blake3[..8], &g.blake3[g.blake3.len() - 8..]);
    ui.label(RichText::new(format!("{} · {} · BLAKE3 {short}", human(g.size), copies(g.copies.len()))).color(theme::muted()));
    ui.add_space(8.0);
    let kept = g.copies.iter().filter(|c| !st.ticked.contains(&c.entry_id)).count();
    let mut toggle = None;
    let mut keep_only = None;
    egui::ScrollArea::vertical().id_salt("dup-copies").auto_shrink([false, false]).show(ui, |ui| {
        for (i, c) in g.copies.iter().enumerate() {
            let marked = st.ticked.contains(&c.entry_id);
            let fill = if i % 2 == 1 { theme::neutral_soft().gamma_multiply(0.6) } else { egui::Color32::TRANSPARENT };
            egui::Frame::NONE.fill(fill).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(8, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    // The last copy kept can't be ticked.
                    let can = !locked && !in_aside(c) && (marked || kept > 1);
                    let mut on = marked;
                    let resp = ui.add_enabled(can, egui::Checkbox::new(&mut on, "Delete"));
                    let resp = if in_aside(c) {
                        resp.on_disabled_hover_text("This copy is in a set-aside archive; manage it on the Excluded page")
                    } else if !marked && kept <= 1 {
                        resp.on_disabled_hover_text("One copy must stay")
                    } else {
                        resp
                    };
                    if resp.changed() {
                        toggle = Some(c.entry_id);
                    }
                    ui.add_space(6.0);
                    let info_w = (ui.available_width() - 130.0).max(120.0);
                    ui.allocate_ui_with_layout(egui::vec2(info_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}.7z", c.archive_name)).strong());
                            if marked {
                                pill(ui, "Delete", theme::danger(), theme::neutral_soft());
                            } else {
                                pill(ui, "Keep", theme::ok(), theme::ok_soft());
                            }
                        });
                        ui.add(egui::Label::new(RichText::new(&c.rel_path).monospace().size(13.0)).wrap());
                        let when = c.modified_utc.as_deref().map(|m| m.replace('T', " ").replace('Z', " UTC")).unwrap_or_default();
                        ui.add(egui::Label::new(RichText::new(format!("from {}   ·   {when}", c.source_path)).size(12.0).color(theme::muted())).wrap());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!locked && !in_aside(c), egui::Button::new("Keep only this")).on_hover_text("Ticks every other copy").clicked() {
                            keep_only = Some(c.entry_id);
                        }
                    });
                });
            });
            ui.add_space(4.0);
        }
    });
    if let Some(id) = toggle
        && !st.ticked.remove(&id)
    {
        st.ticked.insert(id);
    }
    if let Some(keep) = keep_only {
        for c in &g.copies {
            if c.entry_id == keep {
                st.ticked.remove(&c.entry_id);
            } else if !in_aside(c) {
                st.ticked.insert(c.entry_id);
            }
        }
    }
}

fn confirm_delete(ctx: &egui::Context, st: &mut State, out: &std::path::Path, settings: &archive::Settings) {
    let ticked = st.ticked_copies();
    let bytes: u64 = ticked.iter().map(|c| c.size).sum();
    let mut by_archive: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &ticked {
        *by_archive.entry(c.archive_name.as_str()).or_default() += 1;
    }
    let n = ticked.len();
    let mut close = false;
    let mut go = false;
    egui::Modal::new(egui::Id::unique("delete-duplicates")).show(ctx, |ui| {
        ui.set_max_width(500.0);
        ui.heading(format!("Delete {}?", copies(n)));
        ui.label(format!(
            "Removes {} ({}) from the archives below. Each archive is unpacked, written again without them, verified, \
             and only then replaces the old one, so this needs free space for the largest archive. Another copy of \
             every file stays, and each archive's manifest records what was removed, so its folder can still be \
             checked before it is deleted.",
            copies(n),
            human(bytes)
        ));
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
            for (name, k) in &by_archive {
                ui.label(RichText::new(format!("{name}.7z: {}", copies(*k))).monospace().size(13.0));
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let del = egui::Button::new(RichText::new("Delete from the archives").color(egui::Color32::WHITE)).fill(theme::danger_fill());
            if ui.add(del).clicked() {
                go = true;
                close = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if go {
        st.start(out.to_path_buf(), settings.clone());
    }
    if close {
        st.confirm = false;
    }
}

fn copies(n: usize) -> String {
    if n == 1 { "1 copy".into() } else { format!("{n} copies") }
}
