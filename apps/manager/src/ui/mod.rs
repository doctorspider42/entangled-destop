//! Views. Every one of them is a pure function of application state plus a
//! sink for [`crate::app::Action`]s — no view mutates the world directly.

pub mod cards;
pub mod diagnostics;
pub mod dialogs;
pub mod disks;
pub mod logpane;
pub mod machine;
pub mod sidebar;
pub mod snapshots;
pub mod toasts;

use egui::{
    Align, Color32, CornerRadius, Layout, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, Vec2,
};

use crate::theme;

/// Height of every button in the product, so a row of them lines up whatever
/// their kind.
pub const BUTTON_H: f32 = 34.0;
/// Horizontal padding inside a button, each side.
const BUTTON_PAD: f32 = 14.0;

/// The one primary action of a surface: a pale fill with a dark label. There
/// is at most one per view — it is the brightest thing on the page.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, theme::BG_DEEP);
    let size = Vec2::new(galley.size().x + 2.0 * BUTTON_PAD, BUTTON_H);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let t = theme::animate_bool(ui.ctx(), response.id.with("hot"), response.hovered(), 0.12);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(theme::CONTROL_RADIUS),
        theme::mix(theme::PRIMARY, Color32::WHITE, t),
    );
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        Color32::TRANSPARENT,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The same button, in the state a pre-flight can put it in: present, greyed,
/// and still a hover target so the reason can hang off it.
///
/// Never `clicked()` when disabled — a caller can wire it up exactly as it
/// wires up the enabled one, and the refusal is a tooltip rather than an error
/// the user has to provoke.
pub fn primary_button_enabled(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    if enabled {
        return primary_button(ui, text);
    }
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, theme::TEXT_FAINT);
    let size = Vec2::new(galley.size().x + 2.0 * BUTTON_PAD, BUTTON_H);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let radius = CornerRadius::same(theme::CONTROL_RADIUS);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, theme::CARD_HOVER);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, theme::STROKE),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        Color32::TRANSPARENT,
    );
    response
}

/// The secondary button: a quiet body, a hairline that brightens on hover and
/// a label in `tint` (body text for ordinary actions, a status colour for the
/// rare one that needs it — Stop, Delete).
pub fn ghost_button(ui: &mut Ui, text: &str, enabled: bool, tint: Color32) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let fg = if enabled { tint } else { theme::TEXT_FAINT };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let size = Vec2::new(galley.size().x + 2.0 * BUTTON_PAD, BUTTON_H);
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
        0.12,
    );
    let radius = CornerRadius::same(theme::CONTROL_RADIUS);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        radius,
        theme::mix(theme::CARD_HOVER, theme::SELECTED, t),
    );
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(
            1.0_f32,
            theme::mix(theme::STROKE_STRONG, Color32::from_rgb(0x44, 0x52, 0x6a), t),
        ),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        Color32::TRANSPARENT,
    );
    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A secondary button in body text — the ordinary case of [`ghost_button`].
pub fn secondary_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    ghost_button(ui, text, enabled, theme::TEXT)
}

/// A borderless text button for the least important action in a row
/// (Cancel, "Open full log").
pub fn text_button(ui: &mut Ui, text: &str, tint: Color32) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, tint);
    let size = Vec2::new(galley.size().x + 12.0, BUTTON_H);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let color = if response.hovered() {
        theme::TEXT
    } else {
        tint
    };
    ui.painter().galley_with_override_text_color(
        rect.center() - galley.size() * 0.5,
        galley,
        color,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The small upper-case caption that names a section ("RUNNING NOW").
pub fn caption(text: &str) -> RichText {
    RichText::new(text.to_uppercase())
        .monospace()
        .size(10.5)
        .color(theme::TEXT_FAINT)
}

/// A thin meter: `fraction` of `width` filled in `tint`.
pub fn meter(ui: &mut Ui, fraction: f32, width: f32, tint: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), theme::STROKE);
    let filled = Rect::from_min_size(
        rect.left_top(),
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    painter.rect_filled(filled, CornerRadius::same(2), tint);
}

/// A status dot: filled for a state with something in it, a ring for Stopped.
pub fn status_dot(painter: &egui::Painter, center: egui::Pos2, status: crate::app::Status) {
    let color = status.color();
    if status == crate::app::Status::Stopped {
        painter.circle_stroke(center, 3.4, Stroke::new(1.5_f32, color));
    } else {
        painter.circle_filled(center, 4.0, color);
    }
}

