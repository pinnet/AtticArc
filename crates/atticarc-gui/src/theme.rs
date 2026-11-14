//! Colours from the mockups, in a light and a dark palette. The sidebar is
//! dark in both. `set_dark` picks the palette for the whole app, and
//! `set_accent` the colour of selected and primary controls ([`ACCENTS`]).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use eframe::egui::{self, Color32, CornerRadius, Stroke};

static DARK: AtomicBool = AtomicBool::new(false);
static ACCENT: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND: AtomicUsize = AtomicUsize::new(0);

/// A colour for selected and primary controls: `fill` under white text,
/// `text` as text or a link on the page; each `[light, dark]`.
pub struct Accent {
    pub name: &'static str,
    fill: [u32; 2],
    text: [u32; 2],
}

impl Accent {
    /// The fill in the palette in use, for a swatch.
    pub fn swatch_colour(&self) -> Color32 {
        rgb(self.fill[usize::from(is_dark())])
    }
}

/// The colours offered in Settings. The first is the default. Every fill
/// is dark enough for white text in both palettes.
pub const ACCENTS: [Accent; 6] = [
    Accent { name: "Teal", fill: [0x0B6B66, 0x0E7A74], text: [0x0B6B66, 0x6CCFC6] },
    // Amber is gold and Sunset raspberry, so the two never look alike (nor like the red of danger).
    Accent { name: "Amber", fill: [0x856200, 0x977208], text: [0x765600, 0xF0C24A] },
    Accent { name: "Sunset", fill: [0xA8325E, 0xB8406E], text: [0x9C2C56, 0xF39AC0] },
    Accent { name: "Blue", fill: [0x1F5FAF, 0x2B6CC4], text: [0x1F5FAF, 0x8DB8F0] },
    Accent { name: "Plum", fill: [0x6A3FA0, 0x7B50B4], text: [0x6A3FA0, 0xC3A6EE] },
    Accent { name: "Slate", fill: [0x4A5560, 0x56626E], text: [0x3E4852, 0xB8C2CC] },
];

/// A background: the page and the sidebar in each palette (`[light, dark]`),
/// and the cards in the dark palette (light cards stay white).
pub struct Background {
    pub name: &'static str,
    ground: [u32; 2],
    sidebar: [u32; 2],
    card_dark: u32,
}

impl Background {
    /// The page colour in the palette in use, for a swatch.
    pub fn swatch_colour(&self) -> Color32 {
        rgb(self.ground[usize::from(is_dark())])
    }
}

/// The backgrounds offered in Settings. The first is the default. Each is a
/// light tint (or, in dark mode, a dark one) of the same hue, so text and
/// accents read the same on all of them.
pub const BACKGROUNDS: [Background; 6] = [
    Background { name: "Stone", ground: [0xF3F3EF, 0x16181B], sidebar: [0x1C1E21, 0x0E0F11], card_dark: 0x23262B },
    Background { name: "Paper", ground: [0xF6F0E4, 0x1B1813], sidebar: [0x26211A, 0x100E0B], card_dark: 0x29251E },
    Background { name: "Mist", ground: [0xECF1F6, 0x141A20], sidebar: [0x1A222B, 0x0B0F14], card_dark: 0x202933 },
    Background { name: "Sage", ground: [0xEDF2EA, 0x151B16], sidebar: [0x1B241D, 0x0C110D], card_dark: 0x212B23 },
    Background { name: "Blush", ground: [0xF6EEF0, 0x1C1618], sidebar: [0x271D21, 0x110C0E], card_dark: 0x2B2226 },
    Background { name: "Snow", ground: [0xFBFBFA, 0x111111], sidebar: [0x202020, 0x080808], card_dark: 0x1D1D1D },
];

/// The background with this name (ignoring case), or the default.
pub fn background_index(name: &str) -> usize {
    BACKGROUNDS.iter().position(|b| b.name.eq_ignore_ascii_case(name)).unwrap_or(0)
}

fn current_background() -> &'static Background {
    &BACKGROUNDS[BACKGROUND.load(Ordering::Relaxed).min(BACKGROUNDS.len() - 1)]
}

