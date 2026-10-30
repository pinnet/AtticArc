//! Catalogue: search every file in every archive without unpacking, see its
//! details and extract it. Mockup: Catalogue.dc.html.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};

use attic_core::archive;
use attic_core::catalog::{ArchiveSummary, CATALOG_FILE, Catalog, FoundFile};

use crate::{App, card, heading, human, primary_button, theme};

const MAX_RESULTS: usize = 500;

#[derive(Default)]
pub struct State {
    loaded: bool,
    query: String,
    /// The query and filter the results are for.
    searched: Option<(String, Option<i64>)>,
    archive_filter: Option<i64>,
    archives: Vec<ArchiveSummary>,
    results: Vec<FoundFile>,
    selected: Option<usize>,
    error: Option<String>,
    note: Option<String>,
    extracting: Option<Receiver<Result<PathBuf, String>>>,
    confirm_clear: bool,
    /// Set after the catalogue was cleared, so the folder list reloads.
    cleared: bool,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
        self.searched = None;
    }

    pub fn busy(&self) -> bool {
        self.extracting.is_some()
    }
}

impl App {
    pub fn catalogue(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        if std::mem::take(&mut self.catalogue.cleared) {
            self.reload();
        }
        let busy = self.jobs.is_busy();
        let st = &mut self.catalogue;
        if let Some(rx) = &st.extracting
            && let Ok(res) = rx.try_recv() {
                st.note = Some(match res {
                    Ok(p) => format!("Extracted to {}", p.display()),
                    Err(e) => format!("Extract failed: {e}"),
                });
                st.extracting = None;
            }
        if !st.loaded {
            st.loaded = true;
            st.error = None;
            st.archives.clear();
            if out.join(CATALOG_FILE).is_file() {
                match Catalog::open_existing(&out).and_then(|c| c.archives()) {
                    Ok(a) => st.archives = a,
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
        }
        let want = (st.query.trim().to_string(), st.archive_filter);
        if !want.0.is_empty() && st.searched.as_ref() != Some(&want) {
            st.selected = None;
            st.results = Catalog::open_existing(&out)
                .and_then(|c| c.search(&want.0, want.1, MAX_RESULTS))
                .unwrap_or_else(|e| {
                    st.error = Some(format!("{e:#}"));
                    Vec::new()
                });
            st.selected = (!st.results.is_empty()).then_some(0);
            st.searched = Some(want);
        }

        heading(ui, "Catalogue", &format!("Every file in every archive in {}, searchable without unpacking.", out.display()));
        if let Some(e) = &st.error {
            ui.colored_label(theme::danger(), e);
        }
        if st.archives.is_empty() {
            card(ui, |ui| {
                ui.label("No archives in this destination yet. Archive some folders from New job first.");
                if let Some(n) = &st.note {
                    ui.label(RichText::new(n).color(theme::muted()));
                }
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("Search files").strong());
            ui.add(egui::TextEdit::singleline(&mut st.query).hint_text("name or folder, e.g. holiday 2014").desired_width(360.0));
            let current = st
                .archive_filter
                .and_then(|id| st.archives.iter().find(|a| a.id == id))
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "All archives".into());
            egui::ComboBox::from_id_salt("archive-filter").selected_text(current).show_ui(ui, |ui| {
                ui.selectable_value(&mut st.archive_filter, None, "All archives");
                for a in &st.archives {
                    ui.selectable_value(&mut st.archive_filter, Some(a.id), &a.name);
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clear = egui::Button::new(RichText::new("Clear the catalogue…").color(theme::danger()));
                if ui.add_enabled(!busy, clear).on_hover_text("For development: forget every archive in this catalogue").clicked() {
                    st.confirm_clear = true;
                }
            });
        });
        ui.add_space(8.0);
        if st.confirm_clear {
            confirm_clear(ui.ctx(), st, &out);
        }

        if st.query.trim().is_empty() {
            archive_list(ui, st);
            return;
        }

        ui.columns(2, |c| {
            card(&mut c[0], |ui| {
                let more = if st.results.len() == MAX_RESULTS { " (first 500, largest first)" } else { "" };
                ui.label(RichText::new(format!("{} matches{more} · click a file for details", st.results.len())).color(theme::muted()));
                ui.separator();
                egui::ScrollArea::vertical().id_salt("results").auto_shrink([false, false]).max_height(ui.available_height()).show(ui, |ui| {
                    for (i, f) in st.results.iter().enumerate() {
                        let name = f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path);
                        let folder = f.rel_path.rsplit_once('/').map(|x| x.0).unwrap_or("");
                        let text = format!("{name}\n{} › {folder}", f.archive_name);
                        let on = st.selected == Some(i);
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::selectable(on, RichText::new(text).monospace()).min_size(egui::vec2(ui.available_width() - 90.0, 0.0))).clicked() {
                                st.selected = Some(i);
                            }
                            ui.label(human(f.size));
                        });
                    }
                });
            });
            card(&mut c[1], |ui| match st.selected.and_then(|i| st.results.get(i)).cloned() {
                None => {
                    ui.label("No file selected.");
                }
                Some(f) => details(ui, st, &f),
            });
        });
    }
}

