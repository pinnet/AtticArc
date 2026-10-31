//! New job: pick source and destination, compression settings, and tick the
//! folders to archive. Mockup: Main.dc.html.

use eframe::egui::{self, Color32, RichText};

use atticarc_core::archive;
use atticarc_core::exclude::{Action, Exclusions, Kind, Rule};
use atticarc_core::settings::Threads;

use crate::jobs::{Dialog, RowState};
use crate::{App, Screen, caption, card, heading, human, pill, primary_button, theme};

impl App {
    pub fn new_job(&mut self, ui: &mut egui::Ui) {
        let busy = self.jobs.is_busy();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            heading(
                ui,
                "New archive job",
                "Each top-level folder in the source becomes one .7z in the destination, hashed and verified.",
            );
            ui.add_enabled_ui(!busy, |ui| {
                self.locations(ui);
                ui.add_space(12.0);
                self.options(ui);
            });
            ui.add_space(12.0);
            self.folder_table(ui, busy);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let (count, bytes) = self.jobs.selection();
                let rules_ok = Exclusions::new(&self.saved.exclusions).is_ok();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let start = primary_button(&format!("Start archiving · {count} folders · {}", human(bytes)));
                    if ui.add_enabled(!busy && count > 0 && rules_ok, start).clicked() {
                        self.save();
                        self.jobs.archive_selected(self.settings());
                        self.screen = Screen::Progress;
                    }
                    if busy {
                        ui.label(RichText::new("A job is running; see Progress.").color(theme::muted()));
                    } else if !rules_ok {
                        ui.label(RichText::new("Fix the exclusion marked in red first.").color(theme::danger()));
                    }
                });
            });
        });
    }

    fn locations(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.columns(2, |cols| {
            card(&mut cols[0], |ui| {
                ui.label(caption("Source"));
                ui.label(RichText::new(self.source_dir.display().to_string()).monospace().size(17.0).color(theme::ink()));
                ui.label(RichText::new(format!("{} folders found", self.jobs.rows.len())).color(theme::muted()));
                if ui.button("Change source…").clicked()
                    && let Some(p) = rfd::FileDialog::new().set_directory(&self.source_dir).pick_folder() {
                        self.source_dir = atticarc_core::settings::strip_verbatim(&p);
                        changed = true;
                    }
            });
            card(&mut cols[1], |ui| {
                ui.label(caption("Destination"));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.out_dir_text)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY),
                );
                if resp.lost_focus() {
                    changed = true;
                }
                ui.label(RichText::new("Archives are kept here, with the catalogue in a hidden .atticarc folder.").color(theme::muted()));
                if archive::is_protected(&self.out_dir(), &self.settings()) {
                    ui.label(
                        RichText::new("This folder is protected, so archiving here will be refused. Choose another one.")
                            .color(theme::danger()),
                    );
                }
                if ui.button("Change destination…").clicked()
                    && let Some(p) = rfd::FileDialog::new().set_directory(nearest_existing(&self.out_dir())).pick_folder() {
                        self.out_dir_text = atticarc_core::settings::strip_verbatim(&p).display().to_string();
                        changed = true;
                    }
            });
        });
        if changed {
            self.save();
            self.reload();
        }
    }

    fn options(&mut self, ui: &mut egui::Ui) {
        let before = self.saved.clone();
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Compression").strong());
                    egui::ComboBox::from_id_salt("level")
                        .selected_text(format!("LZMA2 · level {}", self.saved.level))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.saved.level, 5, "LZMA2 · level 5 (default)");
                            ui.selectable_value(&mut self.saved.level, 9, "LZMA2 · level 9 (smaller, about 2× slower)");
                        });
                });
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Threads").strong());
                    egui::ComboBox::from_id_salt("threads")
                        .selected_text(format!("{} ({})", self.saved.threads.label(), self.saved.threads.count()))
                        .show_ui(ui, |ui| {
                            for t in [Threads::All, Threads::Half, Threads::One] {
                                ui.selectable_value(&mut self.saved.threads, t, format!("{} ({})", t.label(), t.count()));
                            }
                        });
                });
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("File hash").strong());
                    ui.label(RichText::new("BLAKE3 + SHA-256, every file").color(theme::muted()));
                });
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.saved.store_mode, "Store already-compressed files (zip, jpg, mp4…) without recompressing");
                    ui.label(RichText::new("Every archive is decoded and checked against its hashes after writing.").color(theme::muted()));
                });
            });
            ui.add_space(4.0);
            exclusions(ui, &mut self.saved.exclusions);
            egui::CollapsingHeader::new("Protected folders (never deleted)").show(ui, |ui| {
                let resp = ui.add(egui::TextEdit::multiline(&mut self.protected_text).desired_rows(3).desired_width(f32::INFINITY));
                if resp.lost_focus() {
                    self.save();
                }
            });
        });
        if before != self.saved {
            self.save();
        }
    }

    fn folder_table(&mut self, ui: &mut egui::Ui, busy: bool) {
        let settings = self.settings();
        let order = self.jobs.ordered();
        let archived = self.jobs.archived_rows();
        let (count, bytes) = self.jobs.selection();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Folders in {}", self.source_dir.display())).strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Refresh")).clicked() {
                        self.reload();
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} folders · {count} ticked, {} · {} already archived",
                            self.jobs.rows.len(),
                            human(bytes),
                            archived.len()
                        ))
                        .color(theme::muted()),
                    );
                });
            });
            if let Some(e) = &self.jobs.load_error {
                ui.colored_label(theme::danger(), e);
            }
            ui.separator();
            if order.is_empty() && archived.is_empty() {
                ui.label("No folders here.");
                return;
            }
            egui::Grid::new("folders").num_columns(4).striped(true).spacing([18.0, 8.0]).min_col_width(24.0).show(ui, |ui| {
                if !order.is_empty() {
                    ui.label("");
                    ui.label(RichText::new("Folder").color(theme::muted()));
                    ui.label(RichText::new("Size").color(theme::muted()));
                    ui.label(RichText::new("Status").color(theme::muted()));
                    ui.end_row();
                }
                for i in order {
                    let row = &mut self.jobs.rows[i];
                    let dim = !row.ticked;
                    cell(ui, dim, |ui| {
                        ui.add_enabled(!busy, egui::Checkbox::without_text(&mut row.ticked));
                    });
                    cell(ui, dim, |ui| {
                        ui.label(RichText::new(&row.name).monospace());
                    });
                    cell(ui, dim, |ui| {
                        ui.label(size_text(row.size));
                    });
                    cell(ui, dim, |ui| state_label(ui, &row.state, row.ticked));
                    ui.end_row();
                }
                if !archived.is_empty() {
                    ui.label("");
                    ui.label(RichText::new("Already archived").strong());
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                }
                for i in archived {
                    let row = &self.jobs.rows[i];
                    let a = row.archived.as_ref().unwrap();
                    let locked = archive::is_protected(&row.path, &settings);
                    let x = egui::Button::new(RichText::new("X").strong().color(theme::danger()));
                    let resp = ui.add_enabled(!busy && !locked, x);
                    let resp = if locked {
                        resp.on_disabled_hover_text("This folder is protected and can't be deleted here")
                    } else {
                        resp.on_hover_text("Delete this folder (it is checked against its archive first)")
                    };
                    if resp.clicked() {
                        self.jobs.dialog = Some(Dialog::ConfirmDelete(row.path.clone()));
                    }
                    ui.label(RichText::new(&row.name).monospace());
                    ui.label(size_text(row.size));
                    let when = a.created_utc.get(..10).unwrap_or(&a.created_utc);
                    let text = match &row.state {
                        RowState::Done(note) => format!("Archived now · {note}"),
                        _ => format!("Archived {when} · {} .7z", human(a.archive_bytes)),
                    };
                    pill(ui, &text, theme::ok(), theme::ok_soft());
                    ui.end_row();
                }
            });
        });
    }
}

