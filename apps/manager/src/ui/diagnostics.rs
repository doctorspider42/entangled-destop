//! The Diagnostics view: what this host can do, and what it is missing.
//!
//! It is `entangled doctor`, rendered. The CLI already knows the answers and
//! keeps them per host; duplicating that knowledge in the GUI would guarantee
//! the two drift. So this view runs the command and colours the result — the
//! `MISSING` lines in the warning colour, the instruction that follows each one
//! in the quiet one.
//!
//! It is also where the engine itself is reported: which binary, which version,
//! how it was found. That is the information the old Settings panel *asked*
//! for, and it belongs here, as an answer.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp};
use crate::backend::Backend;
use crate::hostcheck::Level;
use crate::theme;
use crate::ui;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            theme::paint_backdrop(ui);
            super::cards::banners(ui, app, actions);
            heading(ui, app, actions);
            ui.add_space(12.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    engine_card(ui, app, actions);
                    ui.add_space(12.0);
                    backend_card(ui, app);
                    ui.add_space(12.0);
                    report_card(ui, app);
                });
        });
}

fn heading(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Diagnostics")
                    .size(24.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.label(ui::dim(
                "What this computer can run, and anything that is missing.",
            ));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if app.doctor_running {
                "Checking…"
            } else {
                "Run the check"
            };
            if ui::primary_button(ui, label).clicked() && !app.doctor_running {
                actions.push(Action::RunDiagnostics);
            }
        });
    });
}

/// The engine, reported rather than requested — the whole point of this
/// deliverable. A failure here is not a text field to fill in; it is a problem
/// with a fix button next to it.
fn engine_card(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    panel(ui, "Engine", theme::CYAN, |ui| match &app.engine {
        Ok(engine) => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    RichText::new(engine.summary())
                        .size(14.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                ui::chip(ui, "ready", theme::OK);
            });
            ui.add_space(4.0);
            ui.add(egui::Label::new(ui::faint(engine.path.display().to_string())).truncate())
                .on_hover_text(
                    "The part of Entangled that actually runs a machine. The manager finds \
                     it by itself; you only ever choose it by hand if this line is wrong.",
                );
        }
        Err(message) => {
            ui.label(RichText::new(message).color(theme::ERR).size(13.0));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui::ghost_button(ui, "Locate it…", true, theme::CYAN)
                    .on_hover_text("Browse for the entangled program on this computer")
                    .clicked()
                {
                    actions.push(Action::OpenSettings);
                }
            });
        }
    });
}

/// Where machines run, and what that costs them. Only interesting where there
/// is a choice — on Linux this collapses to one honest line.
fn backend_card(ui: &mut egui::Ui, app: &ManagerApp) {
    panel(ui, "Where machines run", theme::VIOLET, |ui| {
        let default = app.settings.default_backend;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(ui::dim("Default for new machines:"));
            ui::chip(ui, default.label(), theme::VIOLET);
        })
        .response
        .on_hover_text(default.tooltip());

        if Backend::Wsl.available_on_host() {
            ui.add_space(6.0);
            ui.label(ui::faint(format!(
                "WSL distribution: {}",
                app.settings.wsl_distro
            )));
            ui.label(ui::faint(match &app.settings.wsl_entangled {
                Some(path) => format!("Linux engine: {path}"),
                None => "Linux engine: whatever `entangled` resolves to inside WSL".to_string(),
            }));
            ui.add_space(6.0);
            for (capability, blocked) in [
                ("3D acceleration", Backend::Native.virgl_block()),
                ("TAP networking", Backend::Native.tap_block()),
                ("Debian installer", Backend::Native.debian_install_block()),
            ] {
                let line = match blocked {
                    None => format!("{capability}: available on both backends"),
                    Some(_) => format!("{capability}: WSL (KVM) only"),
                };
                ui.label(ui::faint(line)).on_hover_text(
                    blocked
                        .map(|block| block.long)
                        .unwrap_or("Available on every backend this host has."),
                );
            }
        }
    });
}

fn report_card(ui: &mut egui::Ui, app: &ManagerApp) {
    panel(ui, "Host report", theme::OK, |ui| {
        let Some(report) = &app.doctor else {
            ui.label(ui::dim(
                "No check has run yet. \"Run the check\" asks the engine what this host \
                 can do — virtualisation, firmware, installer media and free space.",
            ));
            return;
        };
        let tint = if report.healthy {
            theme::OK
        } else {
            theme::ERR
        };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(report.headline())
                    .size(14.0)
                    .color(tint)
                    .strong(),
            );
            ui::chip(ui, report.backend.label(), theme::VIOLET);
        });
        if !report.command.is_empty() {
            ui.add_space(2.0);
            ui.add(
                egui::Label::new(
                    RichText::new(&report.command)
                        .monospace()
                        .size(10.5)
                        .color(theme::TEXT_FAINT),
                )
                .truncate(),
            )
            .on_hover_text("The exact command; you can repeat it in a terminal.");
        }
        ui.add_space(10.0);
        egui::Frame::new()
            .fill(theme::INSET)
            .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
            .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for line in &report.lines {
                    let color = match line.level {
                        Level::Missing => theme::WARN,
                        Level::Failure => theme::ERR,
                        Level::Info => theme::TEXT_DIM,
                    };
                    ui.label(
                        RichText::new(&line.text)
                            .monospace()
                            .size(11.5)
                            .color(color),
                    );
                }
            });
    });
}

/// A titled panel with the card surface and the accent hairline — the shape the
/// rest of the product already uses, at full width.
fn panel(ui: &mut egui::Ui, title: &str, tint: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let bar = egui::Rect::from_min_size(
                ui.max_rect().left_top() - Vec2::new(0.0, 10.0),
                Vec2::new(ui.available_width(), 2.0),
            );
            theme::gradient_rect(
                ui.painter(),
                bar,
                tint.gamma_multiply(0.8),
                theme::VIOLET.gamma_multiply(0.5),
            );
            ui.label(
                RichText::new(title.to_uppercase())
                    .monospace()
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(8.0);
            add(ui);
        });
}
