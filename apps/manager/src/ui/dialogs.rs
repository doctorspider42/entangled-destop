//! Modal surfaces: the new-machine wizard (GUI-1602), the delete confirmation
//! (GUI-1604) and the settings panel.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp, Modal};
use crate::backend::Backend;
use crate::discovery::format_bytes;
use crate::launcher::{self, VARIANTS};
use crate::picker::PickTarget;
use crate::theme;
use crate::ui;

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
        ..
    } = app;

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
                            "Pick a guided installer. Entangled prepares the boot path and keeps its console attached here.",
                        );
                        ui.add_space(14.0);
                        // An installer this backend cannot run is greyed out
                        // with the reason on hover, rather than failing forty
                        // minutes later inside the engine.
                        let debian_block = state.machine.backend.debian_install_block();
                        ui.horizontal(|ui| {
                            let card_width =
                                (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
                            let debian = option_card(
                                ui,
                                card_width,
                                "Debian",
                                "Verified network or official installer media",
                                state.machine.family == launcher::GuestFamily::Debian,
                                debian_block.is_none(),
                                theme::CYAN,
                            );
                            if let Some(reason) = debian_block {
                                debian.on_hover_text(reason.long);
                            } else if debian.clicked() {
                                state.machine.family = launcher::GuestFamily::Debian;
                            }
                            if option_card(
                                ui,
                                card_width,
                                "Ubuntu",
                                "UEFI installer from cache or a local ISO",
                                state.machine.family == launcher::GuestFamily::Ubuntu,
                                true,
                                theme::VIOLET,
                            )
                            .clicked()
                            {
                                state.machine.family = launcher::GuestFamily::Ubuntu;
                            }
                        });
                        if let Some(reason) = debian_block {
                            ui.add_space(6.0);
                            ui.label(RichText::new(reason.short).color(theme::WARN).size(11.5));
                            // Keep the form honest: never leave a blocked
                            // choice selected behind the user's back.
                            if state.machine.family == launcher::GuestFamily::Debian {
                                state.machine.family = launcher::GuestFamily::Ubuntu;
                            }
                        }
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
                        backend_row(ui, &mut state.machine.backend);
                    }
                    1 => {
                        wizard_heading(
                            ui,
                            "Name it and size the hardware",
                            "Friendly defaults work well; you can tune everything again after installation.",
                        );
                        ui.add_space(14.0);
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
                                ui.add(
                                    egui::Slider::new(&mut state.machine.vcpus, 1..=16)
                                        .suffix(" vCPU"),
                                );
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
                    }
                    2 => {
                        wizard_heading(
                            ui,
                            "Choose storage",
                            "Create a new sparse disk or install onto a RAW image already in your machine library.",
                        );
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            let card_width =
                                (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
                            if option_card(
                                ui,
                                card_width,
                                "Create a new disk",
                                "Fast sparse RAW image; grows only as data is written",
                                state.machine.disk_mode == launcher::DiskMode::CreateNew,
                                true,
                                theme::CYAN,
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
                                theme::VIOLET,
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
                                            egui::TextEdit::singleline(
                                                &mut state.machine.disk_path,
                                            )
                                            .desired_width(field_w)
                                            .hint_text(format!("{}.raw", state.machine.name)),
                                        );
                                    },
                                );
                                ui::form_note(
                                    ui,
                                    ui::faint(format!("in {}", settings.vm_dir.display())),
                                );
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
                                            .selected_text(
                                                if state.machine.disk_path.trim().is_empty() {
                                                    "Choose a disk…".to_string()
                                                } else {
                                                    state.machine.disk_path.clone()
                                                },
                                            )
                                            .width(ui::combo_width(field_w))
                                            .show_ui(ui, |ui| {
                                                for disk in scan.disks.iter().filter(|d| {
                                                    d.exists
                                                        && d.path.parent()
                                                            == Some(settings.vm_dir.as_path())
                                                }) {
                                                    if let Some(name) = disk.path.file_name() {
                                                        let name =
                                                            name.to_string_lossy().into_owned();
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
                                        "{} MiB memory · {} vCPU",
                                        state.machine.memory_mib, state.machine.vcpus
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
                                summary_row(ui, "Runs on", state.machine.backend.label());
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
                        ui.add_space(10.0);
                        ui.collapsing("Advanced: command preview", |ui| {
                            ui.label(
                                RichText::new(match &preview {
                                    Ok(spec) => spec.command_line(),
                                    Err(_) => "—".to_string(),
                                })
                                .monospace()
                                .size(11.5)
                                .color(theme::TEXT_DIM),
                            );
                        });
                    }
                }

                if let Some(error) = &state.error {
                    ui.add_space(10.0);
                    ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                }

                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    if state.step > 0
                        && ui::ghost_button(ui, "Back", true, theme::TEXT_DIM).clicked()
                    {
                        state.step -= 1;
                        state.error = None;
                    }
                    if state.step < 3 {
                        if ui::primary_button(ui, "Continue").clicked() {
                            state.step += 1;
                            state.error = None;
                        }
                    } else if ui::primary_button(ui, "Create & install").clicked() {
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
        Modal::Settings(form) => frame(ctx, "settings", "Settings", 600.0, |ui| {
            settings_body(ui, form, engine, actions)
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
                if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
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
                    if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
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
                if ui::ghost_button(ui, "Attach", ready, theme::CYAN).clicked() {
                    actions.push(Action::SubmitAttachDisk);
                }
                if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
                    actions.push(Action::CloseModal);
                }
            });
        }),
        Modal::EditVm(state) => frame(
            ctx,
            "edit-vm",
            &format!("Edit {}", state.form.name),
            760.0,
            |ui| edit_vm_body(ui, state, actions),
        ),
        Modal::MoveDisk(state) => frame(
            ctx,
            "move-disk",
            &format!("Move {}", state.row.file_name),
            560.0,
            |ui| move_disk_body(ui, state, actions),
        ),
        Modal::ResizeDisk(state) => frame(
            ctx,
            "resize-disk",
            &format!("Grow {}", state.row.file_name),
            520.0,
            |ui| resize_disk_body(ui, state, actions),
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
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    engine_status(ui, engine, actions);
    ui.add_space(16.0);

    if ui::path_row(
        ui,
        "MACHINES IN",
        "The folder your machines live in: their settings files and their disk images. \
         Everything the Machines and Storage views show comes from here.",
        &mut form.vm_dir,
        "~/entangled-vms",
        true,
    ) {
        actions.push(Action::PickPath(PickTarget::VmDir));
    }
    ui.add_space(14.0);

    backend_row(ui, &mut form.default_backend);
    if Backend::Wsl.available_on_host() && form.default_backend == Backend::Wsl {
        wsl_rows(ui, form);
    }
    ui.add_space(14.0);

    ui.checkbox(&mut form.headless_install, "Install without a window")
        .on_hover_text(
            "The installer still runs and its console is still visible under Activity — it \
             just does not open a window of its own.",
        );
    ui.checkbox(
        &mut form.check_updates_on_startup,
        "Check for updates on startup",
    )
    .on_hover_text(
        "Asks the release server once when the manager opens, on a background thread. \
         Being offline is silently fine.",
    );
    ui.checkbox(&mut form.animations_enabled, "Interface motion")
        .on_hover_text(
            "Ambient scan, logo movement, status pulses and hover easing. Turning it off \
             also drops the window to an event-driven redraw.",
        );

    ui.add_space(14.0);
    let advanced = egui::CollapsingHeader::new(
        RichText::new("Advanced and diagnostics")
            .size(12.5)
            .color(theme::TEXT_DIM),
    )
    .id_salt("settings-advanced")
    .default_open(form.advanced_open);
    advanced.show(ui, |ui| {
        ui.label(ui::faint(
            "Entangled works these out for itself. Change them only if you are running a \
             build from a source checkout.",
        ));
        ui.add_space(10.0);
        if ui::path_row(
            ui,
            "ENGINE",
            "The program that actually runs a machine. Leave empty and Entangled uses the \
             one installed beside this manager.",
            &mut form.entangled_binary,
            "found automatically",
            true,
        ) {
            actions.push(Action::PickPath(PickTarget::EngineBinary));
        }
        ui.add_space(8.0);
        if ui::path_row(
            ui,
            "WORKING DIR",
            "Machines started from here run in this folder, so any relative path inside a \
             machine's settings — its firmware, its kernel — is resolved against it. Leave \
             empty to use the manager's own folder.",
            &mut form.work_dir,
            "the manager's own folder",
            true,
        ) {
            actions.push(Action::PickPath(PickTarget::WorkDir));
        }
        if Backend::Wsl.available_on_host() && form.default_backend != Backend::Wsl {
            ui.add_space(8.0);
            wsl_rows(ui, form);
        }
    });

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if ui::primary_button(ui, "Save").clicked() {
            actions.push(Action::SaveSettings);
        }
        if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

/// The engine, as a fact rather than a question.
fn engine_status(
    ui: &mut egui::Ui,
    engine: &Result<crate::launcher::Engine, String>,
    actions: &mut Vec<Action>,
) {
    let tint = match engine {
        Ok(_) => theme::OK,
        Err(_) => theme::ERR,
    };
    egui::Frame::new()
        .fill(theme::mix(theme::CARD, tint, 0.06))
        .stroke(egui::Stroke::new(1.0_f32, tint.gamma_multiply(0.45)))
        .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
        .inner_margin(egui::Margin::symmetric(13, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match engine {
                Ok(engine) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 7.0;
                        ui.label(
                            RichText::new(format!("Engine: {}", engine.summary()))
                                .size(13.0)
                                .color(theme::TEXT),
                        );
                        ui::chip(ui, "ready", theme::OK);
                    })
                    .response
                    .on_hover_text(engine.path.display().to_string());
                }
                Err(message) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 7.0;
                        ui.label(RichText::new(message).size(13.0).color(theme::TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui::ghost_button(ui, "Locate it…", true, theme::ERR).clicked() {
                                actions.push(Action::PickPath(PickTarget::EngineBinary));
                            }
                        });
                    });
                }
            }
        });
}

