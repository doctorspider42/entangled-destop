//! Modal surfaces: the new-machine wizard (GUI-1602), the delete confirmation
//! (GUI-1604) and the settings panel.

use egui::{Align, Layout, RichText, Vec2};

use control_api::GpuRenderer;

use crate::app::{Action, ManagerApp, Modal};
use crate::backend::Backend;
use crate::discovery::format_bytes;
use crate::launcher::{self, VARIANTS};
use crate::picker::PickTarget;
use crate::theme;
use crate::ui;
use crate::wslengine;

pub fn show(ctx: &egui::Context, app: &mut ManagerApp, actions: &mut Vec<Action>) {
    if !app.modal.is_open() {
        return;
    }
    let ManagerApp {
        modal,
        settings,
        engine,
        scan,
        supervisor,
        wsl_engine,
        wsl_install_running,
        ..
    } = app;
    let wsl_install_running = *wsl_install_running;

    let closed = match modal {
        Modal::None => false,
        Modal::Wizard(state) => {
            let cli_label = engine
                .as_ref()
                .map(|engine| engine.path.display().to_string())
                .unwrap_or_else(|_| "entangled".to_string());
            frame(ctx, "wizard", "Create a machine", 760.0, |ui| {
                ui::form_scope(ui);
                wizard_progress(ui, state.step);
                ui.add_space(18.0);

                match state.step {
                    0 => {
                        wizard_heading(
                            ui,
                            "Choose an operating system",
                            "Everything the installer needs is downloaded and checked for you.",
                        );
                        ui.add_space(14.0);
                        // An installer this backend cannot run is greyed out
                        // with the reason on hover, rather than failing forty
                        // minutes later inside the engine.
                        ui.horizontal(|ui| {
                            let card_width =
                                (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
                            // Debian is offered on both hosts now: its bootstrap
                            // kernel is downloaded rather than built where it
                            // cannot be built. Whether *this* machine has it yet
                            // is a pre-flight question with a command as its
                            // answer (launcher::missing_install_artifact), not a
                            // backend capability, so nothing is greyed out here.
                            let debian = option_card(
                                ui,
                                card_width,
                                "Debian",
                                "Verified network or official installer media",
                                state.machine.family == launcher::GuestFamily::Debian,
                                true,
                                theme::ACCENT,
                            );
                            if debian.clicked() {
                                state.machine.family = launcher::GuestFamily::Debian;
                            }
                            if option_card(
                                ui,
                                card_width,
                                "Ubuntu",
                                "UEFI installer from cache or a local ISO",
                                state.machine.family == launcher::GuestFamily::Ubuntu,
                                true,
                                theme::ACCENT,
                            )
                            .clicked()
                            {
                                state.machine.family = launcher::GuestFamily::Ubuntu;
                            }
                        });
                        ui.add_space(16.0);
                        match state.machine.family {
                            launcher::GuestFamily::Debian => {
                                ui::form_row(
                                    ui,
                                    "INSTALLER",
                                    "Which Debian installer runs: the fast keyboard-driven one, \
                                     the graphical one, or the official netinst disc image, \
                                     downloaded and signature-checked for you.",
                                    |ui, field_w| {
                                        egui::ComboBox::from_id_salt("variant")
                                            .selected_text(variant_label(&state.machine.variant))
                                            .width(ui::combo_width(field_w))
                                            .show_ui(ui, |ui| {
                                                for variant in VARIANTS {
                                                    ui.selectable_value(
                                                        &mut state.machine.variant,
                                                        variant.to_string(),
                                                        variant_label(variant),
                                                    );
                                                }
                                            });
                                    },
                                );
                            }
                            launcher::GuestFamily::Ubuntu => {
                                if ui::path_row(
                                    ui,
                                    "INSTALLER",
                                    "The Ubuntu disc image to install from. Leave it empty and \
                                     Entangled uses the newest one it has already downloaded and \
                                     signature-checked.",
                                    &mut state.machine.iso_path,
                                    "newest verified download",
                                    true,
                                ) {
                                    actions.push(Action::PickPath(PickTarget::WizardIso));
                                }
                            }
                        }
                        ui.add_space(16.0);
                        backend_row(ui, &mut state.machine.backend, wsl_engine);
                        if state.machine.backend == Backend::Wsl {
                            wsl_setup_note(
                                ui,
                                wsl_engine,
                                wsl_install_running,
                                ui::FORM_LABEL_W,
                                actions,
                            );
                        }
                    }
                    1 => {
                        wizard_heading(
                            ui,
                            "Name it and size it",
                            "You can change all of this later from the machine's page.",
                        );
                        ui.add_space(18.0);
                        ui::form_row(
                            ui,
                            "NAME",
                            "Names the machine, its disk file and the guest's own hostname, so \
                             pick something you will recognise later.",
                            |ui, field_w| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.machine.name)
                                        .desired_width(field_w)
                                        .hint_text("my-machine"),
                                );
                            },
                        );
                        ui.add_space(18.0);
                        ui.label(RichText::new("Power").size(13.5).color(theme::TEXT));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let gap = ui.spacing().item_spacing.x;
                            let card_width = ((ui.available_width() - 2.0 * gap) / 3.0).floor();
                            for preset in POWER_PRESETS {
                                let selected = state.machine.vcpus == preset.vcpus
                                    && state.machine.memory_mib == preset.memory_mib;
                                if option_card(
                                    ui,
                                    card_width,
                                    preset.name,
                                    &format!(
                                        "{} cores · {} GiB · {}",
                                        preset.vcpus,
                                        preset.memory_mib / 1024,
                                        preset.hint
                                    ),
                                    selected,
                                    true,
                                    theme::ACCENT,
                                )
                                .clicked()
                                {
                                    state.machine.vcpus = preset.vcpus;
                                    state.machine.memory_mib = preset.memory_mib;
                                }
                            }
                        });
                        if !POWER_PRESETS.iter().any(|p| {
                            p.vcpus == state.machine.vcpus
                                && p.memory_mib == state.machine.memory_mib
                        }) {
                            ui.add_space(6.0);
                            ui.label(ui::faint(format!(
                                "Custom: {} cores · {} — set under More options",
                                state.machine.vcpus,
                                format_bytes(state.machine.memory_mib * 1024 * 1024)
                            )));
                        }
                        if state.machine.disk_mode == launcher::DiskMode::CreateNew {
                            ui.add_space(18.0);
                            ui.label(RichText::new("Disk").size(13.5).color(theme::TEXT));
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                for gib in DISK_CHOICES_GIB {
                                    if choice_chip(
                                        ui,
                                        &format!("{gib} GiB"),
                                        state.machine.disk_gib == gib,
                                    )
                                    .clicked()
                                    {
                                        state.machine.disk_gib = gib;
                                    }
                                }
                            });
                            ui.add_space(6.0);
                            ui.label(ui::faint(
                                "Takes only what the guest writes — a fresh install uses a \
                                 fraction of it.",
                            ));
                        }

                        ui.add_space(18.0);
                        let toggle = if state.more {
                            "Fewer options"
                        } else {
                            "More options — every setting by hand"
                        };
                        if ui::text_button(ui, toggle, theme::ACCENT).clicked() {
                            state.more = !state.more;
                        }
                        if state.more {
                            ui.add_space(8.0);
                            more_options(ui, state, settings, scan, actions);
                        }
                    }
                    _ => {
                        wizard_heading(
                            ui,
                            "Ready to build",
                            "Review the plan. Nothing starts until you confirm.",
                        );
                        ui.add_space(14.0);
                        egui::Frame::new()
                            .fill(theme::CARD)
                            .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
                            .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
                            .inner_margin(egui::Margin::same(16))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                summary_row(ui, "Machine", &state.machine.name);
                                summary_row(
                                    ui,
                                    "Installer",
                                    &format!(
                                        "{} · {}",
                                        state.machine.family.label(),
                                        if state.machine.automated {
                                            "automatic"
                                        } else {
                                            "interactive"
                                        }
                                    ),
                                );
                                summary_row(
                                    ui,
                                    "Hardware",
                                    &format!(
                                        "{} cores · {} memory",
                                        state.machine.vcpus,
                                        format_bytes(state.machine.memory_mib * 1024 * 1024)
                                    ),
                                );
                                let disk = state.machine.disk_path(&settings.vm_dir);
                                summary_row(
                                    ui,
                                    "Storage",
                                    &format!(
                                        "{} · {}",
                                        disk.display(),
                                        if state.machine.disk_mode == launcher::DiskMode::CreateNew
                                        {
                                            format!(
                                                "new {} GiB sparse disk",
                                                state.machine.disk_gib
                                            )
                                        } else {
                                            "existing disk, preserved".to_string()
                                        }
                                    ),
                                );
                                if state.machine.family == launcher::GuestFamily::Ubuntu {
                                    summary_row(
                                        ui,
                                        "ISO",
                                        if state.machine.iso_path.trim().is_empty() {
                                            "Newest verified cached Ubuntu ISO"
                                        } else {
                                            state.machine.iso_path.trim()
                                        },
                                    );
                                }
                                if Backend::Wsl.available_on_host() {
                                    summary_row(ui, "Runs on", state.machine.backend.label());
                                }
                            });
                        let runner = launcher::Runner::new(
                            state.machine.backend,
                            std::path::PathBuf::from(&cli_label),
                            settings,
                        );
                        let preview = launcher::install_spec(
                            &runner,
                            &settings.vm_dir,
                            settings.child_cwd(),
                            &state.machine,
                        );
                        // A machine whose files the chosen backend cannot see
                        // is refused here, on the review step, with the fix —
                        // not after the button is pressed.
                        if let Err(message) = &preview {
                            ui.add_space(10.0);
                            ui.label(RichText::new(message).color(theme::ERR).size(12.5));
                        }
                        // …and neither is a backend whose engine is not there.
                        // The same refusal `ManagerApp::backend_block` gives the
                        // Start button, shown before the last click rather than
                        // after it, with the install offer attached.
                        if state.machine.backend == Backend::Wsl && wsl_engine.refusal().is_some() {
                            ui.add_space(6.0);
                            wsl_setup_note(ui, wsl_engine, wsl_install_running, 0.0, actions);
                        }
                    }
                }

                if let Some(error) = &state.error {
                    ui.add_space(10.0);
                    ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                }

                ui.add_space(24.0);
                theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                        actions.push(Action::CloseModal);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if state.step < 2 {
                            if ui::primary_button(ui, "Continue").clicked() {
                                state.step += 1;
                                state.error = None;
                            }
                        } else {
                            // The last step's button is the one the whole
                            // pre-flight protects: a machine that cannot start
                            // must not be creatable, and the reason must be
                            // readable without pressing it.
                            let blocked = (state.machine.backend == Backend::Wsl)
                                .then(|| wsl_engine.refusal())
                                .flatten();
                            let button = ui::primary_button_enabled(
                                ui,
                                "Create and install",
                                blocked.is_none(),
                            );
                            match &blocked {
                                Some(reason) => {
                                    button.on_hover_text(reason);
                                }
                                None if button.clicked() => actions.push(Action::SubmitWizard),
                                None => {}
                            }
                        }
                        if state.step > 0 && ui::secondary_button(ui, "Back", true).clicked() {
                            state.step -= 1;
                            state.error = None;
                        }
                        ui.label(ui::faint(format!("Step {} of 3", state.step + 1)));
                    });
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
                        if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                            actions.push(Action::CloseModal);
                        }
                    });
                },
            )
        }
        Modal::Settings(form) => frame(ctx, "settings", "Settings", 600.0, |ui| {
            settings_body(ui, form, engine, wsl_engine, wsl_install_running, actions)
        }),
        Modal::CreateDisk(state) => frame(ctx, "create-disk", "New disk", 470.0, |ui| {
            ui.label(ui::dim(
                "A sparse RAW image: it occupies almost no space until the guest writes to it.",
            ));
            ui.add_space(14.0);

            ui.label(ui::faint("NAME"));
            ui.add(
                egui::TextEdit::singleline(&mut state.name)
                    .desired_width(f32::INFINITY)
                    .hint_text("disk-1"),
            );
            ui.label(ui::faint(format!(
                "file {}",
                settings
                    .vm_dir
                    .join(format!("{}.raw", state.name.trim()))
                    .display()
            )));
            ui.add_space(12.0);

            ui.label(ui::faint("SIZE"));
            ui.add(
                egui::TextEdit::singleline(&mut state.size)
                    .desired_width(120.0)
                    .hint_text("32G"),
            );
            // Live validation through the same parser the CLI uses.
            match disk_image::parse_size(state.size.trim()) {
                Ok(bytes) => {
                    ui.label(ui::faint(format!(
                        "= {} ({bytes} bytes)",
                        format_bytes(bytes)
                    )));
                }
                Err(e) => {
                    ui.label(RichText::new(e.to_string()).color(theme::WARN).size(11.5));
                }
            }

            if let Some(error) = &state.error {
                ui.add_space(10.0);
                ui.label(RichText::new(error).color(theme::ERR).size(12.5));
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if ui::primary_button(ui, "Create disk").clicked() {
                    actions.push(Action::SubmitCreateDisk);
                }
                if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                    actions.push(Action::CloseModal);
                }
            });
        }),
        Modal::DeleteDisk(state) => frame(
            ctx,
            "delete-disk",
            &format!("Delete {}", state.row.file_name),
            470.0,
            |ui| {
                ui.label(
                    RichText::new("This removes the image file permanently.")
                        .color(theme::TEXT)
                        .size(13.0),
                );
                ui.add_space(12.0);
                ui.label(ui::faint(format!(
                    "− {}  ({})",
                    state.row.path.display(),
                    format_bytes(state.row.apparent_bytes)
                )));
                if state.row.nvram {
                    ui.label(ui::faint(format!(
                        "− {}  (UEFI variable store — deleted with the disk)",
                        disk_image::nvram_sidecar_path(&state.row.path).display()
                    )));
                }
                if !state.row.attachments.is_empty() {
                    ui.add_space(8.0);
                    let vms: Vec<&str> = state
                        .row
                        .attachments
                        .iter()
                        .map(|a| a.vm.as_str())
                        .collect();
                    ui.label(
                        RichText::new(format!(
                            "Still attached to {} — detach it there first",
                            vms.join(", ")
                        ))
                        .color(theme::WARN)
                        .size(12.5),
                    );
                }
                if let Some(error) = &state.error {
                    ui.add_space(8.0);
                    ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let deletable = state.row.attachments.is_empty();
                    if ui::ghost_button(ui, "Delete permanently", deletable, theme::ERR).clicked() {
                        actions.push(Action::ConfirmDeleteDisk);
                    }
                    if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                        actions.push(Action::CloseModal);
                    }
                });
            },
        ),
        Modal::AttachDisk(state) => frame(ctx, "attach-disk", "Attach disk", 470.0, |ui| {
            ui.label(ui::dim(format!(
                "Adds {} as a writable [[disk]] to a machine's profile.",
                state.disk.display()
            )));
            ui.add_space(12.0);

            let attached: Vec<&str> = scan
                .disks
                .iter()
                .find(|d| d.path == state.disk)
                .map(|d| d.attachments.iter().map(|a| a.vm.as_str()).collect())
                .unwrap_or_default();
            let candidates: Vec<&str> = scan
                .vms
                .iter()
                .filter(|vm| !attached.contains(&vm.name.as_str()) && !supervisor.is_busy(&vm.name))
                .map(|vm| vm.name.as_str())
                .collect();

            if candidates.is_empty() {
                ui.label(
                    RichText::new(
                        "No machine can take it: every machine is running, already \
                         attached, or none exists yet",
                    )
                    .color(theme::WARN)
                    .size(12.5),
                );
            } else {
                ui.horizontal(|ui| {
                    ui.label(ui::faint("MACHINE"));
                    ui.add_space(8.0);
                    egui::ComboBox::from_id_salt("attach-target")
                        .selected_text(state.selected.clone().unwrap_or_else(|| "—".into()))
                        .show_ui(ui, |ui| {
                            for name in &candidates {
                                ui.selectable_value(
                                    &mut state.selected,
                                    Some((*name).to_string()),
                                    *name,
                                );
                            }
                        });
                });
                ui.label(ui::faint(
                    "Running machines are not offered — stop them first",
                ));
            }

            if let Some(error) = &state.error {
                ui.add_space(8.0);
                ui.label(RichText::new(error).color(theme::ERR).size(12.5));
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let ready = state.selected.is_some() && !candidates.is_empty();
                if ui::ghost_button(ui, "Attach", ready, theme::TEXT).clicked() {
                    actions.push(Action::SubmitAttachDisk);
                }
                if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                    actions.push(Action::CloseModal);
                }
            });
        }),
        Modal::EditVm(state) => edit_vm_drawer(ctx, state, wsl_engine, actions),
        Modal::MoveDisk(state) => frame(
            ctx,
            "move-disk",
            &format!("Move {}", state.row.file_name),
            560.0,
            |ui| move_disk_body(ui, state, actions),
        ),
        Modal::RenameVm(state) => frame(
            ctx,
            "rename-vm",
            &format!("Rename {}", state.source),
            480.0,
            |ui| rename_vm_body(ui, state, actions),
        ),
        Modal::CloneVm(state) => frame(
            ctx,
            "clone-vm",
            &format!("Clone {}", state.source),
            520.0,
            |ui| clone_vm_body(ui, state, actions),
        ),
        Modal::ResizeDisk(state) => frame(
            ctx,
            "resize-disk",
            &format!("Grow {}", state.row.file_name),
            520.0,
            |ui| resize_disk_body(ui, state, actions),
        ),
        Modal::DeleteSnapshot(state) => frame(
            ctx,
            "delete-snapshot",
            "Forget this saved session?",
            520.0,
            |ui| delete_snapshot_body(ui, state, actions),
        ),
        Modal::DiscardSnapshot(state) => frame(
            ctx,
            "discard-snapshot",
            &format!("Start {} from scratch?", state.vm),
            520.0,
            |ui| discard_snapshot_body(ui, state, actions),
        ),
    };

    if closed {
        actions.push(Action::CloseModal);
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseModal);
    }
}

