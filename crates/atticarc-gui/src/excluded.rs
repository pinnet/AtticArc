//! Excluded: housekeeping for archived folders some of whose files the
//! exclusions left out. Shows what was set aside or discarded, opens the
//! archives, puts chosen files back into the main archive, and deletes the
//! folder through the usual check once everything in it is accounted for.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;

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
    /// Ticked files, by archive id and path inside the folder.
    ticked: BTreeSet<(i64, String)>,
    /// Files being put back into an archive, in the background.
    running: Option<Running>,
    /// What the last put-back did, and whether it worked.
    note: Option<(bool, String)>,
    /// Waiting for a yes before discarding the rest.
    confirm: Option<Request>,
}

struct Running {
    archive_id: i64,
    name: String,
    progress: Arc<archive::Progress>,
    rx: Receiver<Result<archive::Restored, String>>,
}

#[derive(Clone)]
struct Request {
    archive_id: i64,
    archive: PathBuf,
    name: String,
    keep: Vec<String>,
    discard_rest: bool,
    /// Set-aside files that will be discarded.
    rest: usize,
    folder_gone: bool,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }

    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    fn start(&mut self, r: Request, settings: archive::Settings) {
        let (tx, rx) = channel();
        let progress = Arc::new(archive::Progress::default());
        let p = progress.clone();
        std::thread::spawn(move || {
            let res = archive::restore_excluded(&r.archive, &r.keep, r.discard_rest, &settings, &p);
            let _ = tx.send(res.map_err(|e| format!("{e:#}")));
        });
        self.ticked.retain(|(id, _)| *id != r.archive_id);
        self.note = None;
        self.running = Some(Running { archive_id: r.archive_id, name: r.name, progress, rx });
    }
}