/// Picks the background by name and restyles egui to match.
pub fn set_background(ctx: &egui::Context, name: &str) {
    BACKGROUND.store(background_index(name), Ordering::Relaxed);
    set_dark(ctx, is_dark());
}

/// The accent with this name (ignoring case), or the default.
pub fn accent_index(name: &str) -> usize {
    ACCENTS.iter().position(|a| a.name.eq_ignore_ascii_case(name)).unwrap_or(0)
}

fn current() -> &'static Accent {
    &ACCENTS[ACCENT.load(Ordering::Relaxed).min(ACCENTS.len() - 1)]
}

/// Picks the accent by name and restyles egui to match.
pub fn set_accent(ctx: &egui::Context, name: &str) {
    ACCENT.store(accent_index(name), Ordering::Relaxed);
    set_dark(ctx, is_dark());
}

/// A small square of `colour`, edged so a pale one shows on a white card,
/// and ringed when it is the one in use.
pub fn swatch(ui: &mut egui::Ui, colour: Color32, name: &str, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(34.0, 26.0), egui::Sense::click());
    let painter = ui.painter();
    let inner = rect.shrink(3.0);
    painter.rect_filled(inner, 5, colour);
    painter.rect_stroke(inner, 5, Stroke::new(1.0, border()), egui::StrokeKind::Inside);
    let ring = if selected {
        Stroke::new(2.0, ink())
    } else if response.hovered() {
        Stroke::new(1.0, muted())
    } else {
        Stroke::NONE
    };
    painter.rect_stroke(rect, 7, ring, egui::StrokeKind::Inside);
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(name)
}

pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn pick(light: u32, dark: u32) -> Color32 {
    rgb(if is_dark() { dark } else { light })
}

pub fn ground() -> Color32 {
    let b = current_background();
    pick(b.ground[0], b.ground[1])
}
pub fn sidebar() -> Color32 {
    let b = current_background();
    pick(b.sidebar[0], b.sidebar[1])
}
pub fn sidebar_text() -> Color32 {
    rgb(0xE8E8E3)
}
pub fn card() -> Color32 {
    pick(0xFFFFFF, current_background().card_dark)
}
pub fn border() -> Color32 {
    pick(0xDADAD3, 0x3A3E45)
}
pub fn ink() -> Color32 {
    pick(0x1C1E21, 0xEDEDE8)
}
pub fn muted() -> Color32 {
    pick(0x5B5F66, 0xADB2BA)
}
/// Fill for selected and primary controls; white text sits on it in both palettes.
pub fn accent() -> Color32 {
    let a = current();
    pick(a.fill[0], a.fill[1])
}
/// Accent used as text or a link on the page background.
pub fn accent_text() -> Color32 {
    let a = current();
    pick(a.text[0], a.text[1])
}
pub fn ok() -> Color32 {
    pick(0x245A24, 0xA8DDA8)
}
pub fn ok_soft() -> Color32 {
    pick(0xE3F0E3, 0x1F3A23)
}
pub fn warn() -> Color32 {
    pick(0x9A3B00, 0xF2B66D)
}
pub fn warn_soft() -> Color32 {
    pick(0xFFF4E5, 0x3A2A15)
}
/// Danger as text (the X, Cancel now).
pub fn danger() -> Color32 {
    pick(0xB43232, 0xF08A8A)
}
/// Danger as a button fill under white text.
pub fn danger_fill() -> Color32 {
    rgb(0xB43232)
}
/// Neutral pill background ("Ready", "Queued").
pub fn neutral_soft() -> Color32 {
    pick(0xECECE6, 0x33373D)
}

/// Switches the palette and restyles egui to match.
pub fn set_dark(ctx: &egui::Context, dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
    let theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_theme(theme);
    ctx.style_mut_of(theme, |style| {
        let v = &mut style.visuals;
        v.panel_fill = ground();
        v.window_fill = card();
        v.extreme_bg_color = if dark { rgb(0x1A1C20) } else { Color32::WHITE };
        v.faint_bg_color = pick(0xF7F7F3, 0x2A2E33);
        v.hyperlink_color = accent_text();
        v.selection.bg_fill = accent();
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, ink());
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border());
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
            w.corner_radius = CornerRadius::same(6);
        }
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style.spacing.interact_size.y = 30.0;
    });
}