/// The where-does-this-run row, shared by Settings (the default) and the wizard
/// and editor (one machine). On a host with only one backend it degrades to a
/// read-only line rather than a combo box with one entry.
fn backend_row(ui: &mut egui::Ui, value: &mut Backend) {
    let choices: Vec<Backend> = Backend::ALL
        .into_iter()
        .filter(|backend| backend.available_on_host())
        .collect();
    if choices.len() < 2 {
        ui::form_row(ui, "RUNS ON", Backend::Native.tooltip(), |ui, _| {
            ui.label(
                RichText::new(Backend::Native.label())
                    .size(13.0)
                    .color(theme::TEXT),
            );
        });
        return;
    }
    ui::form_row(
        ui,
        "RUNS ON",
        "Which hypervisor starts the machine. Windows runs it natively; WSL runs the Linux \
         build inside your WSL distribution, which is the only way to get 3D acceleration \
         and TAP networking today.",
        |ui, field_w| {
            egui::ComboBox::from_id_salt(("backend-choice", ui.id()))
                .selected_text(value.label())
                .width(ui::combo_width(field_w))
                .show_ui(ui, |ui| {
                    for backend in &choices {
                        ui.selectable_value(value, *backend, backend.label())
                            .on_hover_text(backend.tooltip());
                    }
                });
        },
    );
}