impl App {
    pub fn excluded(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        let settings = self.settings();
        let busy = self.jobs.is_busy();
        let st = &mut self.excluded;
        let mut changed = false;
        if let Some(run) = &st.running
            && let Ok(res) = run.rx.try_recv()
        {
            st.note = Some(match res {
                Ok(r) => {
                    let mut t = format!("{}: put back {} and verified the archive.", run.name, files(r.put_back));
                    if r.discarded > 0 {
                        t.push_str(&format!(" {} moved to discarded.", files(r.discarded)));
                    }
                    if r.aside_removed {
                        t.push_str(" The set-aside archive was emptied and removed.");
                    }
                    (true, t)
                }
                Err(e) => (false, format!("{}: nothing was changed. {e}", run.name)),
            });
            st.running = None;
            st.loaded = false;
            changed = true;
        }
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
        if let Some((ok, text)) = &st.note {
            ui.colored_label(if *ok { theme::ok() } else { theme::danger() }, text);
            ui.add_space(6.0);
        }
        if st.folders.is_empty() {
            card(ui, |ui| {
                ui.label("No archived folder has had files left out. Exclusions are set on the New job page.");
            });
            return;
        }

        let mut browse = None;
        let mut delete = None;
        let mut request = None;
        let running = st.running.as_ref().map(|r| (r.archive_id, r.progress.clone()));
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
                            let resp = ui.add_enabled(!busy && !locked && running.is_none(), del);
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
                    if let Some((id, p)) = &running
                        && *id == f.archive.archive_id
                    {
                        ui.add_space(6.0);
                        ui.label(RichText::new(p.phase.lock().unwrap().clone()).color(theme::muted()));
                        ui.add(egui::ProgressBar::new(p.fraction()).desired_height(10.0).fill(theme::accent()));
                    }
                    let id = f.archive.archive_id;
                    let folder_there = f.source_deleted_utc.is_none() && f.source.is_dir();
                    egui::CollapsingHeader::new("Files left out").id_salt(("left-out", id)).show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "Tick files to put them back into the main archive. Discarded files can come back \
                                 only while the folder still has them.",
                            )
                            .color(theme::muted()),
                        );
                        ui.add_space(4.0);
                        egui::Grid::new(("left-out-grid", id)).num_columns(4).spacing([18.0, 4.0]).striped(true).show(ui, |ui| {
                            let rows = f.set_aside.iter().map(|x| (x, true)).chain(f.discarded.iter().map(|x| (x, false)));
                            for ((rel, size), aside) in rows {
                                let key = (id, rel.clone());
                                let mut on = st.ticked.contains(&key);
                                let can = aside || folder_there;
                                let resp = ui.add_enabled(can && running.is_none(), egui::Checkbox::without_text(&mut on));
                                let resp = if can { resp } else { resp.on_disabled_hover_text("The folder no longer has this file") };
                                if resp.changed() {
                                    if on {
                                        st.ticked.insert(key);
                                    } else {
                                        st.ticked.remove(&key);
                                    }
                                }
                                ui.label(RichText::new(rel).monospace());
                                ui.label(human(*size));
                                ui.label(RichText::new(if aside { "Set aside" } else { "Discarded" }).color(theme::muted()));
                                ui.end_row();
                            }
                        });
                        ui.add_space(6.0);
                        let keep: Vec<String> = st.ticked.iter().filter(|(a, _)| *a == id).map(|(_, r)| r.clone()).collect();
                        let rest = f.set_aside.iter().filter(|(r, _)| !keep.contains(r)).count();
                        let idle = running.is_none() && !busy;
                        let req = |discard_rest| Request {
                            archive_id: id,
                            archive: f.archive.archive_path.clone(),
                            name: f.name.clone(),
                            keep: keep.clone(),
                            discard_rest,
                            rest,
                            folder_gone: !folder_there,
                        };
                        ui.horizontal(|ui| {
                            let back = ui
                                .add_enabled(idle && !keep.is_empty(), egui::Button::new(if keep.is_empty() {
                                    "Put ticked files back in the archive".to_string()
                                } else {
                                    format!("Put {} back in the archive", files(keep.len()))
                                }))
                                .on_hover_text("Rewrites the archive with these files added, verifies it, then replaces the old one");
                            if back.clicked() {
                                request = Some(req(false));
                            }
                            if !f.set_aside.is_empty() {
                                let label = if keep.is_empty() { "Discard the set-aside files…".to_string() } else { "Put ticked back, discard the rest…".to_string() };
                                let b = egui::Button::new(RichText::new(label).color(theme::danger()));
                                if ui.add_enabled(idle && rest > 0, b).clicked() {
                                    request = Some(req(true));
                                }
                            }
                        });
                    });
                });
                ui.add_space(10.0);
            }
        });
        if let Some(r) = request {
            if r.discard_rest {
                st.confirm = Some(r);
            } else {
                st.start(r, settings.clone());
            }
        }
        if st.confirm.is_some() {
            confirm_discard(ui.ctx(), st, &settings);
        }
        if changed {
            self.catalogue.invalidate();
            self.duplicates.invalidate();
        }
        if let Some(id) = browse {
            self.catalogue.show_archive(id);
            self.screen = Screen::Catalogue;
        }
        if let Some((path, archived)) = delete {
            self.jobs.ask_delete(path, archived);
        }
    }
}

fn confirm_discard(ctx: &egui::Context, st: &mut State, settings: &archive::Settings) {
    let Some(r) = st.confirm.clone() else { return };
    let mut close = false;
    egui::Modal::new(egui::Id::unique("discard-rest")).show(ctx, |ui| {
        ui.set_max_width(480.0);
        ui.heading(format!("Discard the rest of {}?", r.name));
        let mut text = String::new();
        if !r.keep.is_empty() {
            text.push_str(&format!("{} go back into the main archive. ", files(r.keep.len())));
        }
        text.push_str(&format!(
            "The other {} set aside are moved to the discarded list and the set-aside archive is deleted, \
             so they are kept nowhere. ",
            files(r.rest)
        ));
        text.push_str(if r.folder_gone {
            "The folder has already been deleted, so they will be gone for good."
        } else {
            "They stay in the folder until it is deleted."
        });
        ui.label(text);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let go = egui::Button::new(RichText::new("Discard the rest").color(egui::Color32::WHITE)).fill(theme::danger_fill());
            if ui.add(go).clicked() {
                st.start(r.clone(), settings.clone());
                close = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if close {
        st.confirm = None;
    }
}

fn files(n: usize) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}