/// Settings.
///
/// What is *not* here any more is as important as what is: the engine path and
/// the working directory used to be two prominent text fields, and a GUI user
/// has no way of knowing what to put in either. The engine is now reported as
/// status — resolved, versioned, with a fix button when it is missing — and
/// both overrides live under Advanced, collapsed, where a developer can still
/// reach them.
fn settings_body(
    ui: &mut egui::Ui,
    form: &mut crate::app::SettingsForm,
    engine: &Result<crate::launcher::Engine, String>,
    wsl_engine: &wslengine::Status,
    wsl_install_running: bool,
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    // Entangled is one application: the part that runs machines is found by
    // itself and never shown — only when it is missing does the user hear
    // about it, as a problem with a fix.
    if engine.is_err() {
        engine_problem(ui, actions);
        ui.add_space(16.0);
    }

    if ui::path_row(
        ui,
        "MACHINES IN",
        "The folder your machines live in: their settings and their disks.",
        &mut form.vm_dir,
        "~/entangled-vms",
        true,
    ) {
        actions.push(Action::PickPath(PickTarget::VmDir));
    }
    ui.add_space(14.0);

    backend_row(ui, &mut form.default_backend, wsl_engine);
    if Backend::Wsl.available_on_host() && form.default_backend == Backend::Wsl {
        wsl_setup_note(
            ui,
            wsl_engine,
            wsl_install_running,
            ui::FORM_LABEL_W,
            actions,
        );
    }
    ui.add_space(14.0);

    ui.checkbox(&mut form.headless_install, "Install in the background")
        .on_hover_text(
            "The installer runs without a window of its own; follow it on the machine's \
             page.",
        );
    ui.checkbox(
        &mut form.check_updates_on_startup,
        "Check for updates on startup",
    )
    .on_hover_text("Asks once when Entangled opens. Being offline is fine.");
    ui.checkbox(&mut form.animations_enabled, "Interface motion")
        .on_hover_text("Hover fades and the running pulse. Off saves a little power.");

    ui.add_space(14.0);
    let advanced =
        egui::CollapsingHeader::new(RichText::new("Advanced").size(12.5).color(theme::TEXT_DIM))
            .id_salt("settings-advanced")
            .default_open(form.advanced_open);
    advanced.show(ui, |ui| {
        ui.label(ui::faint(
            "Entangled works all of these out for itself. Change them only for a build of \
             your own.",
        ));
        ui.add_space(10.0);
        if ui::path_row(
            ui,
            "CUSTOM BUILD",
            "Run machines with a build of Entangled from somewhere else — a source checkout, \
             for instance. Leave empty to use the one that came with this installation.",
            &mut form.entangled_binary,
            "the installed one",
            true,
        ) {
            actions.push(Action::PickPath(PickTarget::EngineBinary));
        }
        ui.add_space(8.0);
        if ui::path_row(
            ui,
            "WORKING DIR",
            "The folder machines start in. Only matters for a machine whose settings name \
             files by relative path.",
            &mut form.work_dir,
            "Entangled's own folder",
            true,
        ) {
            actions.push(Action::PickPath(PickTarget::WorkDir));
        }
        if Backend::Wsl.available_on_host() {
            ui.add_space(8.0);
            wsl_rows(ui, form);
        }
    });

    ui.add_space(20.0);
    ui.horizontal(|ui| {
        if ui::primary_button(ui, "Save").clicked() {
            actions.push(Action::SaveSettings);
        }
        if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

/// The one thing Settings says about the part of Entangled that runs
/// machines: that it is missing, and what to do about it.
fn engine_problem(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    egui::Frame::new()
        .fill(theme::mix(theme::MODAL, theme::ERR, 0.06))
        .stroke(egui::Stroke::new(
            1.0_f32,
            theme::mix(theme::STROKE, theme::ERR, 0.35),
        ))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(
                        "Part of Entangled is missing, so machines cannot start. Reinstalling \
                         Entangled Desktop fixes it.",
                    )
                    .size(13.0)
                    .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::text_button(ui, "Locate it…", theme::ACCENT)
                        .on_hover_text("Point Entangled at a copy you already have")
                        .clicked()
                    {
                        actions.push(Action::PickPath(PickTarget::EngineBinary));
                    }
                });
            });
        });
}