/// Inset between a card's border and its contents, on every edge. Named
/// because a caller that sizes a card from its own measured content — the
/// Snapshots rows, whose refusals wrap — has to subtract exactly this.
pub const CARD_PAD: f32 = 16.0;

/// A fixed-size surface with the card look: rounded, a hairline border that
/// brightens on hover, no shadow — depth comes from the surface step.
pub fn card<R>(
    ui: &mut Ui,
    id: egui::Id,
    size: Vec2,
    add: impl FnOnce(&mut Ui, f32) -> R,
) -> (R, Response) {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let hovered = ui.rect_contains_pointer(rect);
    let t = theme::animate_bool(ui.ctx(), id.with("hover"), hovered, 0.12);

    let radius = CornerRadius::same(theme::CARD_RADIUS);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, theme::CARD);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, theme::mix(theme::STROKE, theme::STROKE_STRONG, t)),
        StrokeKind::Inside,
    );

    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink(CARD_PAD))
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
const FORM_ROW_H: f32 = BUTTON_H;
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
/// shows a quiet `?`. Hover explains it; clicking opens the same help at once.
fn form_label(ui: &mut Ui, label: &str, tooltip: &str) {
    // Painted, not laid out as widgets: egui gives the hover to the top-most
    // widget under the pointer, so a `Label` inside the cell swallowed it and
    // the tooltip on the cell never appeared. With nothing interactive inside,
    // the whole cell — text and `?` alike — is the hover target.
    let sense = if tooltip.is_empty() {
        Sense::hover()
    } else {
        Sense::click()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(FORM_LABEL_W, FORM_ROW_H), sense);
    let painter = ui.painter_at(rect);
    let mark_w = if tooltip.is_empty() { 0.0 } else { 18.0 };
    let galley = painter.layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(11.5),
        theme::TEXT_FAINT,
    );
    let text_w = galley.size().x.min(rect.width() - mark_w);
    painter
        .with_clip_rect(Rect::from_min_size(
            rect.min,
            Vec2::new(text_w, rect.height()),
        ))
        .galley(
            egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5),
            galley,
            theme::TEXT_FAINT,
        );
    if !tooltip.is_empty() {
        let hovered = response.hovered();
        let center = egui::pos2(rect.left() + text_w + 11.0, rect.center().y);
        painter.circle_stroke(
            center,
            6.5,
            Stroke::new(
                1.0_f32,
                if hovered {
                    theme::TEXT_DIM
                } else {
                    theme::STROKE_STRONG
                },
            ),
        );
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            "?",
            egui::FontId::proportional(9.5),
            if hovered {
                theme::TEXT
            } else {
                theme::TEXT_FAINT
            },
        );
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Help);
        }
        let popup = egui::Popup::from_toggle_button_response(&response);
        let open = popup.is_open();
        popup.show(|ui| {
            ui.set_max_width(320.0);
            ui.label(RichText::new(tooltip).color(theme::TEXT).size(12.5));
        });
        if !open {
            response.on_hover_text(tooltip);
        }
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
                .min_size(Vec2::new(0.0, FORM_ROW_H))
                .vertical_align(Align::Center)
                .hint_text(RichText::new(hint).color(theme::TEXT_FAINT).italics()),
        );
        ui.allocate_ui_with_layout(
            Vec2::new(FORM_TRAIL_W, FORM_ROW_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ghost_button(ui, "Browse…", enabled, theme::ACCENT)
                    .on_hover_text("Pick the file from your computer instead of typing its path")
                    .clicked()
            },
        )
        .inner
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
    // A hint of the tint over the surface, with the label carrying the colour
    // — filled chips would fight the cards for attention.
    painter.rect_filled(
        rect,
        CornerRadius::same(255),
        theme::mix(theme::CARD, tint, 0.12),
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
        .fill(theme::mix(theme::BG_DEEP, tint, 0.07))
        .stroke(Stroke::new(1.0_f32, theme::mix(theme::STROKE, tint, 0.35)))
        .corner_radius(radius)
        .inner_margin(egui::Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (dot, _) = ui.allocate_exact_size(Vec2::new(8.0, 18.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.5, tint);
                ui.label(RichText::new(text).color(theme::TEXT).size(13.5));
                if let Some(action) = action {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        clicked = secondary_button(ui, action, true).clicked();
                    });
                }
            });
        });
    clicked
}

