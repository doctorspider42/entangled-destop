//! Transient notifications. Every error the manager meets ends up here (or in
//! a banner), never in a panic.

use egui::{Align2, RichText, Vec2};

use crate::app::{Action, Toast, ToastLevel};
use crate::theme;

/// Fade-in / fade-out window, seconds.
const FADE: f64 = 0.35;

pub fn show(ctx: &egui::Context, toasts: &[Toast], actions: &mut Vec<Action>) {
    if toasts.is_empty() {
        return;
    }
    let now = ctx.input(|i| i.time);

    egui::Area::new(egui::Id::new("toasts"))
        .anchor(Align2::RIGHT_TOP, Vec2::new(-20.0, 100.0))
        .order(egui::Order::Foreground)
        .interactable(true)
        .show(ctx, |ui| {
            ui.set_max_width(430.0);
            for (index, toast) in toasts.iter().enumerate() {
                let age = if toast.born.is_nan() {
                    0.0
                } else {
                    now - toast.born
                };
                let alpha = (age / FADE)
                    .min((toast.lifetime() - age) / FADE)
                    .clamp(0.0, 1.0) as f32;
                let tint = tint(toast.level);

                let response = egui::Frame::new()
                    .fill(theme::mix(theme::BG_PANEL, tint, 0.10).gamma_multiply(alpha))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        tint.gamma_multiply(0.55 * alpha),
                    ))
                    .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .shadow(egui::Shadow {
                        offset: [0, 6],
                        blur: 18,
                        spread: 0,
                        color: egui::Color32::from_black_alpha((140.0 * alpha) as u8),
                    })
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            marker(ui, toast.level, tint, alpha);
                            ui.label(
                                RichText::new(&toast.text)
                                    .color(theme::TEXT.gamma_multiply(alpha))
                                    .size(13.0),
                            );
                        });
                    })
                    .response;

                if response.interact(egui::Sense::click()).clicked() {
                    actions.push(Action::DismissToast(index));
                }
                ui.add_space(8.0);
            }
        });
}

fn tint(level: ToastLevel) -> egui::Color32 {
    match level {
        ToastLevel::Info => theme::CYAN,
        ToastLevel::Success => theme::OK,
        ToastLevel::Warn => theme::WARN,
        ToastLevel::Error => theme::ERR,
    }
}

/// Severity marker, painted rather than typed: the bundled fonts have no glyph
/// for a check mark, and a tofu box next to "deleted …" is not reassuring.
/// A ring for information, a filled disc for success, a bar for a warning and a
/// cross for an error — legible at 10 px, no font involved.
fn marker(ui: &mut egui::Ui, level: ToastLevel, tint: egui::Color32, alpha: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
    let painter = ui.painter();
    let color = tint.gamma_multiply(alpha);
    let center = rect.center();
    match level {
        ToastLevel::Info => {
            painter.circle_stroke(center, 4.0, egui::Stroke::new(1.6_f32, color));
        }
        ToastLevel::Success => {
            painter.circle_filled(center, 4.0, color);
        }
        ToastLevel::Warn => {
            painter.line_segment(
                [center - Vec2::new(0.0, 4.5), center + Vec2::new(0.0, 1.0)],
                egui::Stroke::new(2.0_f32, color),
            );
            painter.circle_filled(center + Vec2::new(0.0, 4.0), 1.2, color);
        }
        ToastLevel::Error => {
            for d in [Vec2::new(3.2, 3.2), Vec2::new(3.2, -3.2)] {
                painter.line_segment([center - d, center + d], egui::Stroke::new(1.8_f32, color));
            }
        }
    }
}