/// The two things the WSL backend cannot work out for itself.
fn wsl_rows(ui: &mut egui::Ui, form: &mut crate::app::SettingsForm) {
    ui::form_scope(ui);
    ui.add_space(8.0);
    ui::form_row(
        ui,
        "WSL DISTRO",
        "Which installed WSL system to run the machine in — the name shown by `wsl --list`.",
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
        "LINUX ENGINE",
        "Where the Linux build of Entangled lives inside WSL — a Linux path such as \
         /usr/bin/entangled. Leave it empty to use whatever `entangled` resolves to on the \
         WSL PATH. A Windows installation does not include a Linux build, so this is \
         normally a path into your own checkout.",
        |ui, field_w| {
            ui.add(
                egui::TextEdit::singleline(&mut form.wsl_entangled)
                    .desired_width(field_w)
                    .hint_text("/usr/bin/entangled"),
            );
        },
    );
}

/// The Edit-VM form body. The left-hand cards are intentionally navigation,
/// not separate save boundaries: changing one area keeps all unsaved edits and
/// Save validates and writes the complete profile.
fn edit_vm_body(ui: &mut egui::Ui, state: &mut crate::app::EditVmState, actions: &mut Vec<Action>) {
    use crate::app::EditVmSection;

    let mut selected = state.section;
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(184.0);
            ui.label(ui::faint("MACHINE SETTINGS"));
            ui.add_space(7.0);
            for (section, title, detail, tint) in [
                (
                    EditVmSection::Hardware,
                    "Hardware",
                    "Memory, processors, transport",
                    theme::CYAN,
                ),
                (
                    EditVmSection::BootMedia,
                    "Boot & media",
                    "UEFI, Linux kernel, ISO",
                    theme::VIOLET,
                ),
                (
                    EditVmSection::NetworkDisplay,
                    "Network & display",
                    "Connectivity, resolution, 3D",
                    theme::OK,
                ),
                (
                    EditVmSection::Storage,
                    "Storage",
                    "Virtual disks and boot order",
                    theme::WARN,
                ),
            ] {
                if edit_section_card(ui, title, detail, selected == section, tint).clicked() {
                    selected = section;
                }
                ui.add_space(8.0);
            }
        });

        ui.add_space(10.0);
        let (rule, _) = ui.allocate_exact_size(Vec2::new(1.0, 352.0), egui::Sense::hover());
        ui.painter().vline(
            rule.center().x,
            rule.y_range(),
            egui::Stroke::new(1.0_f32, theme::STROKE),
        );
        ui.add_space(10.0);

        egui::Frame::new()
            .fill(theme::mix(theme::CARD, theme::BG_PANEL, 0.38))
            .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
            .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    // Frame::show inherits the surrounding horizontal layout;
                    // this explicit column keeps fields vertical and bounded.
                    ui.set_width(500.0);
                    ui.set_min_height(316.0);
                    match selected {
                        EditVmSection::Hardware => edit_hardware(ui, &mut state.form),
                        EditVmSection::BootMedia => {
                            edit_boot_media(ui, &mut state.form, actions);
                        }
                        EditVmSection::NetworkDisplay => {
                            edit_network_display(ui, &mut state.form);
                        }
                        EditVmSection::Storage => edit_storage(ui, &mut state.form, actions),
                    }
                });
            });
    });
    state.section = selected;

    let preview = state.form.to_config();
    if let Err(message) = &preview {
        ui.add_space(8.0);
        ui.label(RichText::new(message).color(theme::ERR).size(12.5));
    } else if let Some(error) = &state.error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        let savable = preview.is_ok() && state.form.dirty();
        if ui::ghost_button(ui, "Save changes", savable, theme::CYAN)
            .on_hover_text("Validates and saves every section")
            .clicked()
        {
            actions.push(Action::SubmitEditVm);
        }
        if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(ui::faint("All sections are saved together"));
        });
    });
}