/// The where-does-this-run row, shared by Settings (the default) and the wizard
/// and editor (one machine). On a host with only one choice there is nothing
/// to ask, so the row is not shown at all.
fn backend_row(ui: &mut egui::Ui, value: &mut Backend, wsl_engine: &wslengine::Status) {
    let choices: Vec<Backend> = Backend::ALL
        .into_iter()
        .filter(|backend| backend.available_on_host())
        .collect();
    if choices.len() < 2 {
        return;
    }
    ui::form_row(
        ui,
        "RUNS ON",
        "Where machines run. Windows is the usual choice. WSL (the Windows Subsystem for \
         Linux) adds OpenGL 3D and bridged networking, after a one-time setup.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt(("backend-choice", ui.id()))
                .selected_text(entry_label(*value, wsl_engine))
                .width(ui::combo_width(field_w))
                .show_ui(ui, |ui| {
                    for backend in &choices {
                        ui.selectable_value(value, *backend, entry_label(*backend, wsl_engine))
                            .on_hover_text(backend.tooltip());
                    }
                });
        },
    );
}

/// The name of one choice in the picker, with whether it is ready yet. A
/// choice that needs setting up stays **selectable**: the setup lives one row
/// below, and a greyed-out entry would be a door locked from the inside.
fn entry_label(backend: Backend, wsl_engine: &wslengine::Status) -> String {
    match (backend, wsl_engine) {
        (Backend::Wsl, wslengine::Status::Failed(_)) => {
            format!("{} — needs a one-time setup", backend.label())
        }
        (Backend::Wsl, wslengine::Status::Checking) => format!("{} — checking…", backend.label()),
        _ => backend.label().to_string(),
    }
}

/// Whether WSL can run machines yet, in one plain sentence, and the button
/// that makes it able to. Before this, "your WSL has no `entangled`" was
/// discovered by `wsl.exe` forty minutes into a wizard and reported as
/// `execvpe entangled failed 2`; the fix is offered before anything starts.
fn wsl_setup_note(
    ui: &mut egui::Ui,
    status: &wslengine::Status,
    installing: bool,
    indent: f32,
    actions: &mut Vec<Action>,
) {
    let (text, color) = match status {
        wslengine::Status::Unknown => return,
        wslengine::Status::Checking => ("Checking WSL…".to_string(), theme::TEXT_DIM),
        wslengine::Status::Ready(_) => ("WSL is set up and ready.".to_string(), theme::OK),
        wslengine::Status::Failed(_) if installing => (
            "Setting up WSL — downloading and checking the files…".to_string(),
            theme::TEXT_DIM,
        ),
        wslengine::Status::Failed(_) if status.installable() => (
            "WSL needs a one-time setup: Entangled copies the files it needs into your WSL \
             system."
                .to_string(),
            theme::TEXT_DIM,
        ),
        // Something only the user can change — no WSL at all, no such system.
        // Its own sentence says what.
        wslengine::Status::Failed(fault) => (fault.what.clone(), theme::WARN),
    };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(indent);
        ui.add(egui::Label::new(RichText::new(text).size(12.5).color(color)).wrap());
    });
    if status.refusal().is_none() {
        return;
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(indent);
        if status.installable() {
            let block = wslengine::install_block();
            let enabled = !installing && block.is_none();
            let label = if installing {
                "Setting up…"
            } else {
                "Set up WSL"
            };
            let button = ui::secondary_button(ui, label, enabled);
            let button = match &block {
                Some(reason) => button.on_hover_text(reason),
                None => button.on_hover_text(
                    "Downloads the Linux part of this exact version, checks it against the \
                     fingerprint built into Entangled, and copies it into WSL.",
                ),
            };
            if enabled && button.clicked() {
                actions.push(Action::InstallWslEngine);
            }
        } else if let wslengine::Status::Failed(fault) = status {
            ui.label(
                RichText::new(&fault.fix)
                    .size(12.0)
                    .color(theme::TEXT_FAINT),
            );
        }
        if ui::text_button(ui, "Check again", theme::TEXT_DIM).clicked() && !installing {
            actions.push(Action::CheckWslEngine);
        }
    });
}

/// The two things WSL cannot work out for itself, under Advanced.
fn wsl_rows(ui: &mut egui::Ui, form: &mut crate::app::SettingsForm) {
    ui::form_row(
        ui,
        "WSL SYSTEM",
        "Which of your WSL systems runs machines — a name from `wsl --list`. Ubuntu \
         unless you have set up another.",
        |ui, field_w| {
            ui.add(
                egui::TextEdit::singleline(&mut form.wsl_distro)
                    .desired_width(field_w)
                    .hint_text(crate::backend::DEFAULT_WSL_DISTRO),
            );
        },
    );
    ui.add_space(6.0);
    ui::form_row(
        ui,
        "WSL BUILD",
        "A Linux build of Entangled of your own inside WSL, as a Linux path. Leave empty: \
         the setup above installs the matching one for you.",
        |ui, field_w| {
            ui.add(
                egui::TextEdit::singleline(&mut form.wsl_entangled)
                    .desired_width(field_w)
                    .hint_text("set up automatically"),
            );
        },
    );
}

/// The Edit-VM form body. The left-hand cards are intentionally navigation,
/// not separate save boundaries: changing one area keeps all unsaved edits and
/// Save validates and writes the complete profile.
/// The machine editor as a drawer from the right edge: the machine's page
/// stays visible behind it, dimmed, so the change being made sits beside the
/// thing it changes. One section at a time, chosen by the tabs along the top
/// (or by the setup tile that opened it); Save writes them all.
fn edit_vm_drawer(
    ctx: &egui::Context,
    state: &mut crate::app::EditVmState,
    wsl_engine: &wslengine::Status,
    actions: &mut Vec<Action>,
) -> bool {
    use crate::app::EditVmSection;

    let screen = ctx.screen_rect();
    let width = 620.0_f32.min(screen.width() - 40.0);
    let id = egui::Id::new("edit-vm");
    let area = egui::Modal::default_area(id).anchor(egui::Align2::RIGHT_TOP, Vec2::ZERO);
    let response = egui::Modal::new(id)
        .area(area)
        .backdrop_color(egui::Color32::from_black_alpha(150))
        .frame(
            egui::Frame::new()
                .fill(theme::MODAL)
                .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
                .shadow(egui::Shadow {
                    offset: [-24, 0],
                    blur: 60,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(110),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.set_height(screen.height());
            let sections = [
                (EditVmSection::Hardware, "Processor & memory"),
                (EditVmSection::DisplayGpu, "Display"),
                (EditVmSection::NetworkDevices, "Network & devices"),
                (EditVmSection::Storage, "Storage"),
                (EditVmSection::BootMedia, "Startup"),
            ];

            // Header: whose settings, which part, and a way out.
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: 28,
                    right: 20,
                    top: 22,
                    bottom: 0,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(ui::caption(&state.form.name));
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Machine settings")
                                    .size(20.0)
                                    .color(theme::TEXT)
                                    .strong(),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            if ui::icon_button(ui, ui::Icon::Close, "Close without saving")
                                .clicked()
                            {
                                actions.push(Action::CloseModal);
                            }
                        });
                    });
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 20.0;
                        for (section, label) in sections {
                            if ui::page_tab(ui, label, state.section == section).clicked() {
                                state.section = section;
                            }
                        }
                    });
                });
            theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());

            let preview = state.form.to_config();
            let savable = preview.is_ok() && state.form.dirty();
            egui::TopBottomPanel::bottom("edit-vm-footer")
                .frame(
                    egui::Frame::new()
                        .fill(theme::MODAL)
                        .inner_margin(egui::Margin::symmetric(28, 16)),
                )
                .show_inside(ui, |ui| {
                    if let Err(message) = &preview {
                        ui.label(RichText::new(message).color(theme::ERR).size(12.5));
                        ui.add_space(8.0);
                    } else if let Some(error) = &state.error {
                        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                        ui.add_space(8.0);
                    }
                    ui.horizontal(|ui| {
                        ui.label(ui::faint(format!(
                            "Applies when {} next starts",
                            state.form.name
                        )))
                        .on_hover_text("All sections are saved together");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let save = ui::primary_button_enabled(ui, "Save", savable)
                                .on_hover_text("Validates and saves every section");
                            if save.clicked() && savable {
                                actions.push(Action::SubmitEditVm);
                            }
                            if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                                actions.push(Action::CloseModal);
                            }
                        });
                    });
                });

            egui::CentralPanel::default()
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 22)))
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            match state.section {
                                EditVmSection::Hardware => {
                                    edit_hardware(ui, &mut state.form, wsl_engine)
                                }
                                EditVmSection::BootMedia => {
                                    edit_boot_media(ui, &mut state.form, actions);
                                }
                                EditVmSection::DisplayGpu => edit_display_gpu(ui, &mut state.form),
                                EditVmSection::NetworkDevices => {
                                    edit_network_devices(ui, &mut state.form);
                                }
                                EditVmSection::Storage => {
                                    edit_storage(ui, &mut state.form, actions)
                                }
                            }
                        });
                });
        });
    response.should_close()
}

/// Every editor section opens with this, which is also where the section's
/// shared row width is pinned — so no section can forget to do it.
fn edit_panel_heading(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui::form_scope(ui);
    ui.label(RichText::new(title).size(19.0).color(theme::TEXT).strong());
    ui.add_space(2.0);
    ui.label(RichText::new(detail).size(12.5).color(theme::TEXT_DIM));
    ui.add_space(18.0);
}

