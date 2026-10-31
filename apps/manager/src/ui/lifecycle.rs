//! One persistent activity treatment on cards and the machine's screen.
//! No percentage is invented: completion follows the engine's lifecycle reply.
use crate::{app::Status, theme};
use egui::{Align2, FontId, Rect, Stroke, Vec2};

pub fn paint(ui: &egui::Ui, bounds: Rect, status: Status) {
    let compact = bounds.height() < 210.0;
    let time = theme::animation_time(ui.ctx());
    let color = status.color();
    let painter = ui.painter().with_clip_rect(bounds);
    let center = bounds.center();
    painter.rect_filled(
        bounds,
        egui::CornerRadius::same(theme::CARD_RADIUS),
        theme::INSET.gamma_multiply(0.88),
    );
    let diameter = if compact { 46.0 } else { 74.0 };
    let mark = Rect::from_center_size(
        center - Vec2::new(0.0, if compact { 17.0 } else { 35.0 }),
        Vec2::splat(diameter),
    );
    let breath = 0.5 + 0.5 * (time * 1.8).sin() as f32;
    painter.circle_stroke(
        mark.center(),
        diameter * 0.58,
        Stroke::new(1.0_f32, color.gamma_multiply(0.10 + 0.08 * breath)),
    );
    crate::logo::paint_mark(&painter, mark.shrink(8.0), theme::INSET);
    crate::logo::paint_spinner(&painter, mark.expand(12.0), time * 0.45, color);
    let title = if status == Status::Suspending {
        "Saving your place"
    } else {
        "Waking your machine"
    };
    painter.text(
        center + Vec2::new(0.0, if compact { 21.0 } else { 25.0 }),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(if compact { 13.0 } else { 21.0 }),
        theme::TEXT,
    );
    if !compact {
        painter.text(
            center + Vec2::new(0.0, 53.0),
            Align2::CENTER_CENTER,
            if status == Status::Suspending {
                "Your session is being saved."
            } else {
                "Your apps and windows are coming back."
            },
            FontId::proportional(13.0),
            theme::TEXT_DIM,
        );
    }
    let rail = Rect::from_center_size(
        center + Vec2::new(0.0, if compact { 44.0 } else { 84.0 }),
        Vec2::new(if compact { 88.0 } else { 144.0 }, 2.0),
    );
    painter.rect_filled(rail, 1, theme::STROKE);
    let length = rail.width() * 0.22;
    let at = (0.5 + 0.5 * (time * 1.6).sin() as f32) * (rail.width() - length);
    painter.rect_filled(
        Rect::from_min_size(rail.min + Vec2::new(at, 0.0), Vec2::new(length, 2.0)),
        1,
        color,
    );
}
