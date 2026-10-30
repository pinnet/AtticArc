//! Duplicates: files with the same size and BLAKE3 hash across all archives.
//! Mockup: Duplicates.dc.html. Read-only for now: choosing a copy to keep and
//! removing the others is not built yet.

use eframe::egui::{self, RichText};

use attic_core::catalog::{CATALOG_FILE, Catalog, DuplicateGroup};

use crate::{App, card, heading, human, theme};

const MAX_GROUPS: usize = 300;

#[derive(Default)]
pub struct State {
    loaded: bool,
    groups: Vec<DuplicateGroup>,
    error: Option<String>,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }
}

impl App {
    pub fn duplicates(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let st = &mut self.duplicates;
        if !st.loaded {
            st.loaded = true;
            st.error = None;
            st.groups.clear();
            if out.join(CATALOG_FILE).is_file() {
                match Catalog::open_existing(&out).and_then(|c| c.duplicate_groups(MAX_GROUPS)) {
                    Ok(g) => st.groups = g,
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
        }
        let reclaimable: u64 = st.groups.iter().map(|g| g.reclaimable()).sum();

        heading(ui, "Duplicates", "Files with identical size and BLAKE3 hash, across every archive in the catalogue.");
        ui.horizontal(|ui| {
            for (label, value) in [("Groups", st.groups.len().to_string()), ("Reclaimable", human(reclaimable))] {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).size(13.0).color(theme::muted()));
                    ui.label(RichText::new(value).size(19.0).strong());
                });
                ui.add_space(24.0);
            }
            if ui.button("Refresh").clicked() {
                st.loaded = false;
            }
        });
        ui.add_space(8.0);
        egui::Frame::NONE
            .fill(theme::warn_soft())
            .stroke(egui::Stroke::new(1.0, theme::warn_border()))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(
                        "This list is for review only: nothing is removed from here yet. Choosing which copy to keep \
                         and removing the others, with a log of every removal, comes next.",
                    )
                    .color(theme::warn()),
                );
            });
        ui.add_space(10.0);
        if let Some(e) = &st.error {
            ui.colored_label(theme::danger(), e);
        }
        if st.groups.is_empty() {
            card(ui, |ui| {
                ui.label("No duplicates found in the catalogue.");
            });
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for g in &st.groups {
                card(ui, |ui| {
                    let name = g.copies[0].rel_path.rsplit('/').next().unwrap_or("").to_string();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(name).monospace().strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let short = format!("{}…{}", &g.blake3[..6], &g.blake3[g.blake3.len() - 6..]);
                            ui.label(
                                RichText::new(format!("{} · {} copies · {short}", human(g.size), g.copies.len())).color(theme::muted()),
                            );
                        });
                    });
                    for c in &g.copies {
                        ui.label(RichText::new(format!("{}.7z › {}", c.archive_name, c.rel_path)).monospace().size(13.0));
                    }
                });
                ui.add_space(8.0);
            }
        });
    }
}