fn edit_section_card(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    selected: bool,
    tint: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(184.0, 58.0), egui::Sense::click());
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("edit-section-hover"),
        response.hovered(),
        0.14,
    );
    let fill_strength = if selected { 0.14 } else { 0.025 + hover * 0.06 };
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(theme::CONTROL_RADIUS),
        theme::mix(theme::CARD, tint, fill_strength),
    );
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(theme::CONTROL_RADIUS),
        egui::Stroke::new(
            if selected { 1.5_f32 } else { 1.0_f32 },
            if selected {
                tint.gamma_multiply(0.82)
            } else {
                theme::mix(theme::STROKE, tint, hover * 0.38)
            },
        ),
        egui::StrokeKind::Inside,
    );
    if selected {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                rect.left_top() + Vec2::new(0.0, theme::CONTROL_RADIUS as f32),
                Vec2::new(3.0, rect.height() - 2.0 * theme::CONTROL_RADIUS as f32),
            ),
            egui::CornerRadius::same(2),
            tint,
        );
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(12.0, 9.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    child.add(
        egui::Label::new(
            RichText::new(title)
                .size(13.5)
                .color(if selected {
                    theme::TEXT
                } else {
                    theme::TEXT_DIM
                })
                .strong(),
        )
        .selectable(false),
    );
    child.add_space(2.0);
    child.add(
        egui::Label::new(RichText::new(detail).size(10.5).color(theme::TEXT_FAINT))
            .selectable(false),
    );
    if ui.rect_contains_pointer(rect) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
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

fn edit_hardware(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
    use control_api::VirtioTransport;

    edit_panel_heading(
        ui,
        "Hardware",
        "Choose the resources this machine may use on the host.",
    );
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
    backend_row(ui, &mut form.backend);
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
                ui::path_status_note(ui, &form.resolve(&firmware), launcher::FIRMWARE_FIX);
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
        clicked = ui::ghost_button(ui, label, true, theme::CYAN)
            .on_hover_text("Fills the field in with the value Entangled would use")
            .clicked();
    });
    clicked
}

