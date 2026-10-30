//! Excluded: housekeeping for archived folders some of whose files the
//! exclusions left out. Shows what was set aside or discarded, opens the
//! archives, and deletes the folder through the usual check once everything
//! in it is accounted for.

use eframe::egui::{self, RichText};

use atticarc_core::archive;
use atticarc_core::catalog::{self, Catalog, ExcludedFolder};

use crate::{App, Screen, card, heading, human, pill, theme};

#[derive(Default)]
pub struct State {
    loaded: bool,
    /// Each folder with the catalogue id of its set-aside archive.
    folders: Vec<(ExcludedFolder, Option<i64>)>,
    error: Option<String>,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }
}

impl App {
    pub fn excluded(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let settings = self.settings();
        let busy = self.jobs.is_busy();
        let st = &mut self.excluded;
        if !st.loaded {
            st.loaded = true;
            st.error = None;
            st.folders.clear();
            if catalog::exists(&out) {
                let found = Catalog::open_existing(&out).and_then(|c| {
                    c.excluded_folders()?
                        .into_iter()
                        .map(|f| {
                            let aside = match &f.aside_archive {
                                Some(p) => c.archive_id_for(p)?,
                                None => None,
                            };
                            Ok((f, aside))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()
                });
                match found {
                    Ok(f) => st.folders = f,
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
        }

        heading(
            ui,
            "Excluded",
            "Archived folders whose exclusions left files out. Set-aside files are kept in a second archive; \
             discarded files are kept nowhere. A folder can be deleted once every file in it is accounted for.",
        );
        if let Some(e) = &st.error {
            ui.colored_label(theme::danger(), e);
        }
        if st.folders.is_empty() {
            card(ui, |ui| {
                ui.label("No archived folder has had files left out. Exclusions are set on the New job page.");
            });
            return;
        }

        let mut browse = None;
        let mut delete = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (f, aside_id) in &st.folders {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&f.name).monospace().strong().size(16.0));
                        let on_disk = f.source.is_dir();
                        let (text, fg, bg) = if f.source_deleted_utc.is_some() {
                            ("Folder deleted", theme::ok(), theme::ok_soft())
                        } else if on_disk {
                            ("Folder still on disk", theme::warn(), theme::warn_soft())
                        } else {
                            ("Folder not found", theme::muted(), theme::neutral_soft())
                        };
                        pill(ui, text, fg, bg);
                    });
                    ui.label(RichText::new(f.source.display().to_string()).monospace().color(theme::muted()));
                    ui.add_space(4.0);
                    let total = |v: &[(String, u64)]| human(v.iter().map(|x| x.1).sum());
                    if !f.set_aside.is_empty() {
                        let name = f.aside_archive.as_ref().and_then(|p| p.file_name()).unwrap_or_default().to_string_lossy();
                        ui.label(format!("{} set aside ({}) in {name}", files(f.set_aside.len()), total(&f.set_aside)));
                    }
                    if !f.discarded.is_empty() {
                        ui.label(
                            RichText::new(format!("{} discarded ({}), kept in no archive", files(f.discarded.len()), total(&f.discarded)))
                                .color(theme::warn()),
                        );
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Browse the archive").clicked() {
                            browse = Some(f.archive.archive_id);
                        }
                        if let Some(id) = aside_id
                            && ui.button("Browse the set-aside archive").clicked()
                        {
                            browse = Some(*id);
                        }
                        let can_delete = f.source_deleted_utc.is_none() && f.source.is_dir();
                        if can_delete {
                            let locked = archive::is_protected(&f.source, &settings);
                            let del = egui::Button::new(RichText::new("Delete the folder…").color(theme::danger()));
                            let resp = ui.add_enabled(!busy && !locked, del);
                            let resp = if locked {
                                resp.on_disabled_hover_text("This folder is protected and can't be deleted here")
                            } else {
                                resp.on_hover_text("Checks every file against the archives first")
                            };
                            if resp.clicked() {
                                delete = Some((f.source.clone(), f.archive.clone()));
                            }
                        }
                    });
                    egui::CollapsingHeader::new("Files left out").id_salt(("left-out", f.archive.archive_id)).show(ui, |ui| {
                        egui::Grid::new(("left-out-grid", f.archive.archive_id)).num_columns(3).spacing([18.0, 4.0]).striped(true).show(
                            ui,
                            |ui| {
                                for (rel, size, what) in f
                                    .set_aside
                                    .iter()
                                    .map(|x| (x, "Set aside"))
                                    .chain(f.discarded.iter().map(|x| (x, "Discarded")))
                                    .map(|((r, s), w)| (r, s, w))
                                {
                                    ui.label(RichText::new(rel).monospace());
                                    ui.label(human(*size));
                                    ui.label(RichText::new(what).color(theme::muted()));
                                    ui.end_row();
                                }
                            },
                        );
                    });
                });
                ui.add_space(10.0);
            }
        });
        if let Some(id) = browse {
            self.catalogue.show_archive(id);
            self.screen = Screen::Catalogue;
        }
        if let Some((path, archived)) = delete {
            self.jobs.ask_delete(path, archived);
        }
    }
}

fn files(n: usize) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}
