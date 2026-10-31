//! Colours from the mockups, in a light and a dark palette. The sidebar is
//! dark in both. `set_dark` picks the palette for the whole app.

use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Color32, CornerRadius, Stroke};

static DARK: AtomicBool = AtomicBool::new(false);

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
    pick(0xF3F3EF, 0x16181B)
}
pub fn sidebar() -> Color32 {
    pick(0x1C1E21, 0x0E0F11)
}
pub fn sidebar_text() -> Color32 {
    rgb(0xE8E8E3)
}
pub fn card() -> Color32 {
    pick(0xFFFFFF, 0x23262B)
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
    pick(0x0B6B66, 0x0E7A74)
}
/// Accent used as text or a link on the page background.
pub fn accent_text() -> Color32 {
    pick(0x0B6B66, 0x6CCFC6)
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
