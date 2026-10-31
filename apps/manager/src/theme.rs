//! The "Deep Field" theme (GUI-1606): a graphite-navy ground, hairline borders
//! and one ice accent. Status colours carry meaning; nothing else glows.
//!
//! Every colour used anywhere in the UI is named here; views never invent one.

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Vec2, Visuals,
};
use std::sync::atomic::{AtomicBool, Ordering};

static MOTION_ENABLED: AtomicBool = AtomicBool::new(true);

/// Page background.
pub const BG_DEEP: Color32 = Color32::from_rgb(0x0a, 0x0d, 0x12);
/// The sidebar and the activity pane, one step above the page.
pub const BG_PANEL: Color32 = Color32::from_rgb(0x0d, 0x11, 0x17);
/// Dialogs and the edit drawer.
pub const MODAL: Color32 = Color32::from_rgb(0x0f, 0x14, 0x1b);
/// Card surface and its hovered variant (also the secondary button body).
pub const CARD: Color32 = Color32::from_rgb(0x11, 0x16, 0x1e);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x15, 0x1b, 0x24);
/// The selected row in the sidebar.
pub const SELECTED: Color32 = Color32::from_rgb(0x16, 0x1d, 0x27);
/// Inset surfaces: the console, text fields, the screen well.
pub const INSET: Color32 = Color32::from_rgb(0x07, 0x0a, 0x0e);

pub const STROKE: Color32 = Color32::from_rgb(0x1f, 0x27, 0x33);
pub const STROKE_STRONG: Color32 = Color32::from_rgb(0x2c, 0x36, 0x45);
/// Rules between panels and between rows.
pub const DIVIDER: Color32 = Color32::from_rgb(0x1a, 0x21, 0x2b);

pub const TEXT: Color32 = Color32::from_rgb(0xe4, 0xe8, 0xee);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x98, 0xa2, 0xb3);
/// Metadata. 4.7:1 on [`CARD_HOVER`] — the faintest text that still reads.
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x7d, 0x86, 0x95);

/// The one accent: selection, focus, meters, links.
pub const ACCENT: Color32 = Color32::from_rgb(0x8c, 0xc8, 0xe0);
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x5a, 0xa9, 0xcc);
pub const RESUMING: Color32 = Color32::from_rgb(0xb8, 0xdc, 0xec);
/// The primary button's fill; its label is [`BG_DEEP`].
pub const PRIMARY: Color32 = Color32::from_rgb(0xdc, 0xe6, 0xef);

pub const OK: Color32 = Color32::from_rgb(0x4f, 0xc3, 0x8d);
pub const WARN: Color32 = Color32::from_rgb(0xe2, 0xa7, 0x4f);
pub const ERR: Color32 = Color32::from_rgb(0xe7, 0x70, 0x7a);
/// A machine at rest: the Stopped dot.
pub const IDLE: Color32 = Color32::from_rgb(0x6b, 0x75, 0x86);

pub const CARD_RADIUS: u8 = 10;
pub const CONTROL_RADIUS: u8 = 6;

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

/// Where the few stars of the backdrop sit, as fractions of the page.
/// Deliberately sparse and static: the page is a working surface, and anything
/// that moves behind the machines competes with them.
const STARS: [(f32, f32, f32); 9] = [
    (0.08, 0.10, 1.0),
    (0.21, 0.27, 0.7),
    (0.87, 0.15, 1.0),
    (0.79, 0.06, 0.6),
    (0.94, 0.47, 0.7),
    (0.06, 0.62, 0.8),
    (0.84, 0.36, 0.5),
    (0.29, 0.09, 0.6),
    (0.68, 0.13, 0.7),
];

/// A handful of static one-pixel stars. Painter-only, no state, no motion.
pub fn paint_backdrop(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    for (x, y, r) in STARS {
        let at = egui::pos2(
            rect.left() + rect.width() * x,
            rect.top() + rect.height() * y,
        );
        painter.circle_filled(at, r, STAR);
    }
}

/// The star colour: a cool white, well below the text.
const STAR: Color32 = Color32::from_rgba_premultiplied(0x4f, 0x56, 0x5c, 0x5c);

