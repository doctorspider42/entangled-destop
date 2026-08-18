//! Modal surfaces: the new-machine wizard (GUI-1602), the delete confirmation
//! (GUI-1604) and the settings panel.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp, Modal};
use crate::discovery::format_bytes;
use crate::launcher::{self, VARIANTS};
use crate::theme;
use crate::ui;

pub fn show(ctx: &egui::Context, app: &mut ManagerApp, actions: &mut Vec<Action>) {
    if !app.modal.is_open() {
        return;
    }
    let ManagerApp {
        modal,
        settings,
        cli,
        ..
    } = app;

    let closed = match modal {
        Modal::None => false,
        Modal::Wizard(state) => {
            let cli_label = cli
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "entangled".to_string());
            frame(ctx, "wizard", "New machine", 470.0, |ui| {
                ui.label(ui::dim(
                    "The Debian installer runs in its own VM window; the manager tracks it \
                     and streams the console into the log pane.",
                ));
                ui.add_space(14.0);

                ui.label(ui::faint("NAME"));
                ui.add(
                    egui::TextEdit::singleline(&mut state.machine.name)
                        .desired_width(f32::INFINITY)
                        .hint_text("debian-1"),
                );
                ui.add_space(4.0);
                ui.label(ui::faint(format!(
                    "profile {} · disk {}",
                    state.machine.profile_path(&settings.vm_dir).display(),
                    state.machine.disk_path(&settings.vm_dir).display()
                )));
                ui.add_space(14.0);

                slider_row(ui, "MEMORY", |ui| {
                    ui.add(
                        egui::Slider::new(&mut state.machine.memory_mib, 512..=16384)
                            .step_by(256.0)
                            .suffix(" MiB"),
                    );
                });
                slider_row(ui, "vCPUs", |ui| {
                    ui.add(egui::Slider::new(&mut state.machine.vcpus, 1..=16));
                });
                slider_row(ui, "DISK", |ui| {
                    ui.add(
                        egui::Slider::new(&mut state.machine.disk_gib, 8..=256)
                            .step_by(2.0)
                            .suffix(" GiB"),
                    );
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(ui::faint("INSTALLER"));
                    ui.add_space(8.0);
                    egui::ComboBox::from_id_salt("variant")
                        .selected_text(state.machine.variant.clone())
                        .show_ui(ui, |ui| {
                            for variant in VARIANTS {
                                ui.selectable_value(
                                    &mut state.machine.variant,
                                    variant.to_string(),
                                    variant,
                                );
                            }
                        });
                });
                ui.add_space(10.0);
                ui.checkbox(&mut state.machine.automated, "Automated installation")
                    .on_hover_text(
                        "Preseeded Debian with the Weston desktop profile — no questions asked",
                    );
                ui.checkbox(&mut state.machine.headless, "Headless installer")
                    .on_hover_text("No installer window; the serial console still streams here");

                ui.add_space(12.0);
                let machine = state.machine.clone();
                let spec = launcher::install_spec(
                    &std::path::PathBuf::from(&cli_label),
                    &settings.vm_dir,
                    &machine,
                );
                egui::Frame::new()
                    .fill(theme::INSET)
                    .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(spec.command_line())
                                .monospace()
                                .size(11.5)
                                .color(theme::TEXT_DIM),
                        );
                    });

                if let Some(error) = &state.error {
                    ui.add_space(10.0);
                    ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui::primary_button(ui, "Create machine").clicked() {
                        actions.push(Action::SubmitWizard);
                    }
                    if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
                        actions.push(Action::CloseModal);
                    }
                });
            })
        }
        Modal::Delete(state) => {
            let matches = state.typed.trim() == state.name;
            frame(
                ctx,
                "delete",
                &format!("Delete {}", state.name),
                470.0,
                |ui| {
                    ui.label(
                        RichText::new("This removes the profile and the disk image permanently.")
                            .color(theme::TEXT)
                            .size(13.0),
                    );
                    ui.add_space(12.0);
                    for path in &state.plan.remove {
                        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        ui.label(ui::faint(format!(
                            "− {}  ({})",
                            path.display(),
                            format_bytes(size)
                        )));
                    }
                    for kept in &state.plan.kept_outside_vm_dir {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!(
                                "{} lies outside the VM directory and is kept",
                                kept.display()
                            ))
                            .color(theme::WARN)
                            .size(12.0),
                        );
                    }

                    ui.add_space(14.0);
                    ui.label(ui::faint(format!("TYPE '{}' TO CONFIRM", state.name)));
                    ui.add(
                        egui::TextEdit::singleline(&mut state.typed)
                            .desired_width(f32::INFINITY)
                            // Explicitly faint: a placeholder that reads like
                            // real input is how people delete the wrong VM.
                            .hint_text(
                                RichText::new(state.name.clone())
                                    .color(theme::TEXT_FAINT)
                                    .italics(),
                            ),
                    );
                    if let Some(error) = &state.error {
                        ui.add_space(8.0);
                        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                    }

                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if ui::ghost_button(ui, "Delete permanently", matches, theme::ERR).clicked()
                        {
                            actions.push(Action::ConfirmDelete);
                        }
                        if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
                            actions.push(Action::CloseModal);
                        }
                    });
                },
            )
        }
        Modal::Settings(form) => frame(ctx, "settings", "Settings", 560.0, |ui| {
            ui.label(ui::faint("VM DIRECTORY"));
            ui.add(
                egui::TextEdit::singleline(&mut form.vm_dir)
                    .desired_width(f32::INFINITY)
                    .hint_text("~/entangled-vms"),
            );
            ui.label(ui::faint(
                "Scanned for *.toml profiles; new disks land here",
            ));
            ui.add_space(14.0);

            ui.label(ui::faint("ENTANGLED BINARY"));
            ui.add(
                egui::TextEdit::singleline(&mut form.entangled_binary)
                    .desired_width(f32::INFINITY)
                    .hint_text("empty = next to this manager, then $PATH"),
            );
            match cli {
                Ok(path) => ui.label(ui::faint(format!("resolved: {}", path.display()))),
                Err(message) => {
                    ui.label(RichText::new(message.as_str()).color(theme::ERR).size(11.5))
                }
            };
            ui.add_space(14.0);

            ui.label(ui::faint("WORKING DIRECTORY FOR VMs"));
            ui.add(
                egui::TextEdit::singleline(&mut form.work_dir)
                    .desired_width(f32::INFINITY)
                    .hint_text("empty = the manager's own working directory"),
            );
            ui.label(ui::faint(
                "Profiles may hold relative paths (artifacts/bootstrap/vmlinuz); they resolve \
                 against this directory",
            ));
            ui.add_space(14.0);

            ui.checkbox(&mut form.headless_install, "Install headless by default");

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if ui::primary_button(ui, "Save").clicked() {
                    actions.push(Action::SaveSettings);
                }
                if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
                    actions.push(Action::CloseModal);
                }
            });
        }),
    };

    if closed {
        actions.push(Action::CloseModal);
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseModal);
    }
}

fn slider_row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(78.0, 20.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(ui::faint(label));
            },
        );
        add(ui);
    });
}

/// Shared modal chrome: dimmed backdrop, panel surface, gradient title rule.
/// Returns true when the user dismissed it by clicking outside.
fn frame<R>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    width: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> bool {
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
                .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
                .shadow(egui::Shadow {
                    offset: [0, 18],
                    blur: 40,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(200),
                })
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            let time = ui.input(|i| i.time);
            ui.horizontal(|ui| {
                let (mark, _) = ui.allocate_exact_size(Vec2::new(34.0, 30.0), egui::Sense::hover());
                crate::logo::paint_mark(ui.painter(), mark, time, 0.85);
                ui.add_space(4.0);
                ui.label(RichText::new(title).size(19.0).color(theme::TEXT).strong());
            });
            ui.add_space(8.0);
            let rule = egui::Rect::from_min_size(
                ui.cursor().left_top(),
                Vec2::new(ui.available_width(), 1.5),
            );
            theme::gradient_rect(
                ui.painter(),
                rule,
                theme::CYAN.gamma_multiply(0.7),
                theme::VIOLET.gamma_multiply(0.2),
            );
            ui.add_space(12.0);
            add(ui);
        });
    response.should_close()
}