fn edit_hardware(
    ui: &mut egui::Ui,
    form: &mut crate::editor::EditForm,
    wsl_engine: &wslengine::Status,
) {
    use control_api::VirtioTransport;

    edit_panel_heading(
        ui,
        "Processor & memory",
        "How much of this computer the machine may use while it runs.",
    );
    ui.horizontal(|ui| {
        let gap = ui.spacing().item_spacing.x;
        let card_width = ((ui.available_width() - 2.0 * gap) / 3.0).floor();
        for preset in POWER_PRESETS {
            let selected = form.vcpus == preset.vcpus && form.memory_mib == preset.memory_mib;
            if option_card(
                ui,
                card_width,
                preset.name,
                &format!(
                    "{} cores · {} GiB · {}",
                    preset.vcpus,
                    preset.memory_mib / 1024,
                    preset.hint
                ),
                selected,
                true,
                theme::ACCENT,
            )
            .clicked()
            {
                form.vcpus = preset.vcpus;
                form.memory_mib = preset.memory_mib;
            }
        }
    });
    ui.add_space(18.0);
    ui::form_row(
        ui,
        "MEMORY",
        "How much of this computer's RAM the machine may use while it runs. It is taken \
         from the host only while the machine is on.",
        |ui, field_w| {
            ui.spacing_mut().slider_width = field_w - 72.0;
            ui.add(
                egui::Slider::new(&mut form.memory_mib, 128..=control_api::MAX_MEMORY_MIB)
                    .step_by(128.0)
                    .suffix(" MiB"),
            );
        },
    );
    ui::form_row(ui, "", "", |ui, _| {
        ui.checkbox(
            &mut form.reclaim_memory,
            "Give memory the guest frees back to this computer",
        )
        .on_hover_text(
            "Without it, memory the guest has used stays taken on this computer until the              machine stops, even after the guest frees it. With it, Linux guests report free              memory (in 2 MiB blocks, a moment after it is freed) and Entangled hands it back.              Costs one of the machine's eight device slots.",
        );
    });
    ui.add_space(6.0);
    ui::form_row(
        ui,
        "PROCESSORS",
        "How many processor cores the guest sees. More than this computer physically has \
         only makes the guest slower.",
        |ui, field_w| {
            ui.spacing_mut().slider_width = field_w - 72.0;
            ui.add(egui::Slider::new(&mut form.vcpus, 1..=64).suffix(" vCPU"));
        },
    );
    ui.add_space(16.0);
    backend_row(ui, &mut form.backend, wsl_engine);
    ui.add_space(12.0);
    ui::form_row(
        ui,
        "DEVICES ON",
        "How the guest sees its virtual disks, network and screen. PCI is what an ordinary \
         PC looks like and is the right answer unless you know otherwise; MMIO is a leaner \
         wiring used by minimal direct-boot setups.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt("edit-transport")
                .selected_text(form.transport.to_string())
                .width(ui::combo_width(field_w))
                .show_ui(ui, |ui| {
                    for transport in [VirtioTransport::Mmio, VirtioTransport::Pci] {
                        ui.selectable_value(&mut form.transport, transport, transport.to_string());
                    }
                });
        },
    );
}

/// Boot & media.
///
/// This is the section the deliverable is about. `firmware` and `nvram` used to
/// be two bare text boxes holding `artifacts/firmware/CLOUDHV.fd` and
/// `ubuntu.nvram` — file names that mean nothing to anyone who has not read the
/// source. They are now labelled in product language, explained on hover,
/// filled in for you from the machine's own disk, badged with whether the file
/// is actually there, and browsable.
fn edit_boot_media(
    ui: &mut egui::Ui,
    form: &mut crate::editor::EditForm,
    actions: &mut Vec<Action>,
) {
    use control_api::BootMode;

    edit_panel_heading(
        ui,
        "Boot & media",
        "How this machine starts, and what it can boot from.",
    );
    ui::form_row(
        ui,
        "STARTS VIA",
        "UEFI firmware behaves like a real PC: the machine has its own boot menu and can \
         boot an installer disc. Direct Linux skips all of that and hands a Linux kernel \
         straight to the machine — faster, but it can only boot that one kernel.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt("edit-boot-mode")
                .selected_text(match form.boot_mode {
                    BootMode::DirectLinux => "Direct Linux",
                    BootMode::Uefi => "UEFI firmware",
                })
                .width(ui::combo_width(field_w))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.boot_mode, BootMode::DirectLinux, "Direct Linux")
                        .on_hover_text("Boots one named Linux kernel directly. No boot menu.");
                    ui.selectable_value(&mut form.boot_mode, BootMode::Uefi, "UEFI firmware")
                        .on_hover_text("Behaves like a PC: firmware, boot entries, discs.");
                });
        },
    );
    ui.add_space(14.0);

    match form.boot_mode {
        BootMode::DirectLinux => {
            if ui::path_row(
                ui,
                "KERNEL",
                "The Linux kernel image this machine boots. With no firmware in the way, \
                 this file *is* the boot process.",
                &mut form.kernel,
                launcher::BOOTSTRAP_KERNEL,
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorKernel));
            }
            if !form.kernel.trim().is_empty() {
                ui::path_status_note(ui, &form.resolve(&form.kernel.clone()), "");
            }
            ui.add_space(6.0);
            if ui::path_row(
                ui,
                "INITRAMFS",
                "A small temporary filesystem the kernel unpacks before it can reach the \
                 real disk. Optional — many kernels do not need one.",
                &mut form.initramfs,
                "optional",
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorInitramfs));
            }
            ui.add_space(6.0);
            ui::form_row(
                ui,
                "BOOT OPTIONS",
                "The command line handed to the kernel: which disk holds the system, where \
                 the console goes, and so on.",
                |ui, field_w| {
                    ui.add(
                        egui::TextEdit::singleline(&mut form.cmdline)
                            .desired_width(field_w)
                            .hint_text("console=ttyS0 root=…"),
                    );
                },
            );
        }
        BootMode::Uefi => {
            // Firmware ------------------------------------------------------
            if ui::path_row(
                ui,
                "FIRMWARE",
                "The UEFI firmware this machine boots — the equivalent of a PC's BIOS. It \
                 is the same file for every machine and ships with Entangled.",
                &mut form.firmware,
                launcher::UEFI_FIRMWARE,
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorFirmware));
            }
            let firmware = form.firmware.trim().to_string();
            if firmware.is_empty() {
                ui::form_note(
                    ui,
                    RichText::new("A UEFI machine cannot start without firmware.")
                        .color(theme::WARN)
                        .size(11.0),
                );
                let suggested = form.suggested_firmware();
                if fix_button(ui, &format!("Use {}", suggested.display())) {
                    form.firmware = suggested.display().to_string();
                }
            } else {
                let resolved = form.resolve(&firmware);
                // A profile carries the firmware path of the computer that
                // created it, so on a fresh installation it often names
                // nothing — and Entangled falls through to the copy this
                // computer has rather than refusing (apps/entangled/src/firmware.rs).
                // Saying "not found" and stopping there would describe a
                // failure that will not happen.
                match (
                    resolved.is_file(),
                    launcher::locate_firmware(&form.work_dir),
                ) {
                    (false, Some((found, origin))) => {
                        ui::form_note(
                            ui,
                            RichText::new(format!("not found: {}", resolved.display()))
                                .color(theme::WARN)
                                .size(11.0),
                        );
                        ui::form_note(
                            ui,
                            RichText::new(format!(
                                "this machine will start on the firmware from {}: {}",
                                origin.label(),
                                found.display()
                            ))
                            .color(theme::TEXT_FAINT)
                            .size(11.0),
                        );
                    }
                    _ => ui::path_status_note(ui, &resolved, launcher::FIRMWARE_FIX),
                }
            }
            ui.add_space(8.0);

            // NVRAM ---------------------------------------------------------
            if ui::path_row(
                ui,
                "UEFI SETTINGS",
                "This machine's own UEFI settings and boot entries — what a real PC keeps \
                 in a chip on the board. It belongs to this machine: it is copied when the \
                 disk is moved and deleted when the machine is deleted. Without it the \
                 machine forgets how to boot after an update.",
                &mut form.nvram,
                "kept beside the disk",
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorNvram));
            }
            let nvram = form.nvram.trim().to_string();
            if nvram.is_empty() {
                let suggested = form.suggested_nvram();
                ui::form_note(
                    ui,
                    RichText::new(
                        "Not set: this machine will forget its boot entries between runs.",
                    )
                    .color(theme::WARN)
                    .size(11.0),
                );
                if fix_button(ui, &format!("Keep it at {}", suggested.display())) {
                    form.nvram = suggested.display().to_string();
                }
            } else {
                ui::path_status_note(
                    ui,
                    &form.resolve(&nvram),
                    "It is created the first time this machine starts.",
                );
            }
            ui.add_space(8.0);

            // Installer disc -------------------------------------------------
            if ui::path_row(
                ui,
                "DISC",
                "An installer disc image (.iso) plugged into this machine's virtual optical \
                 drive. Only a UEFI machine can boot one — Direct Linux has no firmware to \
                 offer it.",
                &mut form.cdrom,
                "no disc inserted",
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorCdrom));
            }
            if !form.cdrom.trim().is_empty() {
                ui::path_status_note(ui, &form.resolve(&form.cdrom.clone()), "");
            }
        }
    }
}

/// A small right-aligned action under a form row: "we know what this should be,
/// press here and it is filled in". Returns true on click.
fn fix_button(ui: &mut egui::Ui, label: &str) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_space(ui::FORM_LABEL_W + ui.spacing().item_spacing.x);
        clicked = ui::ghost_button(ui, label, true, theme::TEXT)
            .on_hover_text("Fills the field in with the value Entangled would use")
            .clicked();
    });
    clicked
}

