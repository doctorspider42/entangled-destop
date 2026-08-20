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
        scan,
        supervisor,
        ..
    } = app;

    let closed = match modal {
        Modal::None => false,
        Modal::Wizard(state) => {
            let cli_label = cli
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "entangled".to_string());
            frame(ctx, "wizard", "Create a machine", 760.0, |ui| {
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
                        ui.horizontal(|ui| {
                            let card_width =
                                (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
                            if option_card(
                                ui,
                                card_width,
                                "Debian",
                                "Verified network or official installer media",
                                state.machine.family == launcher::GuestFamily::Debian,
                                theme::CYAN,
                            )
                            .clicked()
                            {
                                state.machine.family = launcher::GuestFamily::Debian;
                            }
                            if option_card(
                                ui,
                                card_width,
                                "Ubuntu",
                                "UEFI installer from cache or a local ISO",
                                state.machine.family == launcher::GuestFamily::Ubuntu,
                                theme::VIOLET,
                            )
                            .clicked()
                            {
                                state.machine.family = launcher::GuestFamily::Ubuntu;
                            }
                        });
                        ui.add_space(16.0);
                        match state.machine.family {
                            launcher::GuestFamily::Debian => {
                                ui.label(ui::faint("INSTALLER EXPERIENCE"));
                                egui::ComboBox::from_id_salt("variant")
                                    .selected_text(variant_label(&state.machine.variant))
                                    .width(320.0)
                                    .show_ui(ui, |ui| {
                                        for variant in VARIANTS {
                                            ui.selectable_value(
                                                &mut state.machine.variant,
                                                variant.to_string(),
                                                variant_label(variant),
                                            );
                                        }
                                    });
                                ui.label(ui::dim(match state.machine.variant.as_str() {
                                    "text-netboot" => "Fast, keyboard-driven network installer.",
                                    "netinst-iso" => {
                                        "Official netinst ISO fetched and verified for you."
                                    }
                                    _ => "Graphical Debian installer with verified network media.",
                                }));
                            }
                            launcher::GuestFamily::Ubuntu => {
                                ui.label(ui::faint("LOCAL INSTALLER ISO (OPTIONAL)"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.machine.iso_path)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(
                                            "Leave empty to use the newest verified cached ISO",
                                        ),
                                );
                                ui.label(ui::dim(
                                    "Paste an absolute .iso path, or leave it empty and Entangled uses its verified Ubuntu cache.",
                                ));
                            }
                        }
                    }
                    1 => {
                        wizard_heading(
                            ui,
                            "Name it and size the hardware",
                            "Friendly defaults work well; you can tune everything again after installation.",
                        );
                        ui.add_space(14.0);
                        ui.label(ui::faint("MACHINE NAME"));
                        ui.add(
                            egui::TextEdit::singleline(&mut state.machine.name)
                                .desired_width(f32::INFINITY)
                                .hint_text("my-machine"),
                        );
                        ui.add_space(14.0);
                        slider_row(ui, "MEMORY", |ui| {
                            ui.add(
                                egui::Slider::new(
                                    &mut state.machine.memory_mib,
                                    512..=control_api::MAX_MEMORY_MIB,
                                )
                                .step_by(256.0)
                                .suffix(" MiB"),
                            );
                        });
                        slider_row(ui, "PROCESSORS", |ui| {
                            ui.add(
                                egui::Slider::new(&mut state.machine.vcpus, 1..=16).suffix(" vCPU"),
                            );
                        });
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
                                ui.label(ui::faint("DISK FILE"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.machine.disk_path)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(format!("{}.raw", state.machine.name)),
                                );
                                ui.label(ui::faint(format!(
                                    "Saved in {}",
                                    settings.vm_dir.display()
                                )));
                                ui.add_space(12.0);
                                slider_row(ui, "CAPACITY", |ui| {
                                    ui.add(
                                        egui::Slider::new(&mut state.machine.disk_gib, 8..=512)
                                            .step_by(2.0)
                                            .suffix(" GiB"),
                                    );
                                });
                            }
                            launcher::DiskMode::UseExisting => {
                                ui.label(ui::faint("EXISTING RAW IMAGE"));
                                egui::ComboBox::from_id_salt("existing-disk")
                                    .selected_text(if state.machine.disk_path.trim().is_empty() {
                                        "Choose a disk…".to_string()
                                    } else {
                                        state.machine.disk_path.clone()
                                    })
                                    .width(ui.available_width())
                                    .show_ui(ui, |ui| {
                                        for disk in scan.disks.iter().filter(|d| {
                                            d.exists
                                                && d.path.parent()
                                                    == Some(settings.vm_dir.as_path())
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
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.machine.disk_path)
                                        .desired_width(f32::INFINITY)
                                        .hint_text("existing.raw"),
                                );
                                ui.label(ui::dim(
                                    "Only images in the VM directory are listed; they are never truncated or recreated.",
                                ));
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
                            });
                        let spec = launcher::install_spec(
                            &std::path::PathBuf::from(&cli_label),
                            &settings.vm_dir,
                            settings.child_cwd(),
                            &state.machine,
                        );
                        ui.add_space(10.0);
                        ui.collapsing("Advanced: command preview", |ui| {
                            ui.label(
                                RichText::new(spec.command_line())
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
            ui.checkbox(
                &mut form.check_updates_on_startup,
                "Check for updates on startup",
            )
            .on_hover_text(
                "Asks the GitHub Releases API once when the manager opens \
                 (background thread; offline is silently fine)",
            );
            ui.checkbox(&mut form.animations_enabled, "Interface motion")
                .on_hover_text(
                    "Ambient scan, logo movement, status pulses and hover easing. Off uses an event-driven 1 FPS heartbeat.",
                );

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
            520.0,
            |ui| move_disk_body(ui, state, actions),
        ),
    };

    if closed {
        actions.push(Action::CloseModal);
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseModal);
    }
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
                        EditVmSection::BootMedia => edit_boot_media(ui, &mut state.form),
                        EditVmSection::NetworkDisplay => {
                            edit_network_display(ui, &mut state.form);
                        }
                        EditVmSection::Storage => edit_storage(ui, &mut state.form),
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

fn edit_panel_heading(ui: &mut egui::Ui, title: &str, detail: &str) {
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
    slider_row(ui, "MEMORY", |ui| {
        ui.add(
            egui::Slider::new(&mut form.memory_mib, 128..=control_api::MAX_MEMORY_MIB)
                .step_by(128.0)
                .suffix(" MiB"),
        );
    });
    ui.add_space(8.0);
    slider_row(ui, "vCPUs", |ui| {
        ui.add(egui::Slider::new(&mut form.vcpus, 1..=64));
    });
    ui.add_space(20.0);
    ui.label(ui::faint("DEVICE TRANSPORT"));
    ui.add_space(5.0);
    egui::ComboBox::from_id_salt("edit-transport")
        .selected_text(form.transport.to_string())
        .width(200.0)
        .show_ui(ui, |ui| {
            for transport in [VirtioTransport::Mmio, VirtioTransport::Pci] {
                ui.selectable_value(&mut form.transport, transport, transport.to_string());
            }
        });
    ui.add_space(6.0);
    ui.label(ui::faint(
        "PCI is the familiar desktop default; MMIO is useful for lean direct-boot setups.",
    ));
}

fn edit_boot_media(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
    use control_api::BootMode;

    edit_panel_heading(
        ui,
        "Boot & media",
        "Select how the guest starts and which installer media it sees.",
    );
    ui.horizontal(|ui| {
        ui.label(ui::faint("BOOT METHOD"));
        ui.add_space(8.0);
        egui::ComboBox::from_id_salt("edit-boot-mode")
            .selected_text(match form.boot_mode {
                BootMode::DirectLinux => "Direct Linux",
                BootMode::Uefi => "UEFI firmware",
            })
            .width(200.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut form.boot_mode, BootMode::DirectLinux, "Direct Linux");
                ui.selectable_value(&mut form.boot_mode, BootMode::Uefi, "UEFI firmware");
            });
    });
    ui.add_space(16.0);
    match form.boot_mode {
        BootMode::DirectLinux => {
            text_row(
                ui,
                "KERNEL",
                &mut form.kernel,
                "artifacts/bootstrap/vmlinuz",
            );
            ui.add_space(6.0);
            text_row(
                ui,
                "INITRAMFS",
                &mut form.initramfs,
                "artifacts/bootstrap/initrd.img (optional)",
            );
            ui.add_space(6.0);
            text_row(ui, "CMDLINE", &mut form.cmdline, "console=ttyS0 …");
        }
        BootMode::Uefi => {
            text_row(
                ui,
                "FIRMWARE",
                &mut form.firmware,
                "artifacts/firmware/CLOUDHV.fd",
            );
            ui.add_space(6.0);
            text_row(ui, "NVRAM", &mut form.nvram, "<vm>.nvram (optional)");
            ui.add_space(6.0);
            text_row(ui, "CD-ROM", &mut form.cdrom, "installer .iso (optional)");
        }
    }
}

fn edit_network_display(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
    use crate::editor::NetworkChoice;
    edit_panel_heading(
        ui,
        "Network & display",
        "Connect the guest and choose how its desktop is presented.",
    );
    ui.horizontal(|ui| {
        ui.label(ui::faint("NETWORK"));
        ui.add_space(8.0);
        egui::ComboBox::from_id_salt("edit-network")
            .selected_text(form.network.label())
            .width(200.0)
            .show_ui(ui, |ui| {
                for choice in NetworkChoice::ALL {
                    ui.selectable_value(&mut form.network, choice, choice.label());
                }
            });
    });
    if form.network == NetworkChoice::Tap {
        ui.add_space(8.0);
        text_row(ui, "INTERFACE", &mut form.interface, "entangled0");
    }
    if form.network != NetworkChoice::None {
        ui.add_space(8.0);
        text_row(
            ui,
            "MAC",
            &mut form.mac,
            "52:00:… (optional, derived from the name)",
        );
    }
    ui.add_space(22.0);
    ui.label(ui::faint("VIRTUAL DISPLAY"));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(&mut form.display_width).range(320..=7680));
        ui.label(ui::faint("×"));
        ui.add(egui::DragValue::new(&mut form.display_height).range(200..=4320));
        ui.add_space(12.0);
        ui.checkbox(&mut form.virgl, "VirGL 3D").on_hover_text(
            "Offer VIRTIO_GPU_F_VIRGL backed by the host virglrenderer \
             (Linux hosts; the run fails rather than silently booting 2D)",
        );
    });
}

