//! The "quantum" theme (GUI-1606): a deep-space dark palette, cyan→violet
//! accents, rounded cards and no trace of egui's default grey.
//!
//! Every colour used anywhere in the UI is named here; views never invent one.

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Vec2, Visuals,
};
use std::sync::atomic::{AtomicBool, Ordering};

static MOTION_ENABLED: AtomicBool = AtomicBool::new(true);

/// Page background — deep space.
pub const BG_DEEP: Color32 = Color32::from_rgb(0x05, 0x08, 0x12);
/// Header / footer panels, one step above the page.
pub const BG_PANEL: Color32 = Color32::from_rgb(0x09, 0x0e, 0x1b);
/// Card surface and its hovered variant.
pub const CARD: Color32 = Color32::from_rgb(0x0e, 0x16, 0x28);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x15, 0x22, 0x39);
/// Inset surfaces: the log pane, text fields, code.
pub const INSET: Color32 = Color32::from_rgb(0x06, 0x09, 0x12);

pub const STROKE: Color32 = Color32::from_rgb(0x1d, 0x2a, 0x48);
pub const STROKE_STRONG: Color32 = Color32::from_rgb(0x2b, 0x3d, 0x63);

pub const TEXT: Color32 = Color32::from_rgb(0xdd, 0xe6, 0xf7);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8b, 0x9c, 0xbd);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5d, 0x6b, 0x8a);

/// The entanglement gradient: cyan at one end, violet at the other.
pub const CYAN: Color32 = Color32::from_rgb(0x35, 0xe2, 0xf0);
pub const CYAN_DEEP: Color32 = Color32::from_rgb(0x18, 0x9c, 0xb8);
pub const VIOLET: Color32 = Color32::from_rgb(0xa8, 0x6b, 0xff);
pub const VIOLET_DEEP: Color32 = Color32::from_rgb(0x6d, 0x3f, 0xc4);

pub const OK: Color32 = Color32::from_rgb(0x3d, 0xdc, 0x97);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0xb4, 0x54);
pub const ERR: Color32 = Color32::from_rgb(0xff, 0x5d, 0x73);

pub const CARD_RADIUS: u8 = 14;
pub const CONTROL_RADIUS: u8 = 9;
/// Card size in the grid.
pub const CARD_WIDTH: f32 = 362.0;