/// The status pill (GUI-1601 + GUI-1606): a tinted pill with a dot and the
/// state in words ("Running · 2 h 14 min"). Running breathes slowly;
/// Installing and Suspending show a turning arc; everything at rest is still.
///
/// The indicator carries the *kind* of state and the colour carries which one:
/// anything that turns is work in progress, anything static is at rest. That is
/// what keeps Suspending and Suspended — one word apart — from being read as
/// each other at a glance.
pub fn status_pill(ui: &mut Ui, status: crate::app::Status, text: &str, time: f64) {
    let color = status.color();
    let label = theme::mix(color, theme::TEXT, 0.35);
    let font = egui::FontId::proportional(12.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, label);
    let indicator = 14.0;
    let size = Vec2::new(galley.size().x + indicator + 22.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(255),
        theme::mix(theme::BG_DEEP, color, 0.14),
    );
    let indicator_rect = Rect::from_center_size(
        rect.left_center() + Vec2::new(10.0 + indicator * 0.5 - 3.0, 0.0),
        Vec2::splat(indicator),
    );
    let center = indicator_rect.center();
    match status {
        crate::app::Status::Installing | crate::app::Status::Suspending => {
            crate::logo::paint_spinner(painter, indicator_rect, time, color)
        }
        crate::app::Status::Running => {
            // A slow breath: alive without being loud.
            let pulse = 0.5 + 0.5 * (time * std::f64::consts::PI).sin() as f32;
            painter.circle_filled(
                center,
                3.0 + 2.5 * pulse,
                color.gamma_multiply(0.25 * (1.0 - pulse)),
            );
            painter.circle_filled(center, 3.0, color);
        }
        crate::app::Status::Stopping => {
            let pulse = 0.5 + 0.5 * (time * 5.0).sin() as f32;
            painter.circle_filled(center, 3.0, color.gamma_multiply(0.35 + 0.65 * pulse));
        }
        crate::app::Status::Suspended => {
            painter.circle_filled(center, 3.0, color);
        }
        crate::app::Status::Stopped => {
            painter.circle_stroke(center, 3.0, Stroke::new(1.4_f32, color));
        }
    }
    painter.galley(
        egui::pos2(
            indicator_rect.right() + 4.0,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        Color32::TRANSPARENT,
    );
    response.on_hover_text(status.tooltip());
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// The product's line icons, drawn with the painter on a 16-unit grid — no
/// image assets, crisp at any scale, coloured like text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Grid,
    Disk,
    Pulse,
    List,
    Gear,
    Plus,
    Close,
    Chevron,
    More,
    Screen,
    Chip,
    Globe,
    Folder,
    Power,
}

/// Paints `icon` filling `rect` (square is best) in `color`.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let unit = rect.width().min(rect.height()) / 16.0;
    let origin = rect.center() - Vec2::splat(8.0 * unit);
    let p = |x: f32, y: f32| origin + Vec2::new(x, y) * unit;
    let stroke = Stroke::new((1.3 * unit).max(1.0), color);
    let line = |points: &[(f32, f32)]| {
        painter.add(egui::Shape::line(
            points.iter().map(|&(x, y)| p(x, y)).collect(),
            stroke,
        ));
    };
    let frame = |x: f32, y: f32, w: f32, h: f32, r: f32| {
        painter.rect_stroke(
            Rect::from_min_size(p(x, y), Vec2::new(w, h) * unit),
            CornerRadius::same((r * unit).round() as u8),
            stroke,
            StrokeKind::Middle,
        );
    };
    match icon {
        Icon::Grid => {
            for (x, y) in [(2.0, 2.0), (9.0, 2.0), (2.0, 9.0), (9.0, 9.0)] {
                frame(x, y, 5.0, 5.0, 1.0);
            }
        }
        Icon::Disk => {
            painter.add(egui::Shape::ellipse_stroke(
                p(8.0, 4.0),
                Vec2::new(5.5, 2.0) * unit,
                stroke,
            ));
            line(&[(2.5, 4.0), (2.5, 12.0)]);
            line(&[(13.5, 4.0), (13.5, 12.0)]);
            let bottom: Vec<egui::Pos2> = (0..=12)
                .map(|i| {
                    let t = std::f32::consts::PI * i as f32 / 12.0;
                    p(8.0 + 5.5 * t.cos(), 12.0 + 2.0 * t.sin())
                })
                .collect();
            painter.add(egui::Shape::line(bottom, stroke));
        }
        Icon::Pulse => line(&[
            (1.5, 8.0),
            (4.5, 8.0),
            (6.5, 3.5),
            (9.5, 12.5),
            (11.5, 8.0),
            (14.5, 8.0),
        ]),
        Icon::List => {
            line(&[(3.0, 4.0), (13.0, 4.0)]);
            line(&[(3.0, 8.0), (13.0, 8.0)]);
            line(&[(3.0, 12.0), (9.0, 12.0)]);
        }
        Icon::Gear => {
            painter.circle_stroke(p(8.0, 8.0), 2.2 * unit, stroke);
            for i in 0..8 {
                let a = std::f32::consts::TAU * i as f32 / 8.0;
                let (s, c) = a.sin_cos();
                line(&[
                    (8.0 + 4.6 * c, 8.0 + 4.6 * s),
                    (8.0 + 6.5 * c, 8.0 + 6.5 * s),
                ]);
            }
            painter.circle_stroke(p(8.0, 8.0), 4.6 * unit, stroke);
        }
        Icon::Plus => {
            line(&[(8.0, 3.0), (8.0, 13.0)]);
            line(&[(3.0, 8.0), (13.0, 8.0)]);
        }
        Icon::Close => {
            line(&[(4.0, 4.0), (12.0, 12.0)]);
            line(&[(12.0, 4.0), (4.0, 12.0)]);
        }
        Icon::Chevron => line(&[(6.0, 3.5), (10.5, 8.0), (6.0, 12.5)]),
        Icon::More => {
            for x in [3.5, 8.0, 12.5] {
                painter.circle_filled(p(x, 8.0), 1.2 * unit, color);
            }
        }
        Icon::Screen => {
            frame(2.0, 3.0, 12.0, 8.5, 1.5);
            line(&[(5.5, 14.0), (10.5, 14.0)]);
        }
        Icon::Chip => {
            frame(4.5, 4.5, 7.0, 7.0, 1.0);
            for k in [6.5, 9.5] {
                line(&[(k, 1.5), (k, 4.5)]);
                line(&[(k, 11.5), (k, 14.5)]);
                line(&[(1.5, k), (4.5, k)]);
                line(&[(11.5, k), (14.5, k)]);
            }
        }
        Icon::Globe => {
            painter.circle_stroke(p(8.0, 8.0), 6.5 * unit, stroke);
            line(&[(1.5, 8.0), (14.5, 8.0)]);
            painter.add(egui::Shape::ellipse_stroke(
                p(8.0, 8.0),
                Vec2::new(2.8, 6.5) * unit,
                stroke,
            ));
        }
        Icon::Folder => line(&[
            (1.5, 4.5),
            (6.5, 4.5),
            (8.0, 6.0),
            (14.5, 6.0),
            (14.5, 13.0),
            (1.5, 13.0),
            (1.5, 4.5),
        ]),
        Icon::Power => {
            line(&[(8.0, 1.5), (8.0, 7.5)]);
            let arc: Vec<egui::Pos2> = (0..=20)
                .map(|i| {
                    let a = -std::f32::consts::FRAC_PI_2
                        + 0.75
                        + (std::f32::consts::TAU - 1.5) * i as f32 / 20.0;
                    p(8.0 + 5.5 * a.cos(), 8.5 + 5.5 * a.sin())
                })
                .collect();
            painter.add(egui::Shape::line(arc, stroke));
        }
    }
}

