//! Progress: the running batch, per-folder status and a log. Mockup:
//! Progress.dc.html.

use std::sync::atomic::Ordering;

use eframe::egui::{self, RichText};

use crate::jobs::RowState;
use crate::new_job::state_label;
use crate::{App, Screen, card, duration, heading, human, theme};

impl App {
    pub fn progress(&mut self, ui: &mut egui::Ui) {
        let Some(b) = &self.jobs.batch else {
            heading(ui, "Progress", "Nothing has been archived since Attic started.");
            if ui.button("Go to New job").clicked() {
                self.screen = Screen::NewJob;
            }
            return;
        };
        let title = format!("Archiving {} → {}", self.source_dir.display(), self.out_dir().display());
        let fraction = b.fraction();
        let total = b.total_bytes();
        let elapsed = b.started.elapsed().as_secs_f64().max(0.5);
        let done_bytes = (fraction as f64 * total as f64) as u64;
        let stopping = b.stop_after.load(Ordering::Relaxed) && !b.done;
        let (done, verified, failed, n) = (b.done, b.verified, b.failed, b.folders.len());
        let label = b.settings_label.clone();
        let phase = b.progress.phase.lock().unwrap().clone();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| heading(ui, &title, &format!("{label} · verify after write")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if !done {
                        let b = self.jobs.batch.as_ref().unwrap();
                        let cancel = egui::Button::new(RichText::new("Cancel now").color(theme::DANGER));
                        if ui.add(cancel).on_hover_text("Stops the current folder; its partial archive is removed").clicked() {
                            b.progress.cancel.store(true, Ordering::Relaxed);
                        }
                        let stop = egui::Button::new(RichText::new("Stop after this folder").color(theme::WARN));
                        if ui.add_enabled(!stopping, stop).clicked() {
                            b.stop_after.store(true, Ordering::Relaxed);
                        }
                    }
                });
            });

            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:.0}%", fraction * 100.0)).size(38.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("{} of {}", human(done_bytes), human(total))).color(theme::MUTED));
                    });
                });
                ui.add(egui::ProgressBar::new(fraction).desired_height(12.0).fill(theme::ACCENT));
                ui.add_space(6.0);
                let rate = done_bytes as f64 / elapsed;
                let eta = if done {
                    if stopping || verified + failed < n { "Stopped".into() } else { "Done".into() }
                } else if fraction > 0.01 {
                    duration(elapsed / fraction as f64 - elapsed)
                } else {
                    "…".into()
                };
                ui.columns(4, |c| {
                    stat(&mut c[0], "Throughput", &format!("{:.0} MB/s", rate / 1e6));
                    stat(&mut c[1], if done { "Status" } else { "Time left" }, &eta);
                    stat(&mut c[2], "Folders", &format!("{} of {n}", verified + failed));
                    stat(&mut c[3], "Verified", &format!("{verified} · {failed} failed"));
                });
                if !done && !phase.is_empty() {
                    ui.label(RichText::new(format!("Now: {phase}{}", if stopping { " · stopping after this folder" } else { "" })).color(theme::MUTED));
                }
            });
            ui.add_space(12.0);

            ui.columns(2, |c| {
                card(&mut c[0], |ui| {
                    ui.label(RichText::new("Folders").strong().size(16.0));
                    ui.separator();
                    let paths: Vec<_> = self.jobs.batch.as_ref().unwrap().folders.iter().map(|f| f.0.clone()).collect();
                    egui::Grid::new("progress-folders").num_columns(3).spacing([16.0, 8.0]).striped(true).show(ui, |ui| {
                        for p in paths {
                            let Some(r) = self.jobs.rows.iter().find(|r| r.path == p) else { continue };
                            ui.label(RichText::new(&r.name).monospace());
                            ui.label(r.size.map(|s| human(s.0)).unwrap_or_default());
                            if r.state == RowState::Working {
                                let b = self.jobs.batch.as_ref().unwrap();
                                let text = format!("{} {:.0}%", b.progress.phase.lock().unwrap(), b.progress.fraction() * 100.0);
                                crate::pill(ui, &text, egui::Color32::WHITE, theme::ACCENT);
                            } else if r.state == RowState::Idle {
                                ui.label(RichText::new("Not started").color(theme::MUTED));
                            } else {
                                state_label(ui, &r.state, true);
                            }
                            ui.end_row();
                        }
                    });
                });
                egui::Frame::NONE
                    .fill(theme::SIDEBAR)
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(16))
                    .show(&mut c[1], |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Log").strong().size(16.0).color(egui::Color32::WHITE));
                        egui::ScrollArea::vertical().max_height(360.0).stick_to_bottom(true).show(ui, |ui| {
                            for l in &self.jobs.log {
                                ui.label(RichText::new(l).monospace().color(theme::SIDEBAR_TEXT));
                            }
                        });
                    });
            });
        });
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).size(13.0).color(theme::MUTED));
    ui.label(RichText::new(value).size(19.0).strong());
}