/// Network & display — and the two capability gates that used to be discovered
/// at boot: TAP networking and 3D. Both are decided by the machine's backend,
/// so both are answerable here, before anything is launched.
fn edit_display_gpu(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
    edit_panel_heading(
        ui,
        "Display & GPU",
        "The guest's virtual monitor, and how much of this computer's graphics card it may use.",
    );
    ui::form_row(
        ui,
        "SCREEN",
        "The size of the machine's virtual screen, in pixels. The window can be resized \
         freely; this is what the guest itself believes it has.",
        |ui, _| {
            ui.add(egui::DragValue::new(&mut form.display_width).range(320..=7680));
            ui.label(ui::faint("×"));
            ui.add(egui::DragValue::new(&mut form.display_height).range(200..=4320));
        },
    );
    ui::form_row(ui, "", "", |ui, _| {
        ui.checkbox(&mut form.follow_window, "Follow the window's size")
            .on_hover_text(
                "Resize, maximise or go fullscreen and the guest's desktop becomes \
                     exactly the window's size, pixel for pixel. Off keeps the size above \
                     whatever the window does, and scales the image instead.",
            );
    });
    ui.add_space(6.0);
    ui::form_row(
        ui,
        "TYPING",
        "The keyboard layout set inside the guest, so \"type the clipboard\" (Ctrl+Alt+V) \
         can type its letters. Polish (programmers) adds ą ć ę ł ń ó ś ź ż as AltGr + the \
         letter.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt("typing-layout")
                .width(ui::combo_width(field_w))
                .selected_text(match form.typing_layout.as_str() {
                    "pl" => "Polish (programmers)",
                    _ => "US",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.typing_layout, String::new(), "US");
                    ui.selectable_value(
                        &mut form.typing_layout,
                        "pl".to_string(),
                        "Polish (programmers)",
                    );
                });
        },
    );
    ui.add_space(6.0);
    ui::form_row(
        ui,
        "REFRESH",
        "How often the virtual monitor says it refreshes. The guest's desktop draws in          step with it, so a frame that misses one refresh waits for the next: a higher          rate makes animation smoother and lowers latency. \"Match monitor\" picks this          computer's monitor rate — or, above 144 Hz, the largest even fraction of it — the          same rule a new machine gets.",
        |ui, _| {
            ui.add(
                egui::DragValue::new(&mut form.refresh_hz)
                    .range(control_api::MIN_REFRESH_HZ..=control_api::MAX_REFRESH_HZ)
                    .suffix(" Hz"),
            );
            ui.add_space(6.0);
            let monitor = form.monitor_refresh_hz;
            if ui::ghost_button(
                ui,
                &format!("Match monitor ({monitor} Hz)"),
                form.refresh_hz != monitor,
                theme::ACCENT,
            )
            .on_hover_text(
                "This computer's primary monitor, snapped to a standard rate and held to                  what the GPU desktop keeps up with.",
            )
            .clicked()
            {
                form.refresh_hz = monitor;
            }
        },
    );
    ui.add_space(6.0);

    let virgl_block = form.backend.gpu_block(GpuRenderer::Virgl);
    if form.gpu == GpuRenderer::Virgl && virgl_block.is_some() {
        // Unavailable here — and never silently left on, because the engine
        // refuses to boot a profile that asks for 3D it cannot deliver.
        form.gpu = GpuRenderer::TwoD;
    }
    ui::form_row(
        ui,
        "3D",
        "Hardware-accelerated 3D inside the guest, using this computer's graphics card. \
         Without it the guest still has a desktop, drawn by its own processor.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt("edit-gpu")
                .selected_text(gpu_label(form.gpu))
                .width(ui::combo_width(field_w))
                .show_ui(ui, |ui| {
                    for choice in GpuRenderer::ALL {
                        let block = form.backend.gpu_block(choice);
                        let response = ui
                            .add_enabled_ui(block.is_none(), |ui| {
                                ui.selectable_value(&mut form.gpu, choice, gpu_label(choice))
                            })
                            .inner;
                        match block {
                            Some(reason) => response.on_disabled_hover_text(reason.long),
                            None => response.on_hover_text(gpu_hint(choice)),
                        };
                    }
                })
                .response
                .on_hover_text(
                    "If the host cannot provide the renderer chosen here, the machine \
                     refuses to start rather than quietly falling back to software \
                     rendering. `entangled doctor` (Diagnostics) says what this host has.",
                );
        },
    );
    if form.gpu == GpuRenderer::Venus {
        ui::form_note(
            ui,
            RichText::new(
                "The guest must be set up for it: `entangled install ubuntu --venus` does \
                 that; see the user guide's \"A GPU desktop\".",
            )
            .color(theme::WARN)
            .size(11.0),
        );
    }
    if form.gpu == GpuRenderer::Venus {
        ui.add_space(12.0);
        ui::form_row(
            ui,
            "GPU BOOST",
            "Asks the graphics driver to run the card at full clocks while the guest is              drawing, and lets it settle a second after it stops. Makes the guest's 3D              noticeably faster on a 60 Hz desktop and costs nothing while it is idle;              turn it off to save power. NVIDIA only — other cards ignore it.",
            |ui, _| {
                ui.checkbox(&mut form.gpu_boost, "Full GPU clocks while the guest draws");
            },
        );
        ui.add_space(6.0);
        ui::form_row(
            ui,
            "VIDEO MEMORY",
            "The most of this computer's video memory the guest's programs may take              together. Unlimited, they get three quarters of it and your own desktop              keeps the rest; a cap protects the host from a guest that runs away with              memory. Never more than the card has, whatever it says.",
            |ui, _| {
                ui.checkbox(&mut form.gpu_memory_limit, "Cap at");
                ui.add_enabled(
                    form.gpu_memory_limit,
                    egui::DragValue::new(&mut form.gpu_memory_mib)
                        .range(control_api::MIN_GPU_MEMORY_MIB..=control_api::MAX_GPU_MEMORY_MIB)
                        .speed(64.0)
                        .suffix(" MiB"),
                );
            },
        );
        if form.gpu_memory_limit && form.gpu_memory_mib >= 1024 {
            ui::form_note(
                ui,
                ui::faint(format!(
                    "= {:.1} GiB",
                    f64::from(form.gpu_memory_mib) / 1024.0
                )),
            );
        }
    }
}

fn edit_network_devices(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
    use crate::editor::NetworkChoice;
    edit_panel_heading(
        ui,
        "Network & devices",
        "Connect the guest, and give it a sound card and gamepads.",
    );

    let tap_block = form.backend.tap_block();
    ui::form_row(
        ui,
        "NETWORK",
        "How the machine reaches the outside world. \"usernet\" needs no setup at all and \
         works everywhere. \"tap\" gives the machine its own address on your network but \
         needs a host interface, which only Linux has.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt("edit-network")
                .selected_text(form.network.label())
                .width(ui::combo_width(field_w))
                .show_ui(ui, |ui| {
                    for choice in NetworkChoice::ALL {
                        let blocked = choice == NetworkChoice::Tap && tap_block.is_some();
                        let response = ui
                            .add_enabled_ui(!blocked, |ui| {
                                ui.selectable_value(&mut form.network, choice, choice.label())
                            })
                            .inner;
                        if let Some(reason) = tap_block.filter(|_| blocked) {
                            response.on_hover_text(reason.long);
                        }
                    }
                });
        },
    );
    // A profile carried over from a Linux host can still *say* tap; the reason
    // has to be visible, not just the option greyed out.
    if form.network == NetworkChoice::Tap {
        if let Some(reason) = tap_block {
            ui::form_note(
                ui,
                RichText::new(reason.short).color(theme::WARN).size(11.0),
            );
        }
        ui.add_space(6.0);
        ui::form_row(
            ui,
            "INTERFACE",
            "The name of the host network interface to attach to, created once by \
             scripts/setup-tap.sh.",
            |ui, field_w| {
                ui.add(
                    egui::TextEdit::singleline(&mut form.interface)
                        .desired_width(field_w)
                        .hint_text("entangled0"),
                );
            },
        );
    }
    if form.network != NetworkChoice::None {
        ui.add_space(6.0);
        ui::form_row(
            ui,
            "MAC ADDRESS",
            "The hardware address the guest sees. Leave it empty and Entangled derives a \
             stable one from the machine's name.",
            |ui, field_w| {
                ui.add(
                    egui::TextEdit::singleline(&mut form.mac)
                        .desired_width(field_w)
                        .hint_text("derived from the name"),
                );
            },
        );
    }
    if form.network == NetworkChoice::Usernet {
        ui.add_space(6.0);
        let mut remove = None;
        for (i, rule) in form.forwards.iter_mut().enumerate() {
            let (label, tip) = if i == 0 {
                ("PORT FORWARDS", FORWARD_TIP)
            } else {
                ("", "")
            };
            ui::form_row(ui, label, tip, |ui, field_w| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(rule)
                            .desired_width(field_w - 84.0)
                            .hint_text("host port:guest port  (udp: for UDP)"),
                    );
                    if ui::ghost_button(ui, "Remove", true, theme::TEXT_DIM).clicked() {
                        remove = Some(i);
                    }
                });
            });
            if !rule.trim().is_empty() {
                match control_api::PortForwardRule::parse(rule) {
                    Ok(parsed) => ui::form_note(
                        ui,
                        ui::faint(format!(
                            "{} on this computer reaches port {} in the guest",
                            parsed.host_addr(),
                            parsed.guest_port
                        )),
                    ),
                    Err(error) => {
                        ui::form_note(ui, RichText::new(error).color(theme::WARN).size(11.0))
                    }
                }
            }
        }
        if let Some(i) = remove {
            form.forwards.remove(i);
        }
        let (label, tip) = if form.forwards.is_empty() {
            ("PORT FORWARDS", FORWARD_TIP)
        } else {
            ("", "")
        };
        ui::form_row(ui, label, tip, |ui, _| {
            let full = form.forwards.len() >= control_api::MAX_PORT_FORWARDS;
            if ui::ghost_button(ui, "+ Add a forward", !full, theme::TEXT).clicked() {
                form.forwards.push(String::new());
            }
        });
    }
    ui.add_space(18.0);
    ui::form_row(
        ui,
        "SOUND",
        "Gives the machine a sound card, so the guest can play audio out through this \
         computer's speakers. Off by default: an existing machine keeps exactly the \
         hardware it had.",
        |ui, _| {
            ui.checkbox(&mut form.sound, "Give this machine a sound card")
                .on_hover_text(
                    "A standard virtio sound card. The guest needs no drivers from us — \
                     every current Linux desktop already has one.",
                );
        },
    );
    if form.sound {
        // Only this engine's own backend is offered: `alsa` on a Windows engine
        // (or `wasapi` on a Linux one) is not a degraded mode, it is a machine
        // that refuses to start.
        let sound_block = form.backend.sound_backend_block(form.sound_backend);
        ui.add_space(6.0);
        ui::form_row(
            ui,
            "PLAYS INTO",
            "Which of this computer's audio systems the card plays into. Leave it on \
             \"auto\" unless you have a reason: auto uses whatever the host has and never \
             prevents a machine from starting.",
            |ui, field_w| {
                egui::ComboBox::from_id_salt("edit-sound-backend")
                    .selected_text(form.sound_backend.to_string())
                    .width(ui::combo_width(field_w))
                    .show_ui(ui, |ui| {
                        for choice in form.backend.sound_backends() {
                            ui.selectable_value(
                                &mut form.sound_backend,
                                choice,
                                choice.to_string(),
                            )
                            .on_hover_text(sound_backend_hint(choice));
                        }
                    });
            },
        );
        if let Some(reason) = sound_block {
            ui::form_note(
                ui,
                RichText::new(reason.short).color(theme::WARN).size(11.0),
            );
        }
    }

    ui.add_space(12.0);
    ui::form_row(
        ui,
        "GAMEPAD",
        "Gives the machine a game controller, and passes one plugged into this computer \
         through to it. Off by default: an existing machine keeps exactly the hardware it \
         had, and a guest that gains a joystick can change what its games do.",
        |ui, _| {
            ui.checkbox(&mut form.gamepad, "Give this machine gamepads")
                .on_hover_text(
                    "Xbox-shaped controllers the guest recognises with no drivers from \
                     us. Entangled reads whichever pads this computer has — XInput on \
                     Windows, evdev on Linux — and the guest sees them either way, even \
                     with nothing plugged in yet, so a pad can be connected while the \
                     machine is running.",
                );
        },
    );
    if form.gamepad {
        ui.add_space(6.0);
        ui::form_row(
            ui,
            "PLAYERS",
            "How many controllers the guest sees. Each one is a separate device inside \
             the machine, and a machine has room for eight devices in total — a disk, \
             the screen, the keyboard, the pointer, the network and the sound card \
             already use six, so two players fit but a second disk then does not. \
             Player 1 is the pad the guest calls js0.",
            |ui, field_w| {
                egui::ComboBox::from_id_salt("edit-gamepad-players")
                    .selected_text(players_label(form.gamepad_players))
                    .width(ui::combo_width(field_w))
                    .show_ui(ui, |ui| {
                        for players in 1..=control_api::MAX_GAMEPAD_PLAYERS {
                            ui.selectable_value(
                                &mut form.gamepad_players,
                                players,
                                players_label(players),
                            )
                            .on_hover_text(players_hint(players));
                        }
                    });
            },
        );
    }
}