/// Installs the theme on a fresh egui context.
pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(13.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
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
    visuals.window_fill = MODAL;
    visuals.faint_bg_color = CARD;
    visuals.extreme_bg_color = INSET;
    visuals.code_bg_color = INSET;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = ERR;
    visuals.hyperlink_color = ACCENT;
    visuals.window_stroke = Stroke::new(1.0_f32, STROKE_STRONG);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.window_shadow = Shadow {
        offset: [0, 24],
        blur: 60,
        spread: 0,
        color: Color32::from_black_alpha(150),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.selection.bg_fill = ACCENT_DEEP.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.slider_trailing_fill = true;

    // Widgets: quiet bodies, a brighter hairline on interaction. No glow, no
    // size change on hover — controls that grow under the pointer jitter.
    let radius = CornerRadius::same(CONTROL_RADIUS);
    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, STROKE);
    // Body text colour, and the base egui greys out for hints and disabled bits.
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = CARD_HOVER;
    w.inactive.weak_bg_fill = CARD_HOVER;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, STROKE_STRONG);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = radius;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = SELECTED;
    w.hovered.weak_bg_fill = SELECTED;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x44, 0x52, 0x6a));
    w.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = Color32::from_rgb(0x1c, 0x24, 0x30);
    w.active.weak_bg_fill = Color32::from_rgb(0x1c, 0x24, 0x30);
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;

    w.open.bg_fill = CARD_HOVER;
    w.open.weak_bg_fill = CARD_HOVER;
    w.open.bg_stroke = Stroke::new(1.0_f32, STROKE_STRONG);
    w.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.open.corner_radius = radius;

    style.visuals = visuals;

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 8.0);
    s.button_padding = Vec2::new(12.0, 6.0);
    s.window_margin = Margin::same(24);
    s.menu_margin = Margin::same(6);
    s.interact_size = Vec2::new(44.0, 28.0);
    s.slider_width = 220.0;
    s.combo_width = 168.0;
    s.text_edit_width = 260.0;
    s.scroll.bar_width = 8.0;
    s.scroll.floating = false;

    ctx.set_style(style);
}

/// Linear blend between two colours, gamma-space (good enough for hover
/// fades and what egui's own colour picker does).
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

/// A one-pixel horizontal rule in [`DIVIDER`].
pub fn rule(painter: &egui::Painter, x: egui::Rangef, y: f32) {
    painter.hline(x, y, Stroke::new(1.0_f32, DIVIDER));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_interpolates_and_clamps() {
        assert_eq!(mix(BG_DEEP, ACCENT, 0.0), BG_DEEP);
        assert_eq!(mix(BG_DEEP, ACCENT, 1.0), ACCENT);
        assert_eq!(mix(BG_DEEP, ACCENT, -3.0), BG_DEEP);
        assert_eq!(mix(BG_DEEP, ACCENT, 7.0), ACCENT);
        let middle = mix(Color32::BLACK, Color32::WHITE, 0.5);
        assert_eq!((middle.r(), middle.g(), middle.b()), (128, 128, 128));
    }

    #[test]
    fn the_palette_is_dark_cool_and_not_egui_grey() {
        // Guard against an accidental `Visuals::dark()` regression: the page
        // background must stay in the near-black blue range.
        assert!(BG_DEEP.r() < 0x20 && BG_DEEP.b() > BG_DEEP.r());
        assert!(CARD.b() > CARD.g() && CARD.g() >= CARD.r());
        // The accent is a cool, desaturated blue — no violet anywhere.
        assert!(ACCENT.b() > ACCENT.r() && ACCENT.g() > ACCENT.r());
    }

    /// WCAG relative luminance, for the contrast guard below.
    fn luminance(c: Color32) -> f32 {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (hi, lo) = {
            let (x, y) = (luminance(a), luminance(b));
            if x > y {
                (x, y)
            } else {
                (y, x)
            }
        };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn every_text_colour_reads_on_every_surface() {
        for surface in [BG_DEEP, BG_PANEL, MODAL, CARD, CARD_HOVER] {
            for text in [TEXT, TEXT_DIM, TEXT_FAINT] {
                assert!(
                    contrast(text, surface) >= 4.5,
                    "{text:?} on {surface:?} is {:.2}:1",
                    contrast(text, surface)
                );
            }
        }
        assert!(contrast(BG_DEEP, PRIMARY) >= 4.5, "primary button label");
    }
}
