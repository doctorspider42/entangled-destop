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
                            ui.label(
                                RichText::new(glyph(toast.level))
                                    .color(tint.gamma_multiply(alpha))
                                    .size(13.0)
                                    .strong(),
                            );
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

fn glyph(level: ToastLevel) -> &'static str {
    match level {
        ToastLevel::Info => "·",
        ToastLevel::Success => "✓",
        ToastLevel::Warn => "!",
        ToastLevel::Error => "×",
    }
}
