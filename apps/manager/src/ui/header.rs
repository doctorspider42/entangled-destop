//! The title bar: animated mark, wordmark and the global actions.

use egui::{Align, Layout, Rect, RichText, Vec2};

use crate::app::{Action, ManagerApp};
use crate::logo;
use crate::theme;
use crate::ui;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    let time = ctx.input(|i| i.time);

    egui::TopBottomPanel::top("header")
        .exact_height(84.0)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .inner_margin(egui::Margin::symmetric(22, 12)),
        )
        .show(ctx, |ui| {
            let panel = ui.max_rect();
            ui.horizontal(|ui| {
                // The mark, drawn by the painter — no assets involved.
                let (mark_rect, _) =
                    ui.allocate_exact_size(Vec2::new(62.0, 56.0), egui::Sense::hover());
                logo::paint_mark(ui.painter(), mark_rect, time, 1.0);

                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("ENTANGLED")
                            .size(24.0)
                            .color(theme::TEXT)
                            .strong(),
                    );
                    ui.label(
                        RichText::new("desktop virtual machines")
                            .size(12.0)
                            .color(theme::TEXT_DIM),
                    );
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::primary_button(ui, "+  New machine").clicked() {
                        actions.push(Action::OpenWizard);
                    }
                    if ui::ghost_button(ui, "Settings", true, theme::TEXT_DIM).clicked() {
                        actions.push(Action::OpenSettings);
                    }
                    let logs_label = if app.log_open { "Hide log" } else { "Log" };
                    let tint = if app.supervisor.any_active() {
                        theme::CYAN
                    } else {
                        theme::TEXT_DIM
                    };
                    if ui::ghost_button(ui, logs_label, true, tint).clicked() {
                        actions.push(Action::ToggleLogPane);
                    }
                    if ui::ghost_button(ui, "Refresh", true, theme::TEXT_DIM).clicked() {
                        actions.push(Action::Refresh);
                    }

                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.with_layout(Layout::top_down(Align::Max), |ui| {
                            ui.add_space(6.0);
                            let count = app.scan.vms.len();
                            let running = app
                                .scan
                                .vms
                                .iter()
                                .filter(|vm| app.supervisor.is_busy(&vm.name))
                                .count();
                            ui.label(ui::dim(format!(
                                "{count} machine{} · {running} active",
                                if count == 1 { "" } else { "s" }
                            )));
                            ui.label(ui::faint(app.settings.vm_dir.display().to_string()))
                                .on_hover_text("Change it in Settings");
                        });
                    });
                });
            });

            // A hairline of the accent gradient along the bottom edge, tying
            // the two halves of the palette together. Painted inside the panel
            // so modals and popups still sit above it.
            let line = Rect::from_min_size(
                egui::pos2(panel.left() - 22.0, panel.bottom() + 10.5),
                Vec2::new(panel.width() + 44.0, 1.5),
            );
            theme::gradient_rect(
                ui.painter(),
                line,
                theme::CYAN.gamma_multiply(0.55),
                theme::VIOLET.gamma_multiply(0.55),
            );
        });
}