fn confirm_clear(ctx: &egui::Context, st: &mut State, out: &std::path::Path) {
    let mut close = false;
    egui::Modal::new(egui::Id::unique("clear-catalogue")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Clear the catalogue?");
        ui.label(format!(
            "Forgets all {} archives recorded in {}. The .7z files and their manifests stay on disk; \
             only the catalogue's records go, so their folders show as not archived again.",
            st.archives.len(),
            out.join(CATALOG_FILE).display()
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let clear = egui::Button::new(RichText::new("Clear the catalogue").color(egui::Color32::WHITE)).fill(theme::danger_fill());
            if ui.add(clear).clicked() {
                match Catalog::open_existing(out).and_then(|mut c| c.clear()) {
                    Ok(n) => {
                        st.note = Some(format!("Catalogue cleared: {n} archives forgotten."));
                        st.cleared = true;
                        st.invalidate();
                        st.query.clear();
                        st.results.clear();
                    }
                    Err(e) => st.error = Some(format!("Couldn't clear the catalogue: {e:#}")),
                }
                close = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if close {
        st.confirm_clear = false;
    }
}

fn archive_list(ui: &mut egui::Ui, st: &State) {
    card(ui, |ui| {
        ui.label(RichText::new(format!("{} archives", st.archives.len())).strong().size(16.0));
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("archives").num_columns(5).striped(true).spacing([18.0, 8.0]).show(ui, |ui| {
                for h in ["Archive", "Files", "Original", "Archive size", "Archived"] {
                    ui.label(RichText::new(h).color(theme::muted()));
                }
                ui.end_row();
                for a in &st.archives {
                    ui.label(RichText::new(&a.name).monospace()).on_hover_text(&a.archive_path);
                    ui.label(a.file_count.to_string());
                    ui.label(human(a.input_bytes));
                    ui.label(human(a.archive_bytes));
                    let when = a.created_utc.get(..10).unwrap_or(&a.created_utc);
                    let mut t = when.to_string();
                    if a.source_deleted_utc.is_some() {
                        t.push_str(" · source deleted");
                    }
                    ui.label(t);
                    ui.end_row();
                }
            });
        });
    });
}

fn details(ui: &mut egui::Ui, st: &mut State, f: &FoundFile) {
    let name = f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path).to_string();
    ui.label(RichText::new(&name).monospace().strong().size(16.0));
    ui.add_space(4.0);
    let original = PathBuf::from(&f.source_path).join(f.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    egui::Grid::new("details").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(theme::muted()));
            ui.add(egui::Label::new(RichText::new(v).monospace()).wrap());
            ui.end_row();
        };
        row(ui, "Original path", original.display().to_string());
        row(ui, "Size", human(f.size));
        row(ui, "Modified", f.modified_utc.clone().unwrap_or_default().replace('T', " ").replace('Z', " UTC"));
        row(ui, "Archive", f.archive_name.clone() + ".7z");
        row(ui, "BLAKE3", f.blake3.clone().unwrap_or_default());
        row(ui, "SHA-256", f.sha256.clone().unwrap_or_default());
        row(ui, "Copies", if f.copies > 1 { format!("{} (see Duplicates)", f.copies) } else { "1".into() });
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let archive_path = st.archives.iter().find(|a| a.id == f.archive_id).map(|a| PathBuf::from(&a.archive_path));
        let can = archive_path.as_ref().is_some_and(|p| p.is_file()) && st.extracting.is_none();
        if ui.add_enabled(can, primary_button("Extract to…")).clicked()
            && let (Some(dest), Some(arch)) = (rfd::FileDialog::new().pick_folder(), archive_path) {
                let (tx, rx) = channel();
                let rel = f.rel_path.clone();
                std::thread::spawn(move || {
                    let threads = archive::default_threads();
                    let _ = tx.send(archive::extract_file(&arch, &rel, &dest, threads).map_err(|e| format!("{e:#}")));
                });
                st.extracting = Some(rx);
                st.note = Some(format!("Extracting {name}…"));
            }
        if ui.button("Copy hash").clicked() {
            ui.ctx().copy_text(f.blake3.clone().unwrap_or_default());
        }
    });
    if let Some(n) = &st.note {
        ui.label(RichText::new(n).color(theme::muted()));
    }
}