/// An icon-only button: a square hit target with a hairline, the icon in the
/// middle and its meaning on hover (it has no label, so the hover is required).
pub fn icon_button(ui: &mut Ui, icon: Icon, hover: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(BUTTON_H), Sense::click());
    let t = theme::animate_bool(ui.ctx(), response.id.with("hot"), response.hovered(), 0.12);
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        CornerRadius::same(theme::CONTROL_RADIUS),
        Stroke::new(1.0_f32, theme::mix(theme::STROKE, theme::STROKE_STRONG, t)),
        StrokeKind::Inside,
    );
    paint_icon(
        painter,
        Rect::from_center_size(rect.center(), Vec2::splat(16.0)),
        icon,
        theme::mix(theme::TEXT_DIM, theme::TEXT, t),
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.on_hover_text(hover)
}

/// One-line human description of a machine's state for lists ("Running · 2 h",
/// "Saved 3 h ago", "Off").
pub fn status_line(app: &crate::app::ManagerApp, name: &str) -> String {
    use crate::app::Status;
    let status = app.status_of(name);
    match status {
        Status::Running => match app.supervisor.active_task(name) {
            Some(task) => format!(
                "Running · {}",
                crate::metrics::format_uptime(task.started_at.elapsed())
            ),
            None => "Running".to_string(),
        },
        Status::Suspended => match app.snapshot_for(name).map(|row| &row.facts) {
            Some(Ok(facts)) => format!(
                "Saved {}",
                vm_snapshot::meta::describe_age(facts.created_unix, vm_snapshot::meta::now_unix())
            ),
            _ => "Saved session".to_string(),
        },
        Status::Stopped => "Off".to_string(),
        other => other.label().to_string(),
    }
}

