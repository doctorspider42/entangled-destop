//! Views. Every one of them is a pure function of application state plus a
//! sink for [`crate::app::Action`]s — no view mutates the world directly.

pub mod cards;
pub mod dialogs;
pub mod disks;
pub mod header;
pub mod logpane;
pub mod toasts;

use egui::{
    Align, Color32, CornerRadius, Layout, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, Vec2,
};

use crate::theme;

/// Filled accent button for the one primary action of a surface.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    accent_button(ui, text, 0.35, theme::BG_DEEP)
}

fn accent_button(ui: &mut Ui, text: &str, gradient_at: f32, fg: Color32) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let size = galley.size() + Vec2::new(30.0, 15.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id.with("hot"), response.hovered(), 0.14);
    let radius = CornerRadius::same(theme::CONTROL_RADIUS);
    let painter = ui.painter();
    // Glow first, then the body: the accent slides along the cyan→violet ramp
    // as the pointer arrives.
    if t > 0.0 {
        painter.rect_filled(
            rect.expand(3.0 * t),
            CornerRadius::same(theme::CONTROL_RADIUS + 3),
            theme::accent(gradient_at + 0.2).gamma_multiply(0.18 * t),
        );
    }
    painter.rect_filled(rect, radius, theme::accent(gradient_at + 0.30 * t));
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        Color32::TRANSPARENT,
    );
    response
}

/// Quiet outlined button used for secondary actions on the cards.
pub fn ghost_button(ui: &mut Ui, text: &str, enabled: bool, tint: Color32) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let fg = if enabled { tint } else { theme::TEXT_FAINT };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let size = galley.size() + Vec2::new(22.0, 13.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let t = ui.ctx().animate_bool_with_time(
        response.id.with("hot"),
        enabled && response.hovered(),
        0.14,
    );
    let radius = CornerRadius::same(theme::CONTROL_RADIUS);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        radius,
        theme::mix(theme::CARD, tint.gamma_multiply(0.30), 0.18 * t + 0.06),
    );
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, theme::mix(theme::STROKE, tint, 0.25 + 0.55 * t)),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        Color32::TRANSPARENT,
    );
    response
}

/// A fixed-size interactive surface with the card look: rounded, faintly lit
/// border, an accent bar on the left and a hover animation.
pub fn card<R>(
    ui: &mut Ui,
    id: egui::Id,
    size: Vec2,
    accent_at: f32,
    add: impl FnOnce(&mut Ui, f32) -> R,
) -> (R, Response) {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let hovered = ui.rect_contains_pointer(rect);
    let t = ui
        .ctx()
        .animate_bool_with_time(id.with("hover"), hovered, 0.18);

    let radius = CornerRadius::same(theme::CARD_RADIUS);
    let painter = ui.painter();
    painter.rect_filled(
        rect.translate(Vec2::new(0.0, 2.0)),
        radius,
        Color32::from_black_alpha(70),
    );
    painter.rect_filled(rect, radius, theme::mix(theme::CARD, theme::CARD_HOVER, t));
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(
            1.0_f32,
            theme::mix(theme::STROKE, theme::accent(accent_at), 0.10 + 0.55 * t),
        ),
        StrokeKind::Inside,
    );
    // The entanglement bar: a cyan→violet gradient hairline along the top edge,
    // brightening on hover.
    let bar = Rect::from_min_size(
        rect.left_top() + Vec2::new(theme::CARD_RADIUS as f32, 0.0),
        Vec2::new(rect.width() - 2.0 * theme::CARD_RADIUS as f32, 2.0),
    );
    theme::gradient_rect(
        painter,
        bar,
        theme::CYAN.gamma_multiply(0.35 + 0.65 * t),
        theme::VIOLET.gamma_multiply(0.35 + 0.65 * t),
    );

    let inner = ui
        .scope_builder(
            UiBuilder::new()
                .max_rect(rect.shrink(15.0))
                .layout(Layout::top_down(Align::Min)),
            |ui| add(ui, t),
        )
        .inner;
    (inner, response)
}

/// Dim caption text.
pub fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text).color(theme::TEXT_DIM).size(12.5)
}

pub fn faint(text: impl Into<String>) -> RichText {
    RichText::new(text).color(theme::TEXT_FAINT).size(11.5)
}

/// A rounded chip holding one metric ("2048 MiB", "2 vCPU", …).
pub fn chip(ui: &mut Ui, text: &str, tint: Color32) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, tint);
    let size = galley.size() + Vec2::new(16.0, 8.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    // A hint of the tint over the card surface, with the label carrying the
    // colour — filled chips would fight the cards for attention.
    painter.rect_filled(
        rect,
        CornerRadius::same(9),
        theme::mix(theme::CARD, tint, 0.16),
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0_f32, tint.gamma_multiply(0.30)),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        Color32::TRANSPARENT,
    );
}

/// Full-width notice with an optional action button; returns true when clicked.
pub fn banner(ui: &mut Ui, tint: Color32, text: &str, action: Option<&str>) -> bool {
    let mut clicked = false;
    let radius = CornerRadius::same(theme::CONTROL_RADIUS);
    egui::Frame::new()
        .fill(tint.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0_f32, tint.gamma_multiply(0.55)))
        .corner_radius(radius)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new("!").color(tint).strong());
                ui.label(RichText::new(text).color(theme::TEXT).size(13.0));
                if let Some(action) = action {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        clicked = ghost_button(ui, action, true, tint).clicked();
                    });
                }
            });
        });
    clicked
}

/// The animated status pill (GUI-1601 + GUI-1606): a pulsing dot for a running
/// VM, the entangled-particle spinner while installing.
pub fn status_badge(ui: &mut Ui, status: crate::app::Status, time: f64) {
    let text = status.label();
    let color = status.color();
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let indicator = 22.0;
    let size = Vec2::new(galley.size().x + indicator + 20.0, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());

    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(11), color.gamma_multiply(0.12));
    painter.rect_stroke(
        rect,
        CornerRadius::same(11),
        Stroke::new(1.0_f32, color.gamma_multiply(0.45)),
        StrokeKind::Inside,
    );

    let indicator_rect = Rect::from_min_size(
        rect.left_top() + Vec2::new(5.0, 0.0),
        Vec2::splat(rect.height()),
    )
    .with_max_x(rect.left() + 5.0 + indicator);
    match status {
        crate::app::Status::Installing => {
            crate::logo::paint_particle_spinner(painter, indicator_rect, time)
        }
        crate::app::Status::Running => {
            // Breathing halo: alive without being loud.
            let pulse = 0.5 + 0.5 * (time * 1.8).sin() as f32;
            painter.circle_filled(
                indicator_rect.center(),
                4.5 + 2.5 * pulse,
                color.gamma_multiply(0.18 + 0.18 * pulse),
            );
            painter.circle_filled(indicator_rect.center(), 3.6, color);
        }
        crate::app::Status::Stopping => {
            let pulse = 0.5 + 0.5 * (time * 5.0).sin() as f32;
            painter.circle_filled(
                indicator_rect.center(),
                3.6,
                color.gamma_multiply(0.35 + 0.65 * pulse),
            );
        }
        crate::app::Status::Stopped => {
            painter.circle_stroke(
                indicator_rect.center(),
                3.4,
                Stroke::new(1.4_f32, color.gamma_multiply(0.9)),
            );
        }
    }
    painter.galley(
        egui::pos2(
            indicator_rect.right() + 6.0,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        Color32::TRANSPARENT,
    );
}
