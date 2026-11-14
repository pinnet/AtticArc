#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! AtticARC desktop GUI, laid out like the "Archiver GUI mockups" canvas: a dark
//! sidebar with New job, Progress, Catalogue and Duplicates.
//!
//! The archive, verify and delete logic is foldarc's, now in atticarc-core.

mod browse;
mod catalogue;
mod cleanup;
mod dialog;
mod duplicates;
mod excluded;
mod jobs;
mod log_file;
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
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let source_dir = args.iter().find(|a| !a.to_string_lossy().starts_with("--")).map(PathBuf::from);
    // OpenGL if asked for (setting, --compatible-graphics, or ATTICARC_RENDERER=glow):
    // some setups recover better from displays sleeping and waking with it.
    let glow = settings::load().compatible_graphics
        || flag("--compatible-graphics")
        || std::env::var("ATTICARC_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("glow"));
    log_file::start();
    // DirectX 12 on Windows rather than whatever the driver offers first (often
    // Vulkan): it copes better with displays sleeping, waking and changing mode.
    // WGPU_BACKEND set by hand still wins.
    #[cfg(windows)]
    if std::env::var_os("WGPU_BACKEND").is_none() {
        // SAFETY: no other thread has started yet.
        unsafe { std::env::set_var("WGPU_BACKEND", "dx12") };
    }
    log::info!(
        "AtticARC {} starting; renderer {}, WGPU_BACKEND={}",
        env!("CARGO_PKG_VERSION"),
        if glow { "OpenGL (compatible graphics)" } else { "wgpu" },
        std::env::var("WGPU_BACKEND").unwrap_or_default()
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AtticARC")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 480.0]),
        renderer: if glow { eframe::Renderer::Glow } else { eframe::Renderer::Wgpu },
        // Don't wait for vsync: with the default, the renderer blocks waiting
        // for the display to take a frame, and a display asleep or changing
        // mode can leave it waiting for ever, freezing the window. AtticARC
        // only redraws when something changes, so this costs nothing.
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            // egui-wgpu 0.37 moved the present mode into `surface`.
            surface: eframe::egui_wgpu::SurfaceConfig {
                present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
                ..eframe::egui_wgpu::SurfaceConfig::HIGH_THROUGHPUT
            },
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "AtticARC",
        options,
        Box::new(move |cc| {
            let app = App::new(source_dir);
            theme::set_accent(&cc.egui_ctx, &app.saved.accent);
            theme::set_background(&cc.egui_ctx, &app.saved.background);
            theme::set_dark(&cc.egui_ctx, app.saved.dark_mode);
            Ok(Box::new(app))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    NewJob,
    Progress,
    Duplicates,
    Excluded,
    Cleanup,
    Catalogue,
}

impl Screen {
    /// The steps of the work, in the order they are done.
    const WORKFLOW: [Screen; 5] = [Screen::NewJob, Screen::Progress, Screen::Duplicates, Screen::Excluded, Screen::Cleanup];
    /// Separate from the work: looking things up in what has been archived.
    const LOOK_UP: [Screen; 1] = [Screen::Catalogue];

    fn label(self) -> &'static str {
        match self {
            Screen::NewJob => "New job",
            Screen::Progress => "Progress",
            Screen::Catalogue => "Catalogue",
            Screen::Duplicates => "Duplicates",
            Screen::Excluded => "Excluded",
            Screen::Cleanup => "Clean up",
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
    cleanup: cleanup::State,
    /// The display the window was last drawn for: its scale and monitor size.
    /// When it changes (a display waking with another resolution), the
    /// window is resized to make it redraw at the new scale.
    last_display: Option<(Option<f32>, Option<egui::Vec2>)>,
    /// The size to go back to after a nudge, the frame after.
    unnudge: Option<egui::Vec2>,
    /// The Settings window is open.
    settings_open: bool,
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
            cleanup: cleanup::State::default(),
            last_display: None,
            unnudge: None,
            settings_open: false,
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
        self.cleanup.invalidate();
    }

    /// Makes the window redraw at the right size and scale: when the display
    /// it is on changes (such as a monitor waking with another resolution or
    /// scale), or when asked (F5, or Refresh window). The window is nudged a
    /// pixel larger and back, which makes Windows and the renderer start
    /// afresh with the current scale.
    fn heal_window(&mut self, ctx: &egui::Context, asked: bool) {
        if let Some(size) = self.unnudge.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.request_repaint();
        }
        let (now, inner) = ctx.input(|i| {
            let v = i.viewport();
            ((v.native_pixels_per_point, v.monitor_size), v.inner_rect)
        });
        let changed = self.last_display.is_some_and(|before| before != now);
        if changed {
            log::info!("display changed from {:?} to {now:?}; redrawing at the new size", self.last_display);
        }
        self.last_display = Some(now);
        if (asked || changed)
            && let Some(r) = inner
        {
            let size = r.size();
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size + egui::vec2(1.0, 1.0)));
            self.unnudge = Some(size);
            ctx.request_repaint();
        }
    }

    fn nav_button(&mut self, ui: &mut egui::Ui, s: Screen) {
        let on = self.screen == s;
        let colour = if on { Color32::WHITE } else { theme::sidebar_text() };
        let busy = match s {
            Screen::Progress => self.jobs.is_busy(),
            Screen::Cleanup => self.cleanup.busy(),
            _ => false,
        };
        let label = if busy { format!("{}  ●", s.label()) } else { s.label().to_string() };
        let mut text = RichText::new(label).size(15.0).color(colour);
        if on {
            text = text.strong();
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

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("AtticARC").size(18.0).strong().color(theme::sidebar_text()));
        ui.add_space(16.0);
        for s in Screen::WORKFLOW {
            self.nav_button(ui, s);
        }
        // The catalogue is apart from the steps of the work: it is for
        // finding things in what has been archived.
        ui.add_space(14.0);
        ui.add(egui::Separator::default().spacing(10.0));
        ui.label(RichText::new("Look up").size(12.5).color(theme::sidebar_text().gamma_multiply(0.7)));
        ui.add_space(4.0);
        for s in Screen::LOOK_UP {
            self.nav_button(ui, s);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            let on = self.settings_open;
            let button = egui::Button::new(RichText::new("⚙  Settings").size(15.0).color(if on { Color32::WHITE } else { theme::sidebar_text() }))
                .fill(if on { theme::accent() } else { Color32::TRANSPARENT })
                .stroke(Stroke::new(1.0, theme::sidebar_text().gamma_multiply(0.4)))
                .corner_radius(CornerRadius::same(6))
                .min_size(egui::vec2(ui.available_width(), 40.0));
            if ui.add(button).on_hover_text("Light or dark, colour, graphics and the log file").clicked() {
                self.settings_open = !on;
            }
        });
    }

    /// The Settings window: how AtticARC looks and draws, and its log file.
    /// Each change is saved as it is made.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut refresh = false;
        egui::Window::new(RichText::new("Settings").strong())
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(214.0, -18.0))
            .frame(
                egui::Frame::NONE
                    .fill(theme::card())
                    .stroke(Stroke::new(1.0, theme::border()))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(18)),
            )
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(caption("Appearance"));
                ui.horizontal(|ui| {
                    row_label(ui, "Mode");
                    let mut dark = self.saved.dark_mode;
                    ui.selectable_value(&mut dark, false, "Light");
                    ui.selectable_value(&mut dark, true, "Dark");
                    if dark != self.saved.dark_mode {
                        self.saved.dark_mode = dark;
                        theme::set_dark(ui.ctx(), dark);
                        settings::store(&self.saved);
                    }
                });
                ui.horizontal(|ui| {
                    row_label(ui, "Colour");
                    let now = theme::accent_index(&self.saved.accent);
                    for (i, a) in theme::ACCENTS.iter().enumerate() {
                        if theme::swatch(ui, a.swatch_colour(), a.name, i == now).clicked() && i != now {
                            self.saved.accent = a.name.to_string();
                            theme::set_accent(ui.ctx(), a.name);
                            settings::store(&self.saved);
                        }
                    }
                    ui.label(RichText::new(theme::ACCENTS[now].name).color(theme::muted()));
                });
                ui.horizontal(|ui| {
                    row_label(ui, "Background");
                    let now = theme::background_index(&self.saved.background);
                    for (i, b) in theme::BACKGROUNDS.iter().enumerate() {
                        if theme::swatch(ui, b.swatch_colour(), b.name, i == now).clicked() && i != now {
                            self.saved.background = b.name.to_string();
                            theme::set_background(ui.ctx(), b.name);
                            settings::store(&self.saved);
                        }
                    }
                    ui.label(RichText::new(theme::BACKGROUNDS[now].name).color(theme::muted()));
                });
                ui.add_space(10.0);
                ui.label(caption("Display"));
                let mut compat = self.saved.compatible_graphics;
                let tip = "Draw with OpenGL instead of the default. Try it if the window freezes or draws at the wrong size \
                           after a display sleeps and wakes. Takes effect when AtticARC next starts.";
                if ui.checkbox(&mut compat, "Compatible graphics (from the next start)").on_hover_text(tip).changed() {
                    self.saved.compatible_graphics = compat;
                    settings::store(&self.saved);
                }
                if ui.button("Refresh window (F5)").on_hover_text("Redraws the window at the right size, if it looks wrong after a display wakes").clicked() {
                    refresh = true;
                }
                ui.add_space(10.0);
                ui.label(caption("Log"));
                let mut detailed = self.saved.detailed_log;
                if ui
                    .checkbox(&mut detailed, "Detailed: every file in the Progress log")
                    .on_hover_text("Lists each phase and every file as it is hashed, compressed and verified.")
                    .changed()
                {
                    self.saved.detailed_log = detailed;
                    settings::store(&self.saved);
                    if let Some(b) = &self.jobs.batch {
                        b.progress.detailed.store(detailed, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if ui.button("Open the log file").on_hover_text(log_file::path().display().to_string()).clicked() {
                    log_file::open();
                }
            });
        if !open {
            self.settings_open = false;
        }
        if refresh {
            self.heal_window(ctx, true);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        dialog::set_parent(frame);
        let ctx = ui.ctx().clone();
        let asked = ctx.input(|i| i.key_pressed(egui::Key::F5));
        self.heal_window(&ctx, asked);
        let out = self.out_dir();
        let finished_batch = self.jobs.handle_messages(&out);
        if std::mem::take(&mut self.catalogue.changed) {
            self.duplicates.invalidate();
            self.excluded.invalidate();
            self.cleanup.invalidate();
        }
        if finished_batch || std::mem::take(&mut self.jobs.deleted) {
            self.catalogue.invalidate();
            self.duplicates.invalidate();
            self.excluded.invalidate();
            self.cleanup.invalidate();
        }
        if std::mem::take(&mut self.cleanup.changed) {
            // Folders were deleted: the folder list and the other pages reload.
            self.reload();
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
                Screen::Cleanup => self.cleanup(ui),
            });
        self.settings_window(&ctx);
        let settings = self.settings();
        self.jobs.dialogs(&ctx, &settings);
        if self.jobs.needs_repaint() || self.catalogue.busy() || self.excluded.busy() || self.duplicates.busy() || self.cleanup.busy() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

// Shared drawing helpers.

/// A label in a fixed-width cell, so the controls after it line up row by row.
fn row_label(ui: &mut egui::Ui, text: &str) {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(egui::vec2(92.0, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(92.0);
        ui.label(text);
    });
}

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
