//! The settings-list look the machine editor is built from: titled sections of
//! grouped rows, each row a title, one short line of what it does, and its
//! control on the right.
//!
//! This is the counterpart of `form_row` for a surface where a person *chooses*
//! rather than *types*: the visible line says what the setting is for in plain
//! words, and anything longer — the mechanism, the caveats — hangs off the
//! title on hover, as everywhere else in the product.

use egui::{
    Align, Color32, CornerRadius, Layout, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, Vec2,
};

use super::{Icon, BUTTON_H};
use crate::theme;

/// Inset of a row's contents from the group's border.
const ROW_PAD: Vec2 = Vec2::new(16.0, 12.0);
/// The gap between a row's text and its control.
const ROW_GAP: f32 = 20.0;
/// How far a dependent row ("Size" under "Resolution") is indented.
const SUB_INDENT: f32 = 22.0;
/// Corner radius of a group.
const GROUP_RADIUS: u8 = 10;
const TITLE_SIZE: f32 = 14.0;
const DETAIL_SIZE: f32 = 12.5;

/// A section's small heading above its group ("Screen", "3D graphics").
pub fn section_title(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        ui.label(
            RichText::new(text)
                .size(12.5)
                .color(theme::TEXT_DIM)
                .strong(),
        );
    });
    ui.add_space(4.0);
}

/// A rounded group of rows on the card surface, hairlines between them.
pub fn group<R>(ui: &mut Ui, add: impl FnOnce(&mut Group<'_>) -> R) -> R {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(CornerRadius::same(GROUP_RADIUS))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut group = Group { ui, rows: 0 };
            add(&mut group)
        })
        .inner
}

/// The rows of one [`group`]. Each method lays out one row and draws the
/// divider above it when it is not the first.
pub struct Group<'u> {
    ui: &'u mut Ui,
    rows: usize,
}