/// The explanation on the port-forward rows.
const FORWARD_TIP: &str = "Reach a service inside the machine from this computer. \"2222:22\" \
     makes port 2222 here reach the guest's SSH on port 22; it listens on 127.0.0.1, so only \
     this computer can use it. Put an address in front (\"0.0.0.0:8080:80\") to accept \
     connections from your network too, and \"udp:\" in front (\"udp:27015:27015\") for a      game server or anything else on UDP. Changes apply the next time the machine starts.";

/// The picker's own text: "1 player", "2 players".
fn players_label(players: u8) -> String {
    match players {
        1 => "1 player".to_string(),
        n => format!("{n} players"),
    }
}

/// One sentence per player count, priced in device slots — the currency the
/// choice is actually spent in.
fn players_hint(players: u8) -> &'static str {
    match players {
        1 => {
            "One controller. Leaves room for a second disk or a CD-ROM alongside the \
             network and sound cards."
        }
        2 => {
            "Two controllers, on their own devices, told apart by the guest. Fits beside \
             the network and sound cards, but nothing else."
        }
        3 => "Three controllers. Needs the sound card or the network turned off.",
        _ => {
            "Four controllers, which is as many as Windows itself tracks. Needs both the \
             sound card and the network turned off."
        }
    }
}

/// One sentence per sound backend, for the hover in the picker.
fn sound_backend_hint(backend: control_api::SoundBackend) -> &'static str {
    use control_api::SoundBackend;
    match backend {
        SoundBackend::Auto => {
            "Whatever this computer has, and silence if it has nothing. Never prevents a \
             machine from starting."
        }
        SoundBackend::Null => {
            "A card that plays into nothing. The guest still sees and uses a working sound \
             device — useful for a machine that should stay quiet."
        }
        SoundBackend::Alsa => {
            "Linux's own audio system, opened directly. Says so and stops if this computer \
             has no ALSA."
        }
        SoundBackend::Wasapi => {
            "Windows' own audio system, shared with everything else that is playing. Says \
             so and stops if this computer has no playback device."
        }
    }
}

fn edit_storage(ui: &mut egui::Ui, form: &mut crate::editor::EditForm, actions: &mut Vec<Action>) {
    edit_panel_heading(
        ui,
        "Storage",
        "Disks are exposed in this order as /dev/vda, /dev/vdb, and so on.",
    );
    egui::ScrollArea::vertical()
        .id_salt("edit-storage-list")
        .max_height(176.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let mut remove = None;
            for (index, disk) in form.disks.iter_mut().enumerate() {
                egui::Frame::new()
                    .fill(theme::INSET)
                    .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
                    .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                    .inner_margin(egui::Margin::symmetric(11, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui::chip(
                                ui,
                                &format!("vd{}", (b'a' + index as u8) as char),
                                theme::ACCENT,
                            );
                            let path = disk.path.display().to_string();
                            ui.label(RichText::new(&path).color(theme::TEXT).size(12.0))
                                .on_hover_text(path);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui::ghost_button(ui, "Remove", true, theme::WARN).clicked() {
                                    remove = Some(index);
                                }
                                ui.checkbox(&mut disk.writable, "Writable");
                            });
                        });
                    });
                ui.add_space(7.0);
            }
            if let Some(index) = remove {
                form.disks.remove(index);
            }
        });
    ui.add_space(12.0);
    if ui::path_row(
        ui,
        "ADD A DISK",
        "Attaches an existing disk image to this machine as another drive. The guest sees \
         it as /dev/vdb, /dev/vdc and so on, in the order listed above.",
        &mut form.add_disk,
        "choose an image…",
        true,
    ) {
        actions.push(Action::PickPath(PickTarget::EditorAddDisk));
    }
    let path = form.add_disk.trim().to_string();
    ui.horizontal(|ui| {
        ui.add_space(ui::FORM_LABEL_W + ui.spacing().item_spacing.x);
        if ui::ghost_button(ui, "Attach it", !path.is_empty(), theme::TEXT).clicked() {
            form.disks.push(control_api::DiskSection {
                path: std::path::PathBuf::from(path),
                writable: true,
            });
            form.add_disk.clear();
        }
    });

    ui.add_space(18.0);
    let mut remove = None;
    for (index, share) in form.shares.iter_mut().enumerate() {
        let (label, tip) = if index == 0 {
            ("SHARED FOLDERS", SHARE_TIP)
        } else {
            ("", "")
        };
        ui::form_row(ui, label, tip, |ui, field_w| {
            egui::Frame::new()
                .fill(theme::INSET)
                .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
                .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                .inner_margin(egui::Margin::symmetric(9, 6))
                .show(ui, |ui| {
                    ui.set_width(field_w - 18.0);
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut share.tag)
                                .desired_width(96.0)
                                .hint_text("tag"),
                        )
                        .on_hover_text("The name the guest mounts it by");
                        let path = share.path.display().to_string();
                        ui.label(
                            RichText::new(shorten_middle(&path, 34))
                                .color(theme::TEXT)
                                .size(12.0),
                        )
                        .on_hover_text(path);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui::ghost_button(ui, "Remove", true, theme::WARN).clicked() {
                                remove = Some(index);
                            }
                            ui.checkbox(&mut share.read_only, "Read-only");
                        });
                    });
                });
        });
        if !control_api::valid_share_tag(&share.tag) {
            ui::form_note(
                ui,
                RichText::new("a tag is letters, digits, '-', '_' or '.', up to 31 of them")
                    .color(theme::WARN)
                    .size(11.0),
            );
        } else {
            ui::form_note(
                ui,
                ui::faint(format!(
                    "in the guest: sudo mount -t 9p -o trans=virtio {} /mnt/{}",
                    share.tag, share.tag
                )),
            );
        }
    }
    if let Some(index) = remove {
        form.shares.remove(index);
    }
    let full = form.shares.len() >= control_api::MAX_SHARES;
    if ui::path_row(
        ui,
        if form.shares.is_empty() {
            "SHARED FOLDERS"
        } else {
            ""
        },
        SHARE_TIP,
        &mut form.add_share,
        "choose a folder…",
        !full,
    ) {
        actions.push(Action::PickPath(PickTarget::EditorAddShare));
    }
    let path = form.add_share.trim().to_string();
    ui.horizontal(|ui| {
        ui.add_space(ui::FORM_LABEL_W + ui.spacing().item_spacing.x);
        if ui::ghost_button(ui, "Share it", !path.is_empty() && !full, theme::TEXT).clicked() {
            let path = std::path::PathBuf::from(path);
            // A tag of its own: the folder's name, numbered if taken.
            let base = control_api::share_tag_for(&path);
            let mut tag = base.clone();
            let mut n = 2;
            while form.shares.iter().any(|s| s.tag == tag) {
                tag = format!("{}-{n}", &base[..base.len().min(28)]);
                n += 1;
            }
            form.shares.push(control_api::ShareSection {
                path,
                tag,
                read_only: false,
            });
            form.add_share.clear();
        }
    });
}

/// The explanation on the shared-folder rows.
const SHARE_TIP: &str = "A folder on this computer the machine can open as a drive of \
     its own. Each one is mounted inside the guest by its tag, for example \
     `sudo mount -t 9p -o trans=virtio <tag> /mnt/<tag>` (or a line in /etc/fstab). \
     Read-only lets the guest look but never change anything. Files appear to belong to \
     whoever looks at them in the guest; what they can really do is what this computer \
     lets Entangled do.";

/// `C:\Users\…\Projects` — the start and the end of a long path.
fn shorten_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head = max / 3;
    let tail = max - head - 1;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// The move-to-another-drive dialog: destination, the space arithmetic
/// (allocated data vs worst-case apparent size vs free space), a gradient
/// progress bar while the worker copies, and the safety story spelled out.
fn move_disk_body(
    ui: &mut egui::Ui,
    state: &mut crate::app::MoveDiskState,
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    ui.label(ui::dim(
        "Copies the image sparse-preserving (only allocated data travels), verifies the \
         copy against the source, updates every referencing profile, and only then \
         deletes the original. A failure at any step leaves everything as it was.",
    ));
    ui.add_space(12.0);

    ui.label(ui::faint(format!("− {}", state.row.path.display())));
    if state.row.nvram {
        ui.label(ui::faint(format!(
            "− {}  (moves along)",
            disk_image::nvram_sidecar_path(&state.row.path).display()
        )));
    }
    for attachment in &state.row.attachments {
        ui.label(ui::faint(format!(
            "profile of '{}' will be updated",
            attachment.vm
        )));
    }
    ui.add_space(12.0);

    if ui::path_row(
        ui,
        "MOVE TO",
        "The folder the image is copied into. Any machine that uses this disk is updated \
         to point at the new location.",
        &mut state.dest,
        if cfg!(windows) {
            "E:\\vm-storage"
        } else {
            "/mnt/bigdrive/vms"
        },
        !state.running,
    ) {
        actions.push(Action::PickPath(PickTarget::MoveDestination));
    }

    // The space arithmetic, live: what the copy writes now (allocated data)
    // and what the guest may grow into later (apparent size).
    let bill = disk_image::relocate::copy_bill(&state.row.path);
    let dest = state.dest.trim();
    let space = (!dest.is_empty())
        .then(|| disk_image::disk_space(std::path::Path::new(dest)))
        .flatten();
    if let Some((data, apparent)) = bill {
        ui.add_space(4.0);
        ui.label(ui::faint(format!(
            "copies {} of data · image can grow to {}",
            format_bytes(data),
            format_bytes(apparent)
        )));
        if let Some((free, _total)) = space {
            if free < data {
                ui.label(
                    RichText::new(format!(
                        "{} free at the destination — not enough for the data itself",
                        format_bytes(free)
                    ))
                    .color(theme::ERR)
                    .size(12.5),
                );
            } else if free < apparent {
                ui.label(
                    RichText::new(format!(
                        "{} free at the destination — enough for the data, but less than \
                         the image's full {} (the guest can outgrow the drive later)",
                        format_bytes(free),
                        format_bytes(apparent)
                    ))
                    .color(theme::WARN)
                    .size(12.5),
                );
            } else {
                ui.label(ui::faint(format!(
                    "{} free at the destination",
                    format_bytes(free)
                )));
            }
        }
    }

    // Progress: a gradient fill over the inset track, plus the byte counter.
    if let Some((done, total)) = state.progress {
        ui.add_space(12.0);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius::same(7), theme::INSET);
        let fraction = if total == 0 {
            0.0
        } else {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        };
        if fraction > 0.0 {
            let fill = egui::Rect::from_min_size(
                rect.min,
                Vec2::new(rect.width() * fraction, rect.height()),
            );
            painter.rect_filled(fill, egui::CornerRadius::same(2), theme::ACCENT);
        }
        ui.label(ui::faint(format!(
            "copied {} / {}",
            format_bytes(done),
            format_bytes(total)
        )));
    }

    if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        let ready = !state.running && !dest.is_empty();
        let label = if state.running { "Moving…" } else { "Move" };
        if ui::ghost_button(ui, label, ready, theme::TEXT).clicked() {
            actions.push(Action::SubmitMoveDisk);
        }
        if ui::ghost_button(ui, "Cancel", !state.running, theme::TEXT_DIM)
            .on_hover_text(if state.running {
                "The copy is running; it finishes or rolls back on its own"
            } else {
                "Close without moving anything"
            })
            .clicked()
        {
            actions.push(Action::CloseModal);
        }
    });
}