/// The list of exclusion rules: each can be switched on and off, edited or
/// removed, and a rule that doesn't compile says why.
fn exclusions(ui: &mut egui::Ui, rules: &mut Vec<Rule>) {
    let on = rules.iter().filter(|r| r.enabled && !r.pattern.trim().is_empty()).count();
    let title = if on == 0 { "Exclusions (none on)".to_string() } else { format!("Exclusions ({on} on)") };
    egui::CollapsingHeader::new(title).id_salt("exclusions").default_open(true).show(ui, |ui| {
        ui.label(
            RichText::new(
                "Files matching a ticked rule are left out of the main archive. Set aside keeps them in a second \
                 archive, <folder>.excluded.7z; Discard keeps them nowhere, for junk like shortcuts. A wildcard \
                 without / matches file names anywhere (*.lnk); * stays within a folder, ** crosses folders. A regex \
                 matches anywhere in the path inside the folder. Case is ignored. See the Excluded page for what \
                 was left out.",
            )
            .color(theme::muted()),
        );
        ui.add_space(4.0);
        let mut remove = None;
        for (i, rule) in rules.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut rule.enabled, "").on_hover_text("Use this rule");
                // A fixed-width cell, so the patterns line up whichever kind is picked.
                let h = ui.spacing().interact_size.y;
                ui.allocate_ui_with_layout(egui::vec2(110.0, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_min_width(110.0);
                    egui::ComboBox::from_id_salt(("exclusion-kind", i)).selected_text(rule.kind.label()).show_ui(ui, |ui| {
                        for k in [Kind::Wildcard, Kind::Regex] {
                            ui.selectable_value(&mut rule.kind, k, k.label());
                        }
                    });
                });
                egui::ComboBox::from_id_salt(("exclusion-action", i))
                    .selected_text(rule.action.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut rule.action, Action::SetAside, "Set aside")
                            .on_hover_text("Keep these files, in a second archive: <folder>.excluded.7z");
                        ui.selectable_value(&mut rule.action, Action::Discard, "Discard")
                            .on_hover_text("Keep these nowhere: they are listed, then deleted with the folder");
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut rule.pattern)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(if rule.kind == Kind::Regex { r"\.(tmp|bak)$" } else { "*.lnk" })
                        .desired_width(320.0),
                );
                if ui.button("✕").on_hover_text("Remove this rule").clicked() {
                    remove = Some(i);
                }
                if !rule.pattern.trim().is_empty()
                    && let Err(e) = rule.compile()
                {
                    ui.label(RichText::new(e.to_string()).color(theme::danger())).on_hover_text(format!("{e:#}"));
                }
            });
        }
        if let Some(i) = remove {
            rules.remove(i);
        }
        if ui.button("+ Add exclusion").clicked() {
            rules.push(Rule::default());
        }
    });
}

