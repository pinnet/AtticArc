#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Attic desktop GUI, laid out like the "Archiver GUI mockups" canvas: a dark
//! sidebar with New job, Progress, Catalogue and Duplicates.
//!
//! The archive, verify and delete logic is foldarc's, now in attic-core.

mod catalogue;
mod duplicates;
mod jobs;
mod new_job;
mod progress;

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};

use attic_core::settings;

use jobs::Jobs;

/// Colours from the mockups.
pub mod theme {
    use eframe::egui::Color32;
    pub const GROUND: Color32 = Color32::from_rgb(0xF3, 0xF3, 0xEF);
    pub const SIDEBAR: Color32 = Color32::from_rgb(0x1C, 0x1E, 0x21);
    pub const SIDEBAR_TEXT: Color32 = Color32::from_rgb(0xE8, 0xE8, 0xE3);
    pub const CARD: Color32 = Color32::WHITE;
    pub const BORDER: Color32 = Color32::from_rgb(0xDA, 0xDA, 0xD3);
    pub const INK: Color32 = Color32::from_rgb(0x1C, 0x1E, 0x21);
    pub const MUTED: Color32 = Color32::from_rgb(0x5B, 0x5F, 0x66);
    pub const ACCENT: Color32 = Color32::from_rgb(0x0B, 0x6B, 0x66);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xE3, 0xF0, 0xEE);
    pub const OK: Color32 = Color32::from_rgb(0x24, 0x5A, 0x24);
    pub const OK_SOFT: Color32 = Color32::from_rgb(0xE3, 0xF0, 0xE3);
    pub const WARN: Color32 = Color32::from_rgb(0x9A, 0x3B, 0x00);
    pub const WARN_SOFT: Color32 = Color32::from_rgb(0xFF, 0xF4, 0xE5);
    pub const DANGER: Color32 = Color32::from_rgb(0xB4, 0x32, 0x32);
    pub const TRACK: Color32 = Color32::from_rgb(0xE6, 0xE6, 0xE0);
}

fn main() -> eframe::Result {
    let source_dir = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Attic")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Attic",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(App::new(source_dir)))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    NewJob,
    Progress,
    Catalogue,
    Duplicates,
}

impl Screen {
    const ALL: [Screen; 4] = [Screen::NewJob, Screen::Progress, Screen::Catalogue, Screen::Duplicates];

    fn label(self) -> &'static str {
        match self {
            Screen::NewJob => "New job",
            Screen::Progress => "Progress",
            Screen::Catalogue => "Catalogue",
            Screen::Duplicates => "Duplicates",
        }
    }
}

pub struct App {
    screen: Screen,
    saved: settings::Saved,
    source_dir: PathBuf,
    out_dir_text: String,
    protected_text: String,
    jobs: Jobs,
    catalogue: catalogue::State,
    duplicates: duplicates::State,
}

impl App {
    fn new(arg_dir: Option<PathBuf>) -> Self {
        let saved = settings::load();
        let source_dir = arg_dir
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
            jobs: Jobs::new(),
            catalogue: catalogue::State::default(),
            duplicates: duplicates::State::default(),
        };
        app.reload();
        app
    }

    fn out_dir(&self) -> PathBuf {
        PathBuf::from(self.out_dir_text.trim())
    }

    fn settings(&self) -> attic_core::archive::Settings {
        let mut s = self.saved.settings_for(&self.source_dir);
        s.out_dir = self.out_dir();
        s
    }

    fn save(&mut self) {
        self.saved.source_dir = Some(self.source_dir.clone());
        self.saved.out_dir = Some(self.out_dir());
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
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("Attic").size(18.0).strong().color(theme::SIDEBAR_TEXT));
        ui.add_space(16.0);
        for s in Screen::ALL {
            let on = self.screen == s;
            let mut text = RichText::new(s.label()).size(15.0).color(if on { Color32::WHITE } else { theme::SIDEBAR_TEXT });
            if on {
                text = text.strong();
            }
            if s == Screen::Progress && self.jobs.is_busy() {
                text = RichText::new(format!("{}  ●", s.label())).size(15.0).color(if on { Color32::WHITE } else { theme::SIDEBAR_TEXT });
            }
            let button = egui::Button::new(text)
                .fill(if on { theme::ACCENT } else { Color32::TRANSPARENT })
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::same(6))
                .min_size(egui::vec2(ui.available_width(), 40.0));
            if ui.add(button).clicked() {
                self.screen = s;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let out = self.out_dir();
        let finished_batch = self.jobs.handle_messages(&out);
        if finished_batch {
            self.catalogue.invalidate();
            self.duplicates.invalidate();
        }
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(200.0)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(Margin::symmetric(14, 18)))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(theme::GROUND).inner_margin(Margin::symmetric(32, 26)))
            .show(ui, |ui| match self.screen {
                Screen::NewJob => self.new_job(ui),
                Screen::Progress => self.progress(ui),
                Screen::Catalogue => self.catalogue(ui),
                Screen::Duplicates => self.duplicates(ui),
            });
        let ctx = ui.ctx().clone();
        let settings = self.settings();
        self.jobs.dialogs(&ctx, &settings);
        if self.jobs.needs_repaint() || self.catalogue.busy() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.panel_fill = theme::GROUND;
        v.window_fill = theme::CARD;
        v.extreme_bg_color = Color32::WHITE;
        v.faint_bg_color = Color32::from_rgb(0xF7, 0xF7, 0xF3);
        v.hyperlink_color = theme::ACCENT;
        v.selection.bg_fill = theme::ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme::INK);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme::BORDER);
        v.widgets.inactive.corner_radius = CornerRadius::same(6);
        v.widgets.hovered.corner_radius = CornerRadius::same(6);
        v.widgets.active.corner_radius = CornerRadius::same(6);
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style.spacing.interact_size.y = 30.0;
    });
}

// Shared drawing helpers.

/// A white card with the mockups' border and radius.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(26.0).strong().color(theme::INK));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(theme::MUTED));
    }
    ui.add_space(10.0);
}

pub fn caption(text: &str) -> RichText {
    RichText::new(text.to_uppercase()).size(12.0).strong().color(theme::MUTED)
}

pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).strong().color(Color32::WHITE))
        .fill(theme::ACCENT)
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