impl Group<'_> {
    /// A setting: `title`, a line of `detail` under it, `tooltip` on hover of
    /// either, and `control` right-aligned and vertically centred.
    pub fn row<R>(
        &mut self,
        title: &str,
        detail: &str,
        tooltip: &str,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        self.row_at(0.0, title, detail, tooltip, control)
    }

    /// A row that belongs to the one above it — indented, so "Size" reads as
    /// part of "Resolution" rather than as a setting of its own.
    pub fn sub_row<R>(
        &mut self,
        title: &str,
        detail: &str,
        tooltip: &str,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        self.row_at(SUB_INDENT, title, detail, tooltip, control)
    }

    /// A pick-one row: a radio mark, a title and a line of detail; the whole
    /// row is the target. Disabled rows stay visible with the reason as their
    /// detail, because a choice that exists elsewhere should say why not here.
    pub fn choice(&mut self, title: &str, detail: &str, selected: bool, enabled: bool) -> Response {
        self.divider(0.0);
        let ui = &mut *self.ui;
        let width = ui.available_width();
        let mark_w = 30.0;
        let text_w = width - 2.0 * ROW_PAD.x - mark_w;
        let fg = if enabled {
            theme::TEXT
        } else {
            theme::TEXT_FAINT
        };
        let (title_g, detail_g) = text_galleys(ui, title, detail, text_w, fg);
        let text_h = text_height(&title_g, &detail_g);
        let height = text_h + 2.0 * ROW_PAD.y;
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);
        let hot = theme::animate_bool(
            ui.ctx(),
            response.id.with("hot"),
            enabled && response.hovered(),
            0.12,
        );
        let painter = ui.painter();
        if hot > 0.0 {
            painter.rect_filled(
                rect.shrink(1.0),
                CornerRadius::same(GROUP_RADIUS - 2),
                theme::mix(theme::CARD, theme::CARD_HOVER, hot),
            );
        }
        let center = egui::pos2(
            rect.left() + ROW_PAD.x + 9.0,
            rect.top() + ROW_PAD.y + title_g.size().y * 0.5,
        );
        if selected {
            painter.circle_stroke(center, 8.0, Stroke::new(1.5_f32, theme::ACCENT));
            painter.circle_filled(center, 4.0, theme::ACCENT);
        } else {
            painter.circle_stroke(
                center,
                8.0,
                Stroke::new(
                    1.5_f32,
                    theme::mix(theme::STROKE_STRONG, theme::TEXT_FAINT, hot),
                ),
            );
        }
        let origin = egui::pos2(rect.left() + ROW_PAD.x + mark_w, rect.top() + ROW_PAD.y);
        paint_text(painter, origin, title_g, detail_g);
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        self.rows += 1;
        response
    }

    /// Free content inside the group, padded like a row — a table, a meter.
    pub fn body<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> R {
        self.divider(0.0);
        let inner = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(ROW_PAD.x as i8, ROW_PAD.y as i8))
            .show(self.ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;
                add(ui)
            })
            .inner;
        self.rows += 1;
        inner
    }

    /// A full-width action at the foot of a group ("+ Add port forward").
    pub fn action(&mut self, icon: Icon, label: &str, enabled: bool) -> Response {
        self.divider(0.0);
        let ui = &mut *self.ui;
        let width = ui.available_width();
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 42.0), sense);
        let hot = enabled && response.hovered();
        let color = if !enabled {
            theme::TEXT_FAINT
        } else if hot {
            theme::RESUMING
        } else {
            theme::ACCENT
        };
        let painter = ui.painter();
        let icon_rect = Rect::from_center_size(
            egui::pos2(rect.left() + ROW_PAD.x + 7.0, rect.center().y),
            Vec2::splat(14.0),
        );
        super::paint_icon(painter, icon_rect, icon, color);
        painter.text(
            egui::pos2(icon_rect.right() + 9.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.5),
            color,
        );
        if hot {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        self.rows += 1;
        response
    }

    fn row_at<R>(
        &mut self,
        indent: f32,
        title: &str,
        detail: &str,
        tooltip: &str,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        self.divider(indent);
        let ui = &mut *self.ui;
        let width = ui.available_width();
        // The control's width is only known once it is laid out, and the text
        // has to wrap around it before that — so the text wraps against last
        // frame's width. A first frame is off by a few pixels at most and the
        // repaint below corrects it.
        let id = ui.id().with(("settings-row", title));
        let control_w: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(200.0);
        let text_w = (width - 2.0 * ROW_PAD.x - indent - control_w - ROW_GAP).max(120.0);
        let (title_g, detail_g) = text_galleys(ui, title, detail, text_w, theme::TEXT);
        let text_h = text_height(&title_g, &detail_g);
        let height = text_h.max(BUTTON_H) + 2.0 * ROW_PAD.y;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let text_size = Vec2::new(title_g.size().x.max(detail_g.size().x), text_h);
        let origin = egui::pos2(
            rect.left() + ROW_PAD.x + indent,
            rect.center().y - text_h * 0.5,
        );
        paint_text(ui.painter(), origin, title_g, detail_g);
        if !tooltip.is_empty() {
            ui.interact(
                Rect::from_min_size(origin, text_size),
                id.with("tip"),
                Sense::hover(),
            )
            .on_hover_text(tooltip);
        }
        let inner = rect.shrink2(ROW_PAD);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(inner)
                .layout(Layout::right_to_left(Align::Center)),
        );
        let out = control(&mut child);
        let measured = child.min_rect().width();
        if (measured - control_w).abs() > 0.5 {
            ui.data_mut(|d| d.insert_temp(id, measured));
            ui.ctx().request_repaint();
        }
        self.rows += 1;
        out
    }

    fn divider(&mut self, indent: f32) {
        if self.rows == 0 {
            return;
        }
        let rect = self.ui.available_rect_before_wrap();
        let x = egui::Rangef::new(rect.left() + indent, rect.right());
        theme::rule(self.ui.painter(), x, rect.top());
    }
}

fn text_galleys(
    ui: &Ui,
    title: &str,
    detail: &str,
    width: f32,
    title_color: Color32,
) -> (std::sync::Arc<egui::Galley>, std::sync::Arc<egui::Galley>) {
    let painter = ui.painter();
    let title_g = painter.layout(
        title.to_owned(),
        egui::FontId::proportional(TITLE_SIZE),
        title_color,
        width,
    );
    let detail_g = painter.layout(
        detail.to_owned(),
        egui::FontId::proportional(DETAIL_SIZE),
        theme::TEXT_DIM,
        width,
    );
    (title_g, detail_g)
}