fn edit_storage(ui: &mut egui::Ui, form: &mut crate::editor::EditForm) {
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
    ui.label(ui::faint("ATTACH AN EXISTING DISK IMAGE"));
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut form.add_disk)
                .desired_width(f32::INFINITY)
                .hint_text("path/to/image.raw"),
        );
        let path = form.add_disk.trim().to_string();
        if ui::ghost_button(ui, "Add disk", !path.is_empty(), theme::CYAN).clicked() {
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

    ui.label(ui::faint("DESTINATION DIRECTORY"));
    ui.add_enabled(
        !state.running,
        egui::TextEdit::singleline(&mut state.dest)
            .desired_width(f32::INFINITY)
            .hint_text(if cfg!(windows) {
                "E:\\vm-storage"
            } else {
                "/mnt/bigdrive/vms"
            }),
    );

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

fn option_card(
    ui: &mut egui::Ui,
    width: f32,
    title: &str,
    detail: &str,
    selected: bool,
    tint: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 78.0), egui::Sense::click());
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("option-hover"),
        response.hovered(),
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
        ui.label(RichText::new(title).size(15.0).color(theme::TEXT).strong());
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

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(78.0, 20.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(ui::faint(label));
            },
        );
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(f32::INFINITY)
                .hint_text(hint),
        );
    });
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
