//! The title bar: animated mark, wordmark and the global actions.

use egui::{Align, Layout, Rect, RichText, Stroke, StrokeKind, Vec2};

use crate::app::{Action, ManagerApp};
use crate::logo;
use crate::theme;
use crate::ui;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    let time = theme::animation_time(ctx);

    egui::TopBottomPanel::top("header")
        .exact_height(76.0)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .inner_margin(egui::Margin::symmetric(20, 10)),
        )
        .show(ctx, |ui| {
            let panel = ui.max_rect();
            ui.horizontal(|ui| {
                let (mark_rect, _) =
                    ui.allocate_exact_size(Vec2::new(52.0, 52.0), egui::Sense::hover());
                logo::paint_mark(ui.painter(), mark_rect, time, 1.0);

                ui.add_space(7.0);
                ui.vertical(|ui| {
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(
                            RichText::new("ENTANGLED")
                                .size(21.0)
                                .color(theme::TEXT)
                                .strong(),
                        );
                        ui.label(
                            RichText::new("CONTROL PLANE")
                                .monospace()
                                .size(10.5)
                                .color(theme::accent(0.35)),
                        );
                    });
                    ui.label(
                        RichText::new("Virtual machines, made approachable")
                            .size(12.0)
                            .color(theme::TEXT_DIM),
                    );
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::primary_button(ui, "+  Create machine").clicked() {
                        actions.push(Action::OpenWizard);
                    }
                    if ui::ghost_button(ui, "Settings", true, theme::TEXT_DIM).clicked() {
                        actions.push(Action::OpenSettings);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("v{}", crate::VERSION))
                            .monospace()
                            .size(10.5)
                            .color(theme::TEXT_FAINT),
                    );
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

    egui::SidePanel::left("navigation")
        .exact_width(204.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .inner_margin(egui::Margin::symmetric(14, 18)),
        )
        .show(ctx, |ui| {
            ui.label(
                RichText::new("LIBRARY")
                    .monospace()
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(8.0);
            if nav_item(
                ui,
                "Machines",
                "Your virtual computers",
                app.view == crate::app::View::Machines,
                theme::CYAN,
            )
            .clicked()
                && app.view != crate::app::View::Machines
            {
                actions.push(Action::SwitchView(crate::app::View::Machines));
            }
            if nav_item(
                ui,
                "Storage",
                "Disk images and attachments",
                app.view == crate::app::View::Disks,
                theme::VIOLET,
            )
            .clicked()
                && app.view != crate::app::View::Disks
            {
                actions.push(Action::SwitchView(crate::app::View::Disks));
            }

            ui.add_space(20.0);
            ui.label(
                RichText::new("OPERATIONS")
                    .monospace()
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(8.0);
            let activity = if app.log_open {
                "Hide activity"
            } else {
                "Activity"
            };
            let activity_tint = if app.supervisor.any_active() {
                theme::CYAN
            } else {
                theme::OK
            };
            if nav_item(
                ui,
                activity,
                "Installs, starts and console output",
                app.log_open,
                activity_tint,
            )
            .clicked()
            {
                actions.push(Action::ToggleLogPane);
            }
            if nav_item(
                ui,
                "Refresh",
                "Scan the machine library now",
                false,
                theme::TEXT_DIM,
            )
            .clicked()
            {
                actions.push(Action::Refresh);
            }

            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), 146.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        egui::Frame::new()
                            .fill(theme::INSET)
                            .stroke(Stroke::new(1.0_f32, theme::STROKE))
                            .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                            .inner_margin(egui::Margin::same(11))
                            .show(ui, |ui| {
                                let count = app.scan.vms.len();
                                let active = app
                                    .scan
                                    .vms
                                    .iter()
                                    .filter(|vm| app.supervisor.is_busy(&vm.name))
                                    .count();
                                ui.label(
                                    RichText::new("HOST STATUS")
                                        .monospace()
                                        .size(10.0)
                                        .color(theme::TEXT_FAINT),
                                );
                                ui.add_space(5.0);
                                ui.label(
                                    RichText::new(format!("{count} machines · {active} active"))
                                        .size(12.5)
                                        .color(theme::TEXT),
                                );
                                if let Some(cpu) = app.stats.host.cpu_percent {
                                    ui.label(ui::faint(format!("CPU {cpu:.0}%")));
                                }
                                if let Some((free, _)) = app.stats.host.vm_dir_space {
                                    ui.label(ui::faint(format!(
                                        "{} storage free",
                                        crate::discovery::format_bytes(free)
                                    )));
                                }
                                ui.add_space(5.0);
                                ui.add(
                                    egui::Label::new(ui::faint(
                                        app.settings.vm_dir.display().to_string(),
                                    ))
                                    .truncate(),
                                )
                                .on_hover_text(app.settings.vm_dir.display().to_string());
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(if app.settings.animations_enabled {
                                        "MOTION ON"
                                    } else {
                                        "MOTION OFF"
                                    })
                                    .monospace()
                                    .size(9.5)
                                    .color(
                                        if app.settings.animations_enabled {
                                            theme::CYAN_DEEP
                                        } else {
                                            theme::TEXT_FAINT
                                        },
                                    ),
                                );
                            });
                    },
                );
            });
        });
}

fn nav_item(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    active: bool,
    tint: egui::Color32,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 54.0), egui::Sense::click());
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("nav-hover"),
        response.hovered(),
        0.14,
    );
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(theme::CONTROL_RADIUS),
        theme::mix(
            theme::BG_PANEL,
            tint,
            if active { 0.13 } else { hover * 0.06 },
        ),
    );
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(theme::CONTROL_RADIUS),
        Stroke::new(
            1.0_f32,
            if active {
                tint.gamma_multiply(0.55)
            } else {
                theme::STROKE.gamma_multiply(hover)
            },
        ),
        StrokeKind::Inside,
    );
    if active {
        ui.painter().rect_filled(
            Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 10.0), Vec2::new(2.0, 34.0)),
            egui::CornerRadius::same(1),
            tint,
        );
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(12.0, 8.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    child.label(
        RichText::new(title)
            .size(13.5)
            .color(if active { theme::TEXT } else { theme::TEXT_DIM })
            .strong(),
    );
    child.label(RichText::new(detail).size(10.5).color(theme::TEXT_FAINT));
    response
}
