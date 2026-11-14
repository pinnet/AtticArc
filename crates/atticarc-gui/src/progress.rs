//! Progress: the running batch, per-folder status and a log. Mockup:
//! Progress.dc.html.

use std::sync::atomic::Ordering;

use eframe::egui::{self, RichText};

use atticarc_core::settings::{self, LOG_TEXT_MAX, LOG_TEXT_MIN};

use crate::jobs::RowState;
use crate::new_job::state_label;
use crate::{App, Screen, card, duration, heading, human, theme};

impl App {
    pub fn progress(&mut self, ui: &mut egui::Ui) {
        let Some(b) = &self.jobs.batch else {
            heading(ui, "Progress", "Nothing has been archived since Attic ARC started.");
            if ui.button("Go to New job").clicked() {
                self.screen = Screen::NewJob;
            }
            return;
        };
        let title = format!("Archiving {} → {}", self.source_dir.display(), self.out_dir().display());
        let fraction = b.fraction();
        let total = b.total_bytes();
        let elapsed = b.elapsed_secs().max(0.5);
        let done_bytes = (fraction as f64 * total as f64) as u64;
        let stopping = b.stop_after.load(Ordering::Relaxed) && !b.done;
        let (done, verified, failed, n) = (b.done, b.verified, b.failed, b.folders.len());
        let label = b.settings_label.clone();
        let phase = b.progress.phase.lock().unwrap().clone();
        let file = b.progress.current_file.lock().unwrap().clone();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| heading(ui, &title, &format!("{label} · verify after write")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if !done {
                        let b = self.jobs.batch.as_ref().unwrap();
                        let cancel = egui::Button::new(RichText::new("Cancel now").color(theme::danger()));
                        if ui.add(cancel).on_hover_text("Stops the current folder; its partial archive is removed").clicked() {
                            b.progress.cancel.store(true, Ordering::Relaxed);
                        }
                        let stop = egui::Button::new(RichText::new("Stop after this folder").color(theme::warn()));
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
                        ui.label(RichText::new(format!("{} of {}", human(done_bytes), human(total))).color(theme::muted()));
                    });
                });
                ui.add(egui::ProgressBar::new(fraction).desired_height(12.0).fill(theme::accent()));
                ui.add_space(6.0);
                let rate = done_bytes as f64 / elapsed;
                let eta = if done {
                    let word = if verified + failed < n { "Stopped" } else { "Done" };
                    format!("{word} in {}", duration(elapsed))
                } else if fraction > 0.01 {
                    duration(elapsed / fraction as f64 - elapsed)
                } else {
                    "…".into()
                };
                ui.columns(4, |c| {
                    stat(&mut c[0], if done { "Average throughput" } else { "Throughput" }, &format!("{:.0} MB/s", rate / 1e6));
                    stat(&mut c[1], if done { "Status" } else { "Time left" }, &eta);
                    stat(&mut c[2], "Folders", &format!("{} of {n}", verified + failed));
                    stat(&mut c[3], "Verified", &format!("{verified} · {failed} failed"));
                });
                if !done && !phase.is_empty() {
                    let stop = if stopping { " · stopping after this folder" } else { "" };
                    ui.label(RichText::new(format!("Now: {phase}{stop}")).color(theme::muted()));
                    if !file.is_empty() {
                        ui.add(egui::Label::new(RichText::new(&file).monospace().color(theme::muted())).truncate())
                            .on_hover_text(&file);
                    }
                }
            });
            ui.add_space(12.0);

            // Folders, then the log below it, both full width: side by side,
            // long status lines in the folder list ran under the log.
            card(ui, |ui| {
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
                            crate::pill(ui, &text, egui::Color32::WHITE, theme::accent());
                        } else if r.state == RowState::Idle {
                            ui.label(RichText::new("Not started").color(theme::muted()));
                        } else {
                            state_label(ui, &r.state, true);
                        }
                        ui.end_row();
                    }
                });
            });
            ui.add_space(12.0);

            let mut size = self.saved.log_text_size.clamp(LOG_TEXT_MIN, LOG_TEXT_MAX);
            egui::Frame::NONE
                .fill(theme::sidebar())
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Log").strong().size(16.0).color(egui::Color32::WHITE));
                        ui.add_space(12.0);
                        let mut detailed = self.saved.detailed_log;
                        let tick = ui
                            .checkbox(&mut detailed, RichText::new("Detailed: every file").color(theme::sidebar_text()))
                            .on_hover_text("Lists each phase and every file as it is hashed, compressed and verified. Takes effect at once, and is kept for next time.");
                        if tick.changed() {
                            self.saved.detailed_log = detailed;
                            settings::store(&self.saved);
                            if let Some(b) = &self.jobs.batch {
                                b.progress.detailed.store(detailed, Ordering::Relaxed);
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(size < LOG_TEXT_MAX, egui::Button::new("A+")).on_hover_text("Larger log text").clicked() {
                                size = (size + 1.0).min(LOG_TEXT_MAX);
                            }
                            ui.label(RichText::new(format!("{size:.0} pt")).color(theme::sidebar_text()));
                            if ui.add_enabled(size > LOG_TEXT_MIN, egui::Button::new("A−")).on_hover_text("Smaller log text").clicked() {
                                size = (size - 1.0).max(LOG_TEXT_MIN);
                            }
                        });
                    });
                    // Only the rows in view are drawn, so a long detailed log stays quick.
                    let row = ui.text_style_height(&egui::TextStyle::Monospace).max(size * 1.3);
                    let log = &self.jobs.log;
                    egui::ScrollArea::vertical().max_height(360.0).stick_to_bottom(true).auto_shrink([false, true]).show_rows(
                        ui,
                        row,
                        log.len(),
                        |ui, range| {
                            for l in &log[range] {
                                // One line each, so the rows stay even; the whole line shows on hover.
                                ui.add(egui::Label::new(RichText::new(l).monospace().size(size).color(theme::sidebar_text())).truncate())
                                    .on_hover_text(l);
                            }
                        },
                    );
                });
            if size != self.saved.log_text_size {
                self.saved.log_text_size = size;
                settings::store(&self.saved);
            }
        });
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).size(13.0).color(theme::muted()));
    ui.label(RichText::new(value).size(19.0).strong());
}
