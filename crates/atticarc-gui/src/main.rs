#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! AtticARC desktop GUI, laid out like the "Archiver GUI mockups" canvas: a dark
//! sidebar with New job, Progress, Catalogue and Duplicates.
//!
//! The archive, verify and delete logic is foldarc's, now in atticarc-core.

mod browse;
mod catalogue;
mod dialog;
mod duplicates;
mod excluded;
mod jobs;
mod new_job;
mod progress;
mod theme;

/// mimalloc hands freed memory back to the system, so memory stays flat
/// over hours of archiving; the system allocator kept growing in the soak
/// test as many threads freed and reused large buffers.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};

use atticarc_core::settings;

use jobs::Jobs;

fn main() -> eframe::Result {
    let source_dir = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AtticARC")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AtticARC",
        options,
        Box::new(move |cc| {
            let app = App::new(source_dir);
            theme::set_dark(&cc.egui_ctx, app.saved.dark_mode);
            Ok(Box::new(app))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    NewJob,
    Progress,
    Catalogue,
    Duplicates,
    Excluded,
}

impl Screen {
    const ALL: [Screen; 5] = [Screen::NewJob, Screen::Progress, Screen::Catalogue, Screen::Duplicates, Screen::Excluded];

    fn label(self) -> &'static str {
        match self {
            Screen::NewJob => "New job",
            Screen::Progress => "Progress",
            Screen::Catalogue => "Catalogue",
            Screen::Duplicates => "Duplicates",
            Screen::Excluded => "Excluded",
        }
    }
}

pub struct App {
    screen: Screen,
    saved: settings::Saved,
    source_dir: PathBuf,
    out_dir_text: String,
    protected_text: String,
    /// When a folder dialog last closed, so its stray clicks can be ignored.
    picker_closed: Option<std::time::Instant>,
    jobs: Jobs,
    catalogue: catalogue::State,
    duplicates: duplicates::State,
    excluded: excluded::State,
}

impl App {
    fn new(arg_dir: Option<PathBuf>) -> Self {
        let saved = settings::load();
        // The folder given on the command line, else the last one used, else
        // the most recent one still there (the last may be on a drive that
        // isn't plugged in).
        let source_dir = arg_dir
            .or_else(|| saved.source_dir.clone().filter(|p| p.is_dir()))
            .or_else(|| saved.recent_sources.iter().find(|p| p.is_dir()).cloned())
            .or_else(|| saved.source_dir.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let source_dir = std::fs::canonicalize(&source_dir).map(|p| settings::strip_verbatim(&p)).unwrap_or(source_dir);
        let out_dir = saved.out_dir.clone().unwrap_or_else(|| settings::default_out_dir(&source_dir));
        let protected_text = saved.protected.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        let mut app = Self {
            screen: Screen::NewJob,
            saved,
            source_dir,
            out_dir_text: out_dir.display().to_string(),
            protected_text,
            picker_closed: None,
            jobs: Jobs::new(),
            catalogue: catalogue::State::default(),
            duplicates: duplicates::State::default(),
            excluded: excluded::State::default(),
        };
        app.reload();
        app
    }

    fn out_dir(&self) -> PathBuf {
        PathBuf::from(self.out_dir_text.trim())
    }

    fn settings(&self) -> atticarc_core::archive::Settings {
        let mut s = self.saved.settings_for(&self.source_dir);
        s.out_dir = self.out_dir();
        s
    }

    fn save(&mut self) {
        let out = self.out_dir();
        self.saved.source_dir = Some(self.source_dir.clone());
        settings::remember(&mut self.saved.recent_sources, &self.source_dir);
        settings::remember(&mut self.saved.recent_dests, &out);
        self.saved.out_dir = Some(out);
        self.saved.protected =
            self.protected_text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
        settings::store(&self.saved);
    }

    /// Re-lists the source folder and drops cached catalogue views.
    fn reload(&mut self) {
        let out = self.out_dir();
        self.jobs.reload(&self.source_dir, &out);
        self.catalogue.invalidate();
        self.duplicates.invalidate();
        self.excluded.invalidate();
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("AtticARC").size(18.0).strong().color(theme::sidebar_text()));
        ui.add_space(16.0);
        for s in Screen::ALL {
            let on = self.screen == s;
            let mut text = RichText::new(s.label()).size(15.0).color(if on { Color32::WHITE } else { theme::sidebar_text() });
            if on {
                text = text.strong();
            }
            if s == Screen::Progress && self.jobs.is_busy() {
                text = RichText::new(format!("{}  ●", s.label())).size(15.0).color(if on { Color32::WHITE } else { theme::sidebar_text() });
            }
            let button = egui::Button::new(text)
                .fill(if on { theme::accent() } else { Color32::TRANSPARENT })
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::same(6))
                .min_size(egui::vec2(ui.available_width(), 40.0));
            if ui.add(button).clicked() {
                self.screen = s;
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            let dark = self.saved.dark_mode;
            let label = if dark { "Light mode" } else { "Dark mode" };
            let button = egui::Button::new(RichText::new(label).size(15.0).color(theme::sidebar_text()))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.0, theme::sidebar_text().gamma_multiply(0.4)))
                .corner_radius(CornerRadius::same(6))
                .min_size(egui::vec2(ui.available_width(), 40.0));
            if ui.add(button).on_hover_text("Switch between light and dark colours").clicked() {
                self.saved.dark_mode = !dark;
                theme::set_dark(ui.ctx(), !dark);
                settings::store(&self.saved);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        dialog::set_parent(frame);
        let out = self.out_dir();
        let finished_batch = self.jobs.handle_messages(&out);
        if std::mem::take(&mut self.catalogue.changed) {
            self.duplicates.invalidate();
            self.excluded.invalidate();
        }
        if finished_batch || std::mem::take(&mut self.jobs.deleted) {
            self.catalogue.invalidate();
            self.duplicates.invalidate();
            self.excluded.invalidate();
        }
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(200.0)
            .frame(egui::Frame::NONE.fill(theme::sidebar()).inner_margin(Margin::symmetric(14, 18)))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(theme::ground()).inner_margin(Margin::symmetric(32, 26)))
            .show(ui, |ui| match self.screen {
                Screen::NewJob => self.new_job(ui),
                Screen::Progress => self.progress(ui),
                Screen::Catalogue => self.catalogue(ui),
                Screen::Duplicates => self.duplicates(ui),
                Screen::Excluded => self.excluded(ui),
            });
        let ctx = ui.ctx().clone();
        let settings = self.settings();
        self.jobs.dialogs(&ctx, &settings);
        if self.jobs.needs_repaint() || self.catalogue.busy() || self.excluded.busy() || self.duplicates.busy() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

// Shared drawing helpers.

/// A white card with the mockups' border and radius.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(theme::card())
        .stroke(Stroke::new(1.0, theme::border()))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(26.0).strong().color(theme::ink()));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(theme::muted()));
    }
    ui.add_space(10.0);
}

pub fn caption(text: &str) -> RichText {
    RichText::new(text.to_uppercase()).size(12.0).strong().color(theme::muted())
}

pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).strong().color(Color32::WHITE))
        .fill(theme::accent())
        .min_size(egui::vec2(0.0, 40.0))
}

/// A pill-shaped status tag like the mockups' "Verified" / "Queued".
pub fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| ui.label(RichText::new(text).size(13.0).color(fg)))
        .response
}

pub fn human(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