/// `path`, or its closest parent that exists, so a folder picker can open
/// there even when the destination hasn't been created yet.
fn nearest_existing(path: &std::path::Path) -> std::path::PathBuf {
    path.ancestors().find(|p| p.is_dir()).unwrap_or(path).to_path_buf()
}

/// A grid cell, greyed out when `dim`.
fn cell(ui: &mut egui::Ui, dim: bool, add: impl FnOnce(&mut egui::Ui)) {
    ui.scope(|ui| {
        if dim {
            ui.multiply_opacity(0.4);
        }
        add(ui);
    });
}

pub fn state_label(ui: &mut egui::Ui, s: &RowState, ticked: bool) {
    match s {
        RowState::Idle => {
            if ticked {
                pill(ui, "Ready", theme::muted(), theme::neutral_soft());
            } else {
                ui.label(RichText::new("Skipped").color(theme::muted()));
            }
        }
        RowState::Queued => {
            pill(ui, "Queued", theme::muted(), theme::neutral_soft());
        }
        RowState::Working => {
            pill(ui, "Working…", Color32::WHITE, theme::accent());
        }
        RowState::Done(n) => {
            pill(ui, &format!("Verified · {n}"), theme::ok(), theme::ok_soft());
        }
        RowState::Failed(e) => {
            let first = e.lines().next().unwrap_or(e);
            pill(ui, &format!("Failed: {first}"), theme::warn(), theme::warn_soft()).on_hover_text(e);
        }
    }
}

fn size_text(s: Option<(u64, u64)>) -> String {
    match s {
        None => "…".into(),
        Some((b, f)) => format!("{} · {f} {}", human(b), if f == 1 { "file" } else { "files" }),
    }
}