/// The Storage page's title and its two tabs — disk images and saved
/// sessions are both "large files in the machines folder", so they share one
/// place in the sidebar and switch here.
pub fn storage_header(
    ui: &mut Ui,
    app: &crate::app::ManagerApp,
    actions: &mut Vec<crate::app::Action>,
) {
    use crate::app::{Action, View};
    ui.label(
        RichText::new("Storage")
            .size(24.0)
            .color(theme::TEXT)
            .strong(),
    );
    ui.add_space(2.0);
    let free = app
        .stats
        .host
        .vm_dir_space
        .map(|(free, _)| format!(" · {} free there", crate::discovery::format_bytes(free)))
        .unwrap_or_default();
    ui.label(
        RichText::new(format!(
            "Everything in {}{free}",
            app.settings.vm_dir.display()
        ))
        .size(13.5)
        .color(theme::TEXT_DIM),
    );
    ui.add_space(14.0);
    let top = ui.cursor().top();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 24.0;
        for (view, label, count) in [
            (View::Disks, "Disks", app.scan.disks.len()),
            (View::Snapshots, "Saved sessions", app.scan.snapshots.len()),
        ] {
            let selected = app.view == view;
            if page_tab(ui, &format!("{label}  {count}"), selected).clicked() && !selected {
                actions.push(Action::SwitchView(view));
            }
        }
    });
    let bottom = ui.min_rect().bottom().max(top + 40.0);
    theme::rule(ui.painter(), ui.max_rect().x_range(), bottom);
    ui.add_space(20.0);
}

/// A page-level tab: a label with an accent underline when it is the one shown.
pub fn page_tab(ui: &mut Ui, label: &str, selected: bool) -> Response {
    let font = egui::FontId::proportional(13.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, theme::TEXT_DIM);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(galley.size().x, 40.0), Sense::click());
    let color = if selected || response.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    ui.painter().galley_with_override_text_color(
        egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5 - 2.0),
        galley,
        color,
    );
    if selected {
        ui.painter().rect_filled(
            Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - 2.0),
                rect.right_bottom(),
            ),
            CornerRadius::same(1),
            theme::ACCENT,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_opens_on_click_without_waiting_for_hover_and_closes_on_escape() {
        let ctx = egui::Context::default();
        let mut point = egui::Pos2::ZERO;
        let mut frame = |time, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(500.0, 300.0),
                    )),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        point = ui.cursor().min + Vec2::new(100.0, FORM_ROW_H * 0.5);
                        form_label(ui, "CUSTOM BUILD", "Use your own build of Entangled.");
                    });
                },
            );
            point
        };
        let point = frame(0.0, vec![]);
        for (time, pressed) in [(0.01, true), (0.02, false)] {
            frame(
                time,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(
            egui::Popup::is_any_open(&ctx),
            "click must show help immediately"
        );
        frame(
            0.03,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!egui::Popup::is_any_open(&ctx));
    }
}
