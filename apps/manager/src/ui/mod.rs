//! Views. Every one of them is a pure function of application state plus a
//! sink for [`crate::app::Action`]s — no view mutates the world directly.

pub mod cards;
pub mod diagnostics;
pub mod dialogs;
pub mod disks;
pub mod header;
pub mod logpane;
pub mod snapshots;
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
    let t = theme::animate_bool(ui.ctx(), response.id.with("hot"), response.hovered(), 0.14);
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
    let t = theme::animate_bool(
        ui.ctx(),
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
    let t = theme::animate_bool(ui.ctx(), id.with("hover"), hovered, 0.18);

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

    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink(15.0))
            .layout(Layout::top_down(Align::Min)),
    );
    let inner = add(&mut child, t);
    (inner, response)
}

// ---------------------------------------------------------------------------
// Form rows
// ---------------------------------------------------------------------------
//
// Two conventions live in this section, and every form in the product follows
// them because every form goes through these three functions:
//
// 1. **The explanation is a tooltip, not body text.** A visible sentence under
//    each field turns a short form into a wall of prose that experienced users
//    read past and new users still do not read. The label stays short; the
//    sentence that teaches ("NVRAM — this machine's own UEFI settings…") hangs
//    off the label on hover. When a control is disabled, the same tooltip
//    carries the reason.
// 2. **One label column, one field width.** Stacked rows share
//    [`FORM_LABEL_W`] and reserve [`FORM_TRAIL_W`] for a trailing button
//    whether or not the row has one, so the text boxes form a straight edge and
//    the Browse buttons line up under each other. Alignment is a property of
//    the helper; a call site cannot drift out of it by adding a space.

/// Width of the label column shared by every stacked form row.
pub const FORM_LABEL_W: f32 = 112.0;
/// Width reserved at the end of every row for its trailing control. Reserved
/// even when a row has none — that is what keeps the field edges straight.
pub const FORM_TRAIL_W: f32 = 84.0;
/// Height of a row's label cell, so labels sit on the field's centre line.
const FORM_ROW_H: f32 = 22.0;
/// The difference between the width a `ComboBox` is *given* and the width it
/// then draws: it lays the arrow out inside that budget and comes back about
/// eleven pixels narrower. Measured, not guessed — a combo and a text field in
/// consecutive rows have to end on the same pixel.
const COMBO_SHRINK: f32 = 8.5;

/// The width to hand a [`egui::ComboBox`] so it fills a row's field column
/// exactly. Alignment lives in the helper, never in a call site's arithmetic.
pub fn combo_width(field_w: f32) -> f32 {
    (field_w + COMBO_SHRINK).max(80.0)
}

/// Pins the row width for every form row in this container, for this frame.
///
/// Without it each row measures `available_width()` for itself — and a
/// container grows as wrapped notes are added to it, so row three ends up a few
/// pixels wider than row one and the column of text boxes visibly staircases.
/// Measuring once, before any row, is what makes the right-hand edges straight
/// no matter what is rendered between them.
///
/// Call it at the top of every form section. It is idempotent and costs one
/// memory write.
pub fn form_scope(ui: &mut Ui) {
    let width = ui.available_width();
    let id = form_width_id(ui);
    ui.data_mut(|data| data.insert_temp(id, width));
}

fn form_width_id(ui: &Ui) -> egui::Id {
    ui.id().with("entangled-form-width")
}

fn row_width(ui: &Ui) -> f32 {
    let id = form_width_id(ui);
    ui.data(|data| data.get_temp::<f32>(id))
        .unwrap_or_else(|| ui.available_width())
}

/// One labelled row: the label column, then the caller's widget, sized to the
/// shared field width handed to the closure.
///
/// `tooltip` is the user-facing explanation. Pass `""` only when the label is
/// genuinely self-explanatory; anything with jargon in it gets a sentence.
pub fn form_row<R>(
    ui: &mut Ui,
    label: &str,
    tooltip: &str,
    add: impl FnOnce(&mut Ui, f32) -> R,
) -> R {
    let gap = ui.spacing().item_spacing.x;
    let field_w = (row_width(ui) - FORM_LABEL_W - FORM_TRAIL_W - 2.0 * gap).max(140.0);
    ui.horizontal(|ui| {
        form_label(ui, label, tooltip);
        add(ui, field_w)
    })
    .inner
}

/// The label cell: **exactly** [`FORM_LABEL_W`] wide whatever the label says,
/// vertically centred, and the hover target for the explanation.
///
/// `allocate_exact_size` rather than a laid-out child, because a long label in
/// a stretchy cell is precisely how a column of fields drifts out of
/// alignment — "MACHINES IN" is wider than "RUNS ON", and one row would start
/// further right than the next. The label truncates instead.
///
/// A tooltip nobody knows about is not documentation, so a row that has one
/// shows a quiet `?` and the whole cell is the hover target.
fn form_label(ui: &mut Ui, label: &str, tooltip: &str) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(FORM_LABEL_W, FORM_ROW_H), Sense::hover());
    let mut cell = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    cell.spacing_mut().item_spacing.x = 4.0;
    cell.add(egui::Label::new(faint(label)).selectable(false).truncate());
    if !tooltip.is_empty() {
        cell.add(
            egui::Label::new(
                RichText::new("?")
                    .size(10.0)
                    .color(theme::CYAN_DEEP)
                    .monospace(),
            )
            .selectable(false),
        );
        response.on_hover_text(tooltip);
    }
}

