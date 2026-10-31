//! Duplicates: files with the same size and BLAKE3 hash across all archives.
//! One file at a time, in a split pane: one copy on the right, which is
//! kept, and the other copies on the left, each with a tick box to delete
//! it. Previous and Next (or the arrow keys) move between files, and ticks
//! stay, so copies of many files are deleted together.
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
    /// The file shown, as an index into `groups`.
    index: usize,
    /// For each file, by hash, the copy shown on the right, which is kept.
    kept: BTreeMap<String, i64>,
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
            st.index = st.index.min(st.groups.len().saturating_sub(1));
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

        let locked = running.is_some();
        nav_bar(ui, st, locked);
        ui.add_space(8.0);
        let w = ui.available_width();
        let h = ui.available_height();
        let gap = ui.spacing().item_spacing.x;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(w * 0.55 - gap, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                card(ui, |ui| others_view(ui, st, locked))
            });
            ui.allocate_ui_with_layout(egui::vec2(w * 0.45, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                card(ui, |ui| kept_view(ui, st))
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

/// The file shown, the copy on the right, and the other copies.
fn current(st: &State) -> Option<(&DuplicateGroup, &FoundFile, Vec<&FoundFile>)> {
    let g = st.groups.get(st.index)?;
    let kept_id = st.kept.get(&g.blake3).copied();
    let kept = g
        .copies
        .iter()
        .find(|c| Some(c.entry_id) == kept_id)
        .or_else(|| g.copies.iter().find(|c| !in_aside(c)))
        .unwrap_or(&g.copies[0]);
    let others = g.copies.iter().filter(|c| c.entry_id != kept.entry_id).collect();
    Some((g, kept, others))
}

/// Above the panes: which file this is, moving between files, and the
/// quick choices that move on to the next one.
fn nav_bar(ui: &mut egui::Ui, st: &mut State, locked: bool) {
    let n = st.groups.len();
    let Some((g, _, others)) = current(st) else { return };
    let other_ids: Vec<i64> = others.iter().filter(|c| !in_aside(c)).map(|c| c.entry_id).collect();
    let (name, size, count) = (file_name(g).to_string(), g.size, g.copies.len());
    let (prev, mut next) = (st.index > 0, st.index + 1 < n);
    let mut delete_others = false;
    let mut keep_all = false;
    card(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.add_enabled(prev, egui::Button::new("◀ Previous")).on_hover_text("Left arrow").clicked() {
                st.index -= 1;
            }
            ui.vertical(|ui| {
                ui.label(RichText::new(format!("File {} of {n}", st.index + 1)).size(12.5).color(theme::muted()));
                ui.label(RichText::new(&name).monospace().strong().size(16.0));
            });
            ui.label(RichText::new(format!("{} · {}", human(size), copies(count))).color(theme::muted()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(next, egui::Button::new("Next ▶")).on_hover_text("Right arrow").clicked() {
                    st.index += 1;
                    next = false;
                }
                let keep = egui::Button::new("Keep all, next");
                if ui.add_enabled(!locked, keep).on_hover_text("Unticks the other copies of this file and moves on (K)").clicked() {
                    keep_all = true;
                }
                let del = egui::Button::new(RichText::new("Delete the others, next").color(theme::danger()));
                if ui
                    .add_enabled(!locked && !other_ids.is_empty(), del)
                    .on_hover_text("Ticks every copy on the left and moves on (D). Nothing is deleted until you press Delete above.")
                    .clicked()
                {
                    delete_others = true;
                }
            });
        });
    });
    // Keys, unless a text box has them.
    if !locked && !ui.ctx().egui_wants_keyboard_input() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) && prev {
                st.index -= 1;
            } else if i.key_pressed(egui::Key::ArrowRight) && next {
                st.index += 1;
            } else if i.key_pressed(egui::Key::D) && !other_ids.is_empty() {
                delete_others = true;
            } else if i.key_pressed(egui::Key::K) {
                keep_all = true;
            }
        });
    }
    if delete_others || keep_all {
        for id in &other_ids {
            if delete_others {
                st.ticked.insert(*id);
            } else {
                st.ticked.remove(id);
            }
        }
        if next {
            st.index += 1;
        }
    }
}

/// The left pane: the other copies of the file, each with a tick box.
fn others_view(ui: &mut egui::Ui, st: &mut State, locked: bool) {
    let Some((g, _, others)) = current(st) else { return };
    ui.label(RichText::new("Other copies").strong());
    ui.label(RichText::new("Tick the ones to delete. Unticked copies are kept.").size(12.5).color(theme::muted()));
    ui.add_space(6.0);
    let hash = g.blake3.clone();
    let mut toggle = None;
    let mut swap = None;
    egui::ScrollArea::vertical().id_salt("dup-others").auto_shrink([false, false]).show(ui, |ui| {
        for (i, c) in others.iter().enumerate() {
            let marked = st.ticked.contains(&c.entry_id);
            let fill = if i % 2 == 1 { theme::neutral_soft().gamma_multiply(0.6) } else { egui::Color32::TRANSPARENT };
            egui::Frame::NONE.fill(fill).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(8, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let mut on = marked;
                    let resp = ui.add_enabled(!locked && !in_aside(c), egui::Checkbox::new(&mut on, "Delete"));
                    let resp = resp.on_disabled_hover_text("This copy is in a set-aside archive; manage it on the Excluded page");
                    if resp.changed() {
                        toggle = Some(c.entry_id);
                    }
                    ui.add_space(6.0);
                    let info_w = (ui.available_width() - 120.0).max(120.0);
                    ui.allocate_ui_with_layout(egui::vec2(info_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| copy_info(ui, c, marked));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!locked, egui::Button::new("Show on right")).on_hover_text("Makes this the copy on the right, and keeps it").clicked() {
                            swap = Some(c.entry_id);
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
    if let Some(id) = swap {
        // The copy on the right is never deleted.
        st.ticked.remove(&id);
        st.kept.insert(hash, id);
    }
}

/// The right pane: the copy that stays.
fn kept_view(ui: &mut egui::Ui, st: &State) {
    let Some((g, kept, _)) = current(st) else { return };
    ui.label(RichText::new("This copy").strong());
    ui.label(RichText::new("Always kept, so one copy of every file stays.").size(12.5).color(theme::muted()));
    ui.add_space(6.0);
    copy_info(ui, kept, false);
    ui.add_space(10.0);
    egui::Grid::new("dup-kept").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(theme::muted()));
            ui.add(egui::Label::new(RichText::new(v).monospace().size(13.0)).wrap());
            ui.end_row();
        };
        row(ui, "Size", human(g.size));
        row(ui, "BLAKE3", g.blake3.clone());
        if let Some(sha) = &kept.sha256 {
            row(ui, "SHA-256", sha.clone());
        }
    });
}

/// A copy's archive, path inside it, folder and date.
fn copy_info(ui: &mut egui::Ui, c: &FoundFile, marked: bool) {
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