fn text_height(title: &egui::Galley, detail: &egui::Galley) -> f32 {
    if detail.is_empty() {
        title.size().y
    } else {
        title.size().y + 3.0 + detail.size().y
    }
}

fn paint_text(
    painter: &egui::Painter,
    origin: egui::Pos2,
    title: std::sync::Arc<egui::Galley>,
    detail: std::sync::Arc<egui::Galley>,
) {
    let below = origin.y + title.size().y + 3.0;
    painter.galley(origin, title, Color32::TRANSPARENT);
    if !detail.is_empty() {
        painter.galley(egui::pos2(origin.x, below), detail, Color32::TRANSPARENT);
    }
}

/// A two-to-four-way choice drawn as one control: an inset track with the
/// picked segment raised. Returns true on the frame the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let font = egui::FontId::proportional(13.0);
    let pad = 3.0;
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), theme::TEXT)
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 28.0).collect();
    let total = widths.iter().sum::<f32>() + 2.0 * pad + (options.len() as f32 - 1.0) * 2.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, BUTTON_H), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::same(8), theme::INSET);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, theme::STROKE),
        StrokeKind::Inside,
    );
    let mut changed = false;
    let mut x = rect.left() + pad;
    for ((option, label), (galley, w)) in options.iter().zip(galleys.into_iter().zip(widths)) {
        let seg = Rect::from_min_size(
            egui::pos2(x, rect.top() + pad),
            Vec2::new(w, BUTTON_H - 2.0 * pad),
        );
        let response = ui.interact(seg, ui.id().with(("segment", *label)), Sense::click());
        let selected = *value == *option;
        if selected {
            painter.rect_filled(seg, CornerRadius::same(6), theme::SELECTED);
            painter.rect_stroke(
                seg,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, theme::STROKE_STRONG),
                StrokeKind::Inside,
            );
        }
        let color = if selected || response.hovered() {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        painter.galley_with_override_text_color(seg.center() - galley.size() * 0.5, galley, color);
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() && !selected {
            *value = *option;
            changed = true;
        }
        x += w + 2.0;
    }
    changed
}

/// An on/off switch. Returns the response; `changed()` on the flip.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(40.0, 22.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let value = *on;
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, value, label));
    let t = theme::animate_bool(ui.ctx(), response.id.with("switch"), value, 0.12);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(11),
        theme::mix(theme::STROKE_STRONG, theme::ACCENT_DEEP, t),
    );
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    painter.circle_filled(egui::pos2(x, rect.center().y), 8.0, theme::TEXT);
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A − n + stepper for a small count.
pub fn stepper(ui: &mut Ui, value: &mut u8, range: std::ops::RangeInclusive<u8>, label: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(104.0, BUTTON_H), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::same(8), theme::INSET);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, theme::STROKE),
        StrokeKind::Inside,
    );
    let minus = Rect::from_min_size(rect.min, Vec2::splat(BUTTON_H));
    let plus = Rect::from_min_size(
        egui::pos2(rect.right() - BUTTON_H, rect.top()),
        Vec2::splat(BUTTON_H),
    );
    for (button, down) in [(minus, true), (plus, false)] {
        let enabled = if down {
            *value > *range.start()
        } else {
            *value < *range.end()
        };
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let response = ui.interact(button, ui.id().with((label, down)), sense);
        let color = if !enabled {
            theme::TEXT_FAINT
        } else if response.hovered() {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        let c = button.center();
        let stroke = Stroke::new(1.6_f32, color);
        painter.hline(c.x - 5.0..=c.x + 5.0, c.y, stroke);
        if !down {
            painter.vline(c.x, c.y - 5.0..=c.y + 5.0, stroke);
        }
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() {
            *value = if down {
                value.saturating_sub(1)
            } else {
                value.saturating_add(1)
            };
        }
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        value.to_string(),
        egui::FontId::proportional(13.5),
        theme::TEXT,
    );
}