pub fn set_motion_enabled(enabled: bool) {
    MOTION_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn motion_enabled() -> bool {
    MOTION_ENABLED.load(Ordering::Relaxed)
}

pub fn animation_time(ctx: &egui::Context) -> f64 {
    if motion_enabled() {
        ctx.input(|i| i.time)
    } else {
        0.0
    }
}

pub fn animate_bool(ctx: &egui::Context, id: egui::Id, target: bool, seconds: f32) -> f32 {
    if motion_enabled() {
        ctx.animate_bool_with_time(id, target, seconds)
    } else if target {
        1.0
    } else {
        0.0
    }
}

/// Sparse technical grid plus one low-contrast scan line. It is deliberately
/// painter-only: no texture uploads, shaders or allocations that survive a frame.
pub fn paint_backdrop(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let grid = STROKE.gamma_multiply(0.18);
    let step = 48.0;
    let mut x = rect.left() - rect.left().rem_euclid(step);
    while x <= rect.right() {
        painter.vline(x, rect.y_range(), Stroke::new(0.5_f32, grid));
        x += step;
    }
    let mut y = rect.top() - rect.top().rem_euclid(step);
    while y <= rect.bottom() {
        painter.hline(rect.x_range(), y, Stroke::new(0.5_f32, grid));
        y += step;
    }
    if motion_enabled() {
        let time = ui.input(|i| i.time) as f32;
        let travel = (time * 18.0).rem_euclid(rect.height() + 120.0) - 60.0;
        painter.hline(
            rect.x_range(),
            rect.top() + travel,
            Stroke::new(1.0_f32, CYAN.gamma_multiply(0.055)),
        );
    }
}

/// Installs the theme on a fresh egui context.
pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(23.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.5, FontFamily::Monospace),
        ),
    ]
    .into();

    let mut visuals = Visuals::dark();
    visuals.dark_mode = true;
    // No `override_text_color`: it would also repaint hint text and disabled
    // labels in full-strength body colour. The body colour comes from
    // `widgets.noninteractive.fg_stroke` below, and egui derives its weak
    // variants from it.
    visuals.panel_fill = BG_DEEP;
    visuals.window_fill = BG_PANEL;
    visuals.faint_bg_color = Color32::from_rgb(0x0e, 0x14, 0x27);
    visuals.extreme_bg_color = INSET;
    visuals.code_bg_color = INSET;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = ERR;
    visuals.hyperlink_color = CYAN;
    visuals.window_stroke = Stroke::new(1.0_f32, STROKE_STRONG);
    visuals.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    visuals.menu_corner_radius = CornerRadius::same(CONTROL_RADIUS);
    visuals.window_shadow = Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(180),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(160),
    };
    visuals.selection.bg_fill = CYAN_DEEP.linear_multiply(0.55);
    visuals.selection.stroke = Stroke::new(1.0_f32, CYAN);
    visuals.slider_trailing_fill = true;

    // Widgets: dark bodies, cyan-lit edges on interaction.
    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, STROKE);
    // Body text colour, and the base egui greys out for hints and disabled bits.
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = CornerRadius::same(CONTROL_RADIUS);

    w.inactive.bg_fill = Color32::from_rgb(0x16, 0x1f, 0x38);
    w.inactive.weak_bg_fill = Color32::from_rgb(0x14, 0x1c, 0x33);
    w.inactive.bg_stroke = Stroke::new(1.0_f32, STROKE);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = CornerRadius::same(CONTROL_RADIUS);
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = Color32::from_rgb(0x1e, 0x2b, 0x4c);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x1a, 0x25, 0x42);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, CYAN_DEEP);
    w.hovered.fg_stroke = Stroke::new(1.2_f32, Color32::WHITE);
    w.hovered.corner_radius = CornerRadius::same(CONTROL_RADIUS);
    w.hovered.expansion = 1.0;

    w.active.bg_fill = Color32::from_rgb(0x25, 0x36, 0x5e);
    w.active.weak_bg_fill = Color32::from_rgb(0x20, 0x2e, 0x52);
    w.active.bg_stroke = Stroke::new(1.2_f32, CYAN);
    w.active.fg_stroke = Stroke::new(1.4_f32, Color32::WHITE);
    w.active.corner_radius = CornerRadius::same(CONTROL_RADIUS);
    w.active.expansion = 1.0;

    w.open.bg_fill = CARD_HOVER;
    w.open.weak_bg_fill = CARD_HOVER;
    w.open.bg_stroke = Stroke::new(1.0_f32, STROKE_STRONG);
    w.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.open.corner_radius = CornerRadius::same(CONTROL_RADIUS);

    style.visuals = visuals;

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(9.0, 9.0);
    s.button_padding = Vec2::new(12.0, 6.0);
    s.window_margin = Margin::same(18);
    s.menu_margin = Margin::same(8);
    s.interact_size = Vec2::new(44.0, 26.0);
    s.slider_width = 190.0;
    s.combo_width = 168.0;
    s.text_edit_width = 260.0;
    s.scroll.bar_width = 9.0;
    s.scroll.floating = false;

    ctx.set_style(style);
}

/// A horizontal cyan→violet gradient. egui has no gradient brush, so this is a
/// two-triangle mesh with interpolated vertex colours.
pub fn gradient_rect(painter: &egui::Painter, rect: egui::Rect, left: Color32, right: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// Linear blend between two colours, gamma-space (good enough for accents and
/// what egui's own colour picker does).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

/// Position along the accent gradient, `0.0` cyan … `1.0` violet.
pub fn accent(t: f32) -> Color32 {
    mix(CYAN, VIOLET, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_interpolates_and_clamps() {
        assert_eq!(mix(CYAN, VIOLET, 0.0), CYAN);
        assert_eq!(mix(CYAN, VIOLET, 1.0), VIOLET);
        assert_eq!(mix(CYAN, VIOLET, -3.0), CYAN);
        assert_eq!(mix(CYAN, VIOLET, 7.0), VIOLET);
        let middle = mix(Color32::BLACK, Color32::WHITE, 0.5);
        assert_eq!((middle.r(), middle.g(), middle.b()), (128, 128, 128));
    }

    #[test]
    fn the_palette_is_dark_and_not_egui_grey() {
        // Guard against an accidental `Visuals::dark()` regression: the page
        // background must stay in the near-black blue range.
        assert!(BG_DEEP.r() < 0x20 && BG_DEEP.b() > BG_DEEP.r());
        assert!(CARD.b() > CARD.g() && CARD.g() >= CARD.r());
        assert!(accent(0.5).r() > 0x50 && accent(0.5).b() > 0xc0);
    }
}