/// Renaming a machine: one field, and what will follow the name.
fn rename_vm_body(
    ui: &mut egui::Ui,
    state: &mut crate::app::RenameVmState,
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    ui.label(ui::dim(
        "The profile, and the disks and UEFI store named after the machine, are \
         renamed with it. A disk another machine uses too keeps its name. The guest \
         inside is not touched: its own hostname stays what it was.",
    ));
    ui.add_space(4.0);
    ui.label(ui::faint(format!("− {}", state.profile.display())));
    ui.add_space(12.0);
    ui::form_row(
        ui,
        "NAME",
        "Letters, digits, '-', '_' and '.', starting with a letter or digit.",
        |ui, field_w| {
            let response =
                ui.add(egui::TextEdit::singleline(&mut state.name).desired_width(field_w));
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::SubmitRenameVm);
            }
        },
    );
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        let ready = !state.name.trim().is_empty();
        if ui::ghost_button(ui, "Rename", ready, theme::TEXT).clicked() {
            actions.push(Action::SubmitRenameVm);
        }
        if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

/// Cloning a machine: the new name, what gets copied, and the same progress
/// bar the move uses.
fn clone_vm_body(
    ui: &mut egui::Ui,
    state: &mut crate::app::CloneVmState,
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    ui.label(ui::dim(
        "Copies every writable disk next to the original (only the data it holds \
         travels, and the copy is verified), the UEFI boot entries and the settings. \
         The new machine gets its own network address; port forwards stay with the \
         original. Saved sessions are not copied — the clone starts with a cold boot.",
    ));
    ui.add_space(12.0);
    ui::form_row(
        ui,
        "NAME",
        "The new machine's name — also its profile's and disks' file names. Inside the \
         guest it is the same installation: same hostname and accounts.",
        |ui, field_w| {
            ui.add_enabled(
                !state.running,
                egui::TextEdit::singleline(&mut state.name).desired_width(field_w),
            );
        },
    );
    if let Some((done, total)) = state.progress {
        ui.add_space(12.0);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius::same(7), theme::INSET);
        let fraction = if total == 0 {
            0.0
        } else {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        };
        if fraction > 0.0 {
            let fill = egui::Rect::from_min_size(
                rect.min,
                Vec2::new(rect.width() * fraction, rect.height()),
            );
            painter.rect_filled(fill, egui::CornerRadius::same(2), theme::ACCENT);
        }
        ui.label(ui::faint(format!(
            "copied {} / {}",
            format_bytes(done),
            format_bytes(total)
        )));
    }
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        let ready = !state.running && !state.name.trim().is_empty();
        let label = if state.running { "Cloning…" } else { "Clone" };
        if ui::ghost_button(ui, label, ready, theme::TEXT).clicked() {
            actions.push(Action::SubmitCloneVm);
        }
        if ui::ghost_button(ui, "Cancel", !state.running, theme::TEXT_DIM)
            .on_hover_text(if state.running {
                "The copy is running; it finishes or rolls back on its own"
            } else {
                "Close without cloning anything"
            })
            .clicked()
        {
            actions.push(Action::CloseModal);
        }
    });
}

/// Growing a disk. Only growing: the CLI refuses to shrink because the guest
/// keeps data at the end of the image, so a "smaller" button could only ever
/// produce an error.
fn resize_disk_body(
    ui: &mut egui::Ui,
    state: &mut crate::app::ResizeDiskState,
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    ui.label(ui::dim(
        "Makes the image look larger to the guest. Nothing is written now — the file stays \
         sparse — and the guest still has to grow its own partition and filesystem to use \
         the new room.",
    ));
    ui.add_space(12.0);
    ui.label(ui::faint(format!(
        "now {} apparent{}",
        format_bytes(state.row.apparent_bytes),
        state
            .row
            .allocated_bytes
            .map(|allocated| format!(" · {} really on disk", format_bytes(allocated)))
            .unwrap_or_default()
    )));
    ui.add_space(10.0);

    ui::form_row(
        ui,
        "NEW SIZE",
        "The size the guest should see, written the way the rest of the product writes \
         sizes: 48G, 512M, or a plain number of bytes.",
        |ui, field_w| {
            ui.add(
                egui::TextEdit::singleline(&mut state.size)
                    .desired_width(field_w)
                    .hint_text("48G"),
            );
        },
    );
    match disk_image::parse_size(state.size.trim()) {
        Ok(bytes) if bytes < state.row.apparent_bytes => ui::form_note(
            ui,
            RichText::new(format!(
                "smaller than the current {} — a disk can only grow",
                format_bytes(state.row.apparent_bytes)
            ))
            .color(theme::ERR)
            .size(11.5),
        ),
        Ok(bytes) => ui::form_note(
            ui,
            ui::faint(format!(
                "= {} (a gain of {})",
                format_bytes(bytes),
                format_bytes(bytes.saturating_sub(state.row.apparent_bytes))
            )),
        ),
        Err(e) => ui::form_note(
            ui,
            RichText::new(e.to_string()).color(theme::WARN).size(11.5),
        ),
    }

    if !state.row.attachments.is_empty() {
        ui.add_space(8.0);
        let vms: Vec<&str> = state
            .row
            .attachments
            .iter()
            .map(|a| a.vm.as_str())
            .collect();
        ui.label(ui::faint(format!("used by {}", vms.join(", "))));
    }
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        let ready = disk_image::parse_size(state.size.trim())
            .is_ok_and(|bytes| bytes >= state.row.apparent_bytes);
        if ui::ghost_button(ui, "Grow the disk", ready, theme::TEXT).clicked() {
            actions.push(Action::SubmitResizeDisk);
        }
        if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

/// The snapshot delete confirmation.
///
/// No typed name here, unlike deleting a machine: what is lost is one session's
/// worth of work in progress, not a computer and its disks, and a confirmation
/// heavy enough for the second is theatre for the first. What the dialog owes
/// the user instead is an exact statement of what goes and what stays — and
/// those two sentences are the whole content.
fn delete_snapshot_body(
    ui: &mut egui::Ui,
    state: &crate::app::DeleteSnapshotState,
    actions: &mut Vec<Action>,
) {
    ui.label(
        RichText::new(crate::snapshots::what_is_lost(&state.row))
            .color(theme::TEXT)
            .size(13.0),
    );
    ui.add_space(12.0);
    ui.label(ui::faint(format!(
        "− {}  ({})",
        state.row.path.display(),
        format_bytes(state.row.apparent_bytes)
    )));
    if let Some(allocated) = state.row.allocated_bytes {
        ui.label(ui::faint(format!(
            "   {} of drive space comes back",
            format_bytes(allocated)
        )));
    }
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if ui::ghost_button(ui, "Forget it", true, theme::ERR).clicked() {
            actions.push(Action::ConfirmDeleteSnapshot);
        }
        if ui::ghost_button(ui, "Keep it", true, theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

/// "Start fresh" on a suspended machine.
///
/// The saved session is deleted *before* the machine starts, deliberately. A
/// cold-booted guest writes to the disk within seconds, and a snapshot pinned
/// to that disk as it was is then a file the engine will refuse — so keeping it
/// would mean a card that goes on offering a Resume that cannot work. Better to
/// throw it away with the user watching than to leave a trap.
fn discard_snapshot_body(
    ui: &mut egui::Ui,
    state: &crate::app::DiscardSnapshotState,
    actions: &mut Vec<Action>,
) {
    ui.label(
        RichText::new(format!(
            "'{}' has a saved session. Booting it from scratch cannot keep that: the guest \
             writes to the same disk the session is pinned to, and a snapshot whose disk has \
             moved on can no longer be restored.",
            state.vm
        ))
        .color(theme::TEXT)
        .size(13.0),
    );
    ui.add_space(12.0);
    ui.label(ui::dim(crate::snapshots::what_is_lost(&state.row)));
    ui.add_space(8.0);
    ui.label(ui::faint(format!(
        "− {}  ({})",
        state.row.path.display(),
        format_bytes(state.row.apparent_bytes)
    )));
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if ui::ghost_button(ui, "Discard it and start", true, theme::WARN).clicked() {
            actions.push(Action::ConfirmDiscardSnapshot);
        }
        if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

/// The wizard's "More options": every setting the presets choose for you,
/// by hand — exact cores and memory, how the installer runs, and where the
/// disk comes from.
fn more_options(
    ui: &mut egui::Ui,
    state: &mut crate::app::WizardState,
    settings: &crate::settings::Settings,
    scan: &crate::discovery::Scan,
    actions: &mut Vec<Action>,
) {
    ui.label(ui::caption("Processor & memory"));
    ui.add_space(6.0);
    ui.add_space(10.0);
    ui::form_row(
        ui,
        "MEMORY",
        "How much of this computer's RAM the machine may use while it runs. \
         It is taken from the host only while the machine is on.",
        |ui, field_w| {
            ui.spacing_mut().slider_width = field_w - 72.0;
            ui.add(
                egui::Slider::new(
                    &mut state.machine.memory_mib,
                    512..=control_api::MAX_MEMORY_MIB,
                )
                .step_by(256.0)
                .suffix(" MiB"),
            );
        },
    );
    ui::form_row(
        ui,
        "PROCESSORS",
        "How many processor cores the guest sees. More than this computer \
         physically has only makes the guest slower.",
        |ui, field_w| {
            ui.spacing_mut().slider_width = field_w - 72.0;
            ui.add(egui::Slider::new(&mut state.machine.vcpus, 1..=16).suffix(" vCPU"));
        },
    );
    ui.add_space(12.0);
    ui.checkbox(&mut state.machine.automated, "Install automatically")
        .on_hover_text("Uses Entangled's maintained unattended profile");
    ui.checkbox(
        &mut state.machine.headless,
        "Run installer without a window",
    )
    .on_hover_text("The serial console remains available in Activity");
    ui.add_space(18.0);
    ui.label(ui::caption("Storage"));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let card_width = (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
        if option_card(
            ui,
            card_width,
            "Create a new disk",
            "Fast sparse RAW image; grows only as data is written",
            state.machine.disk_mode == launcher::DiskMode::CreateNew,
            true,
            theme::ACCENT,
        )
        .clicked()
        {
            state.machine.disk_mode = launcher::DiskMode::CreateNew;
            state.machine.disk_path.clear();
        }
        if option_card(
            ui,
            card_width,
            "Use an existing disk",
            "Keep the data already present in a library image",
            state.machine.disk_mode == launcher::DiskMode::UseExisting,
            true,
            theme::ACCENT,
        )
        .clicked()
        {
            state.machine.disk_mode = launcher::DiskMode::UseExisting;
        }
    });
    ui.add_space(16.0);
    match state.machine.disk_mode {
        launcher::DiskMode::CreateNew => {
            ui::form_row(
                ui,
                "FILE NAME",
                "The image file this machine's disk is stored in. It is \
                 created inside your machine folder; leave it empty and it \
                 is named after the machine.",
                |ui, field_w| {
                    ui.add(
                        egui::TextEdit::singleline(&mut state.machine.disk_path)
                            .desired_width(field_w)
                            .hint_text(format!("{}.raw", state.machine.name)),
                    );
                },
            );
            ui::form_note(ui, ui::faint(format!("in {}", settings.vm_dir.display())));
            ui.add_space(10.0);
            ui::form_row(
                ui,
                "CAPACITY",
                "How large the disk looks to the guest. The file only grows \
                 as the guest actually writes, so a generous number costs \
                 nothing up front.",
                |ui, field_w| {
                    ui.spacing_mut().slider_width = field_w - 64.0;
                    ui.add(
                        egui::Slider::new(&mut state.machine.disk_gib, 8..=512)
                            .step_by(2.0)
                            .suffix(" GiB"),
                    );
                },
            );
        }
        launcher::DiskMode::UseExisting => {
            ui::form_row(
                ui,
                "PICK ONE",
                "Images already in your machine folder. Whichever you pick \
                 is installed onto as-is — never recreated or emptied.",
                |ui, field_w| {
                    egui::ComboBox::from_id_salt("existing-disk")
                        .selected_text(if state.machine.disk_path.trim().is_empty() {
                            "Choose a disk…".to_string()
                        } else {
                            state.machine.disk_path.clone()
                        })
                        .width(ui::combo_width(field_w))
                        .show_ui(ui, |ui| {
                            for disk in scan.disks.iter().filter(|d| {
                                d.exists && d.path.parent() == Some(settings.vm_dir.as_path())
                            }) {
                                if let Some(name) = disk.path.file_name() {
                                    let name = name.to_string_lossy().into_owned();
                                    ui.selectable_value(
                                        &mut state.machine.disk_path,
                                        name.clone(),
                                        format!(
                                            "{}  ·  {}",
                                            name,
                                            format_bytes(disk.apparent_bytes)
                                        ),
                                    );
                                }
                            }
                        });
                },
            );
            if ui::path_row(
                ui,
                "OR BROWSE",
                "Any RAW disk image on this computer. It must end up inside \
                 your machine folder, so that the profile the installer \
                 writes stays visible here.",
                &mut state.machine.disk_path,
                "existing.raw",
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::WizardDisk));
            }
        }
    }
}

/// One click sizes a machine. The numbers are the ones a desktop guest is
/// comfortable with at each step; "More options" sets any other pair.
struct PowerPreset {
    name: &'static str,
    vcpus: u32,
    memory_mib: u64,
    hint: &'static str,
}

const POWER_PRESETS: [PowerPreset; 3] = [
    PowerPreset {
        name: "Light",
        vcpus: 2,
        memory_mib: 4096,
        hint: "servers",
    },
    PowerPreset {
        name: "Balanced",
        vcpus: 4,
        memory_mib: 8192,
        hint: "a desktop",
    },
    PowerPreset {
        name: "Performance",
        vcpus: 8,
        memory_mib: 16384,
        hint: "builds",
    },
];

/// The disk sizes offered as one-click chips.
const DISK_CHOICES_GIB: [u64; 4] = [32, 64, 128, 256];

/// A pill-shaped pick-one chip.
fn choice_chip(ui: &mut egui::Ui, text: &str, selected: bool) -> egui::Response {
    let font = egui::FontId::proportional(13.0);
    let fg = if selected {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 28.0, 34.0),
        egui::Sense::click(),
    );
    let hover = theme::animate_bool(ui.ctx(), response.id.with("hover"), response.hovered(), 0.1);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(255),
        if selected {
            theme::SELECTED
        } else {
            theme::MODAL
        },
    );
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(255),
        egui::Stroke::new(
            1.0_f32,
            if selected {
                theme::ACCENT
            } else {
                theme::mix(theme::STROKE_STRONG, theme::TEXT_FAINT, hover)
            },
        ),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        egui::Color32::TRANSPARENT,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// Where the wizard is: a check for each step done, a filled dot for this one,