/// Network & display — and the two capability gates that used to be discovered
/// at boot: TAP networking and 3D. Both are decided by the machine's backend,
/// so both are answerable here, before anything is launched.
fn edit_network_display(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
    use crate::editor::NetworkChoice;
    edit_panel_heading(
        ui,
        "Network & display",
        "Connect the guest and choose how its desktop is presented.",
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

    ui.add_space(18.0);
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
    ui.add_space(6.0);

    let virgl_block = form.backend.virgl_block();
    if virgl_block.is_some() {
        // Unavailable here — and never silently left on, because the engine
        // refuses to boot a profile that asks for 3D it cannot deliver.
        form.virgl = false;
    }
    ui::form_row(
        ui,
        "3D",
        "Hardware-accelerated 3D inside the guest, using this computer's graphics card. \
         Without it the guest still has a desktop, drawn by its own processor.",
        |ui, _| {
            let response = ui
                .add_enabled_ui(virgl_block.is_none(), |ui| {
                    ui.checkbox(&mut form.virgl, "Accelerate 3D graphics")
                })
                .inner;
            match virgl_block {
                Some(reason) => response.on_hover_text(reason.long),
                None => response.on_hover_text(
                    "The machine gets a real GPU pipeline. If the host cannot provide one, \
                     the machine refuses to start rather than quietly falling back to \
                     software rendering.",
                ),
            };
        },
    );
    if let Some(reason) = virgl_block {
        ui::form_note(
            ui,
            RichText::new(reason.short).color(theme::WARN).size(11.0),
        );
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
                                theme::VIOLET,
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
        if ui::ghost_button(ui, "Attach it", !path.is_empty(), theme::CYAN).clicked() {
            form.disks.push(control_api::DiskSection {
                path: std::path::PathBuf::from(path),
                writable: true,
            });
            form.add_disk.clear();
        }
    });
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
            theme::gradient_rect(
                painter,
                fill,
                theme::CYAN.gamma_multiply(0.9),
                theme::VIOLET.gamma_multiply(0.9),
            );
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
        if ui::ghost_button(ui, label, ready, theme::VIOLET).clicked() {
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
        if ui::ghost_button(ui, "Grow the disk", ready, theme::CYAN).clicked() {
            actions.push(Action::SubmitResizeDisk);
        }
        if ui::ghost_button(ui, "Cancel", true, theme::TEXT_DIM).clicked() {
            actions.push(Action::CloseModal);
        }
    });
}

fn wizard_progress(ui: &mut egui::Ui, current: usize) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        for (index, label) in ["System", "Hardware", "Storage", "Review"]
            .into_iter()
            .enumerate()
        {
            let active = index == current;
            let done = index < current;
            let tint = if active {
                theme::CYAN
            } else if done {
                theme::OK
            } else {
                theme::TEXT_FAINT
            };
            let marker = if done {
                "OK".to_string()
            } else {
                format!("{:02}", index + 1)
            };
            ui.label(RichText::new(marker).monospace().color(tint).size(11.0));
            ui.label(
                RichText::new(label)
                    .color(if active { theme::TEXT } else { theme::TEXT_DIM })
                    .strong(),
            );
            if index < 3 {
                ui.add_space(3.0);
                ui.label(RichText::new("/").color(theme::STROKE_STRONG));
            }
        }
    });
}

fn wizard_heading(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.label(RichText::new(title).size(21.0).color(theme::TEXT).strong());
    ui.add_space(3.0);
    ui.label(RichText::new(detail).size(13.0).color(theme::TEXT_DIM));
}

/// A big pick-one card. `enabled` is what a capability gate turns off: the card
/// stays visible (the choice exists, it is just unavailable here) but stops
/// responding, and the caller hangs the reason off the returned response.
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
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 78.0), sense);
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("option-hover"),
        enabled && response.hovered(),
        0.14,
    );
    let strength = if selected { 0.16 } else { 0.05 + hover * 0.06 };
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(theme::CONTROL_RADIUS),
        theme::mix(theme::CARD, tint, strength),
    );
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(theme::CONTROL_RADIUS),
        egui::Stroke::new(
            if selected { 1.5_f32 } else { 1.0_f32 },
            if selected {
                tint.gamma_multiply(0.9)
            } else {
                theme::mix(theme::STROKE, tint, hover * 0.4)
            },
        ),
        egui::StrokeKind::Inside,
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(13.0))
            .layout(Layout::top_down(Align::Min)),
    );
    child.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .size(15.0)
                .color(if enabled {
                    theme::TEXT
                } else {
                    theme::TEXT_FAINT
                })
                .strong(),
        );
        if selected {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("SELECTED").monospace().size(10.0).color(tint));
            });
        }
    });
    child.add_space(5.0);
    child.label(RichText::new(detail).size(12.0).color(theme::TEXT_DIM));
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
            let time = theme::animation_time(ui.ctx());
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