/// A row holding one path: text field, Browse button, and (optionally) a status
/// note underneath, indented to the field column.
///
/// Returns `true` on the frame the Browse button was clicked; the caller turns
/// that into a picker [`crate::app::Action`], because views never act.
pub fn path_row(
    ui: &mut Ui,
    label: &str,
    tooltip: &str,
    value: &mut String,
    hint: &str,
    enabled: bool,
) -> bool {
    form_row(ui, label, tooltip, |ui, field_w| {
        ui.add_enabled(
            enabled,
            egui::TextEdit::singleline(value)
                .desired_width(field_w)
                .hint_text(RichText::new(hint).color(theme::TEXT_FAINT).italics()),
        );
        let (rect, _) = ui.allocate_exact_size(Vec2::new(FORM_TRAIL_W, FORM_ROW_H), Sense::hover());
        let mut browse = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        ghost_button(&mut browse, "Browse…", enabled, theme::CYAN)
            .on_hover_text("Pick the file from your computer instead of typing its path")
            .clicked()
    })
}

/// A note under a form row, indented to the field column so it reads as
/// belonging to the field above it rather than to the form, and bounded by the
/// same field width so a long path cannot widen the form.
pub fn form_note(ui: &mut Ui, text: RichText) {
    let gap = ui.spacing().item_spacing.x;
    let width = (row_width(ui) - FORM_LABEL_W - gap).max(140.0);
    ui.horizontal(|ui| {
        ui.add_space(FORM_LABEL_W);
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_max_width(width);
            ui.add(egui::Label::new(text).selectable(false).wrap());
        });
    });
}

/// "✓ found (4.0 MiB)" / "not there yet" for a path field, as a [`form_note`].
///
/// `resolved` is the path actually checked — a profile may hold a relative path
/// that resolves against the working directory, and a badge that did not say
/// *which* file it looked at would be worse than no badge.
pub fn path_status_note(ui: &mut Ui, resolved: &std::path::Path, missing_hint: &str) {
    if let Ok(meta) = std::fs::metadata(resolved) {
        form_note(
            ui,
            RichText::new(format!(
                "found: {}  ({})",
                resolved.display(),
                crate::discovery::format_bytes(meta.len())
            ))
            .color(theme::OK)
            .size(11.0),
        );
    } else {
        form_note(
            ui,
            RichText::new(format!("not found: {}", resolved.display()))
                .color(theme::WARN)
                .size(11.0),
        );
        if !missing_hint.is_empty() {
            form_note(
                ui,
                RichText::new(missing_hint)
                    .color(theme::TEXT_FAINT)
                    .size(11.0),
            );
        }
    }
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

/// A tiny CPU sparkline: the last ~90 one-second samples as a polyline on an
/// inset chip. Scale is anchored at 100% and grows with the peak, so a
/// multi-vCPU guest (top-style, >100%) never clips.
pub fn sparkline(ui: &mut Ui, values: &[f32], size: Vec2, tint: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(4),
        theme::INSET.gamma_multiply(0.85),
    );
    if values.len() < 2 {
        return;
    }
    let max = values.iter().copied().fold(100.0_f32, f32::max);
    let n = values.len();
    let points: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + 1.0 + (rect.width() - 2.0) * i as f32 / (n - 1) as f32;
            let y = rect.bottom() - 1.5 - (v / max).clamp(0.0, 1.0) * (rect.height() - 3.0);
            egui::pos2(x, y)
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.3_f32, tint)));
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
/// VM, the entangled-particle spinner while installing or suspending, a quiet
/// filled ring for a machine parked in a file.
///
/// The indicator carries the *kind* of state and the colour carries which one:
/// anything that spins is work in progress, anything static is at rest. That is
/// what keeps Suspending and Suspended — one word apart — from being read as
/// each other at a glance.
pub fn status_badge(ui: &mut Ui, status: crate::app::Status, time: f64) {
    let text = status.label();
    let color = status.color();
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let indicator = 22.0;
    let size = Vec2::new(galley.size().x + indicator + 20.0, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());

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
        crate::app::Status::Installing | crate::app::Status::Suspending => {
            crate::logo::paint_particle_spinner(painter, indicator_rect, time)
        }
        crate::app::Status::Suspended => {
            // At rest, but holding something: a filled core inside a complete
            // ring, and nothing moving.
            painter.circle_stroke(
                indicator_rect.center(),
                5.5,
                Stroke::new(1.2_f32, color.gamma_multiply(0.55)),
            );
            painter.circle_filled(indicator_rect.center(), 2.8, color);
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
    response.on_hover_text(status.tooltip());
}