/// an open dot for the ones to come, joined by short rules.
fn wizard_progress(ui: &mut egui::Ui, current: usize) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        for (index, label) in ["System", "Name & size", "Review"].into_iter().enumerate() {
            let (dot, _) = ui.allocate_exact_size(Vec2::new(16.0, 20.0), egui::Sense::hover());
            let c = dot.center();
            let painter = ui.painter();
            if index < current {
                painter.add(egui::Shape::line(
                    vec![
                        c + Vec2::new(-4.5, 0.5),
                        c + Vec2::new(-1.5, 3.5),
                        c + Vec2::new(4.5, -3.0),
                    ],
                    egui::Stroke::new(1.8_f32, theme::OK),
                ));
            } else if index == current {
                painter.circle_filled(c, 4.0, theme::ACCENT);
            } else {
                painter.circle_stroke(c, 3.5, egui::Stroke::new(1.2_f32, theme::STROKE_STRONG));
            }
            ui.label(
                RichText::new(label)
                    .size(13.0)
                    .color(match index.cmp(&current) {
                        std::cmp::Ordering::Less => theme::TEXT_DIM,
                        std::cmp::Ordering::Equal => theme::TEXT,
                        std::cmp::Ordering::Greater => theme::TEXT_FAINT,
                    }),
            );
            if index < 2 {
                let (rule, _) = ui.allocate_exact_size(Vec2::new(28.0, 20.0), egui::Sense::hover());
                ui.painter().hline(
                    rule.x_range(),
                    rule.center().y,
                    egui::Stroke::new(1.0_f32, theme::STROKE_STRONG),
                );
            }
        }
    });
}

fn wizard_heading(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.label(RichText::new(title).size(22.0).color(theme::TEXT).strong());
    ui.add_space(4.0);
    ui.label(RichText::new(detail).size(13.5).color(theme::TEXT_DIM));
}

/// A big pick-one card: title, a line of detail, and a radio mark. `enabled`
/// is what a capability gate turns off: the card stays visible (the choice
/// exists, it is just unavailable here) but stops responding, and the caller
/// hangs the reason off the returned response.
fn option_card(
    ui: &mut egui::Ui,
    width: f32,
    title: &str,
    detail: &str,
    selected: bool,
    enabled: bool,
    tint: egui::Color32,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let tint = if enabled { tint } else { theme::TEXT_FAINT };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 80.0), sense);
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("option-hover"),
        enabled && response.hovered(),
        0.12,
    );
    let radius = egui::CornerRadius::same(10);
    let painter = ui.painter();
    if selected {
        painter.rect_filled(
            rect.expand(3.0),
            egui::CornerRadius::same(13),
            tint.gamma_multiply(0.10),
        );
    }
    painter.rect_filled(
        rect,
        radius,
        if selected {
            theme::CARD_HOVER
        } else {
            theme::mix(theme::CARD, theme::CARD_HOVER, hover)
        },
    );
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(
            1.0_f32,
            if selected {
                tint
            } else {
                theme::mix(theme::STROKE, theme::STROKE_STRONG, hover)
            },
        ),
        egui::StrokeKind::Inside,
    );
    let mark = egui::pos2(rect.right() - 20.0, rect.top() + 22.0);
    if selected {
        painter.circle_filled(mark, 7.0, tint);
        painter.circle_filled(mark, 3.0, theme::MODAL);
    } else {
        painter.circle_stroke(mark, 6.5, egui::Stroke::new(1.0_f32, theme::STROKE_STRONG));
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(16.0, 14.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    child.label(
        RichText::new(title)
            .size(14.5)
            .color(if enabled {
                theme::TEXT
            } else {
                theme::TEXT_FAINT
            })
            .strong(),
    );
    child.add_space(4.0);
    child.label(RichText::new(detail).size(12.0).color(theme::TEXT_DIM));
    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

fn variant_label(variant: &str) -> &'static str {
    match variant {
        "text-netboot" => "Text installer · network",
        "netinst-iso" => "Official netinst ISO",
        _ => "Graphical installer · network",
    }
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(104.0, 22.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(ui::faint(label.to_uppercase()));
            },
        );
        ui.label(RichText::new(value).size(13.0).color(theme::TEXT));
    });
}

// `text_row` and `slider_row` used to live here, each with its own hard-coded
// 78 px label column. They are gone: every labelled row in the product now goes
// through `ui::form_row` / `ui::path_row`, which is what keeps the fields in one
// straight column and the explanations in tooltips.

/// Shared modal chrome: dimmed backdrop, dialog surface, a title over a rule.
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
                .fill(theme::MODAL)
                .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
                .corner_radius(egui::CornerRadius::same(12))
                .shadow(egui::Shadow {
                    offset: [0, 24],
                    blur: 60,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(150),
                })
                .inner_margin(egui::Margin::same(28)),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.label(RichText::new(title).size(20.0).color(theme::TEXT).strong());
            ui.add_space(10.0);
            theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());
            ui.add_space(18.0);
            add(ui);
        });
    response.should_close()
}

/// The 3D renderer as the editor's combo box names it.
fn gpu_label(renderer: control_api::GpuRenderer) -> &'static str {
    use control_api::GpuRenderer;
    match renderer {
        GpuRenderer::TwoD => "Off (2D)",
        GpuRenderer::Virgl => "OpenGL (virgl)",
        GpuRenderer::Venus => "GPU desktop (Venus)",
    }
}

fn gpu_hint(renderer: control_api::GpuRenderer) -> &'static str {
    use control_api::GpuRenderer;
    match renderer {
        GpuRenderer::TwoD => {
            "No 3D. The guest draws its desktop with its own processor, which works \
             everywhere."
        }
        GpuRenderer::Virgl => "OpenGL through the host's virglrenderer. Linux/KVM backends only.",
        GpuRenderer::Venus => {
            "The guest's Vulkan, and its OpenGL and desktop through Zink, on this \
             computer's graphics card. Either backend; needs a Vulkan driver, which \
             Diagnostics checks."
        }
    }
}