/// A collapsible group header ("Advanced") with a one-line summary of what is
/// inside, so a closed one still says what it holds. Returns whether it is
/// open; the state lives in egui's memory, not the form.
pub fn disclosure(ui: &mut Ui, id_salt: &str, title: &str, summary: &str) -> bool {
    let id = ui.id().with(("disclosure", id_salt));
    let mut open: bool = ui.data(|d| d.get_temp(id)).unwrap_or(false);
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::click());
    let hot = theme::animate_bool(ui.ctx(), response.id.with("hot"), response.hovered(), 0.12);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(GROUP_RADIUS),
        theme::mix(theme::CARD, theme::CARD_HOVER, hot),
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same(GROUP_RADIUS),
        Stroke::new(1.0_f32, theme::STROKE),
        StrokeKind::Inside,
    );
    let c = egui::pos2(rect.left() + ROW_PAD.x + 5.0, rect.center().y);
    let chevron = if open {
        vec![
            c + Vec2::new(-4.0, -2.0),
            c + Vec2::new(0.0, 2.0),
            c + Vec2::new(4.0, -2.0),
        ]
    } else {
        vec![
            c + Vec2::new(-2.0, -4.0),
            c + Vec2::new(2.0, 0.0),
            c + Vec2::new(-2.0, 4.0),
        ]
    };
    painter.add(egui::Shape::line(
        chevron,
        Stroke::new(1.6_f32, theme::TEXT_DIM),
    ));
    painter.text(
        egui::pos2(c.x + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(TITLE_SIZE),
        theme::TEXT,
    );
    painter.text(
        egui::pos2(rect.right() - ROW_PAD.x, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        summary,
        egui::FontId::proportional(DETAIL_SIZE),
        theme::TEXT_FAINT,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    open
}

/// A quiet information panel: an "i" mark and a sentence, accent-tinted.
pub fn callout(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::mix(theme::CARD, theme::ACCENT, 0.06))
        .stroke(Stroke::new(
            1.0_f32,
            theme::mix(theme::STROKE, theme::ACCENT, 0.25),
        ))
        .corner_radius(CornerRadius::same(GROUP_RADIUS))
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                let c = rect.center();
                let painter = ui.painter();
                painter.circle_stroke(c, 7.0, Stroke::new(1.3_f32, theme::ACCENT));
                painter.vline(
                    c.x,
                    c.y - 1.0..=c.y + 3.5,
                    Stroke::new(1.5_f32, theme::ACCENT),
                );
                painter.circle_filled(egui::pos2(c.x, c.y - 3.5), 0.9, theme::ACCENT);
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(RichText::new(text).size(DETAIL_SIZE).color(theme::TEXT_DIM))
                        .wrap(),
                );
            });
        });
}

/// A short status at a row's right edge: a dot and a word ("Ready").
pub fn status(ui: &mut Ui, text: &str, color: Color32) {
    let font = egui::FontId::proportional(DETAIL_SIZE);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 14.0, BUTTON_H), Sense::hover());
    let painter = ui.painter();
    painter.circle_filled(egui::pos2(rect.left() + 3.5, rect.center().y), 3.5, color);
    painter.galley(
        egui::pos2(rect.left() + 14.0, rect.center().y - galley.size().y * 0.5),
        galley,
        Color32::TRANSPARENT,
    );
}

/// The editor's section list: icon, label, the current one raised.
pub fn nav_item(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 38.0), Sense::click());
    let hot = theme::animate_bool(ui.ctx(), response.id.with("hot"), response.hovered(), 0.10);
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, CornerRadius::same(8), theme::SELECTED);
    } else if hot > 0.0 {
        painter.rect_filled(
            rect,
            CornerRadius::same(8),
            theme::mix(theme::BG_PANEL, theme::CARD, hot),
        );
    }
    let fg = if selected || response.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    super::paint_icon(
        painter,
        Rect::from_center_size(
            egui::pos2(rect.left() + 21.0, rect.center().y),
            Vec2::splat(16.0),
        ),
        icon,
        if selected { theme::ACCENT } else { fg },
    );
    painter.text(
        egui::pos2(rect.left() + 40.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.5),
        fg,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A thin labelled usage bar: "Processor ····· 4 of 16 cores".
pub fn usage(ui: &mut Ui, label: &str, value: &str, fraction: f32) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(label)
                    .size(DETAIL_SIZE)
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(value).size(DETAIL_SIZE).color(theme::TEXT));
            });
        });
        let width = ui.available_width();
        super::meter(ui, fraction, width, theme::ACCENT);
    });
}
