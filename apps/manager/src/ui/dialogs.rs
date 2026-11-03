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
        update_check_status,
        ..
    } = app;
    let wsl_install_running = *wsl_install_running;

    let closed = match modal {
        Modal::None => false,
        Modal::Wizard(state) => {
            // The native Apple Silicon backend installs one thing — Ubuntu
            // ARM64 from a verified cloud image, unattended — so the wizard
            // offers exactly that there.
            let native_arm = cfg!(all(target_os = "macos", target_arch = "aarch64"))
                && state.machine.backend == Backend::Native;
            let cli_label = engine
                .as_ref()
                .map(|engine| engine.path.display().to_string())
                .unwrap_or_else(|_| "entangled".to_string());
            frame(ctx, "wizard", "Create a machine", 760.0, |ui| {
                ui::form_scope(ui);
                wizard_progress(ui, state.step);
                ui.add_space(18.0);

                // The step body scrolls, the progress bar above and the
                // buttons below stay put: "Advanced options" alone is taller
                // than a laptop screen, and an egui modal never grows a scroll
                // bar of its own — it would just run off the bottom of the window.
                let body_max = (ctx.screen_rect().height() - WIZARD_CHROME_H).max(160.0);
                egui::ScrollArea::vertical()
                    .id_salt("wizard-body")
                    .max_height(body_max)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        wizard_step(
                            ui,
                            state,
                            settings,
                            scan,
                            wsl_engine,
                            wsl_install_running,
                            native_arm,
                            &cli_label,
                            actions,
                        );
                    });

                ui.add_space(24.0);
                theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                        actions.push(Action::CloseModal);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if state.step == 0 {
                            // Nothing to continue with until the installer is
                            // known: an ISO read, or Debian chosen.
                            let ready = state.installer.is_some();
                            let button = ui::primary_button_enabled(ui, "Continue", ready);
                            if !ready {
                                button.on_hover_text("Choose an installer ISO first");
                            } else if button.clicked() {
                                state.step = 1;
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
                                if state.machine.family == launcher::GuestFamily::OtherLinux {
                                    "Create and boot ISO"
                                } else {
                                    "Create and install"
                                },
                                blocked.is_none(),
                            );
                            match &blocked {
                                Some(reason) => {
                                    button.on_hover_text(reason);
                                }
                                None if button.clicked() => actions.push(Action::SubmitWizard),
                                None => {}
                            }
                            if !native_arm && ui::secondary_button(ui, "Back", true).clicked() {
                                state.step = 0;
                                state.error = None;
                            }
                            ui.add_space(8.0);
                            ui.label(ui::faint(machine_summary(state)));
                        }
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
            settings_body(
                ui,
                form,
                engine,
                wsl_engine,
                wsl_install_running,
                update_check_status,
                actions,
            )
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
        Modal::EditVm(state) => {
            super::machine_settings::edit_vm_drawer(ctx, state, wsl_engine, actions)
        }
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
        Modal::Enable3d(state) => frame(
            ctx,
            "enable-3d",
            &format!("Add 3D graphics to {}", state.name),
            560.0,
            |ui| enable_3d_body(ui, state, actions),
        ),
        Modal::Import(state) => frame(ctx, "import", "Add an existing machine", 560.0, |ui| {
            import_body(ui, state, actions)
        }),
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
    update_check_status: &crate::update::CheckStatus,
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
    ui.label(ui::faint(format!("Entangled Desktop {}", crate::VERSION)));
    ui.horizontal_wrapped(|ui| {
        let checking = matches!(update_check_status, crate::update::CheckStatus::Checking);
        if ui::ghost_button(ui, "Check for updates", !checking, theme::TEXT).clicked() {
            actions.push(Action::CheckForUpdates);
        }
        match update_check_status {
            crate::update::CheckStatus::Idle => {}
            crate::update::CheckStatus::Checking => {
                ui.label(ui::dim("Checking for updates…"));
            }
            crate::update::CheckStatus::Current => {
                ui.label(ui::dim("You're up to date."));
            }
            crate::update::CheckStatus::Available(version) => {
                ui.label(ui::dim(format!("Version {version} is available.")));
                if ui::text_button(ui, "View release", theme::ACCENT).clicked() {
                    actions.push(Action::OpenReleasePage);
                }
            }
            crate::update::CheckStatus::Failed(error) => {
                ui.label(ui::dim("Couldn't check for updates. Try again."))
                    .on_hover_text(error);
            }
        }
    });

    ui.add_space(14.0);
    let advanced =
        egui::CollapsingHeader::new(RichText::new("Advanced").size(12.5).color(theme::TEXT_DIM))
            .id_salt("settings-advanced")
            .default_open(form.advanced_open);
    advanced.show(ui, |ui| {
        ui::form_scope(ui);
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
pub(super) fn entry_label(backend: Backend, wsl_engine: &wslengine::Status) -> String {
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
                    .min_size(egui::vec2(0.0, ui::BUTTON_H))
                    .vertical_align(Align::Center)
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
                    .min_size(egui::vec2(0.0, ui::BUTTON_H))
                    .vertical_align(Align::Center)
                    .hint_text("set up automatically"),
            );
        },
    );
}

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

/// *Add 3D graphics*: what it does and costs, the work, then the one thing
/// left to do inside the machine.
fn enable_3d_body(ui: &mut egui::Ui, state: &crate::app::Enable3dState, actions: &mut Vec<Action>) {
    use control_api::enable_3d::{DISC_LABEL, LINUX_COMMAND, WINDOWS_SCRIPT};
    let body = |ui: &mut egui::Ui, text: &str| {
        ui.label(RichText::new(text).color(theme::TEXT).size(13.0));
    };
    match &state.result {
        None => {
            if state.windows {
                body(
                    ui,
                    "Games, Vulkan, Direct3D and WebGL on this computer's graphics card. Entangled downloads its graphics driver, checks it, and puts it on a disc in the machine with an installer to run.",
                );
                ui.add_space(10.0);
                ui.label(ui::dim(
                    "Windows then shows a small \"Test Mode\" label in the corner, and games with anti-cheat may refuse to start. The driver is test-signed: Microsoft's signature needs a certificate this project does not have yet.",
                ));
            } else {
                body(
                    ui,
                    "A smoother desktop and apps, drawn by this computer's graphics card. Nothing is downloaded: Entangled turns 3D on for the machine and puts a small setup script on a disc in it, for you to run once.",
                );
                ui.add_space(10.0);
                ui.label(ui::dim(
                    "The machine gets at least 4 GiB of memory. This computer needs an up-to-date graphics driver; Health checks it.",
                ));
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if state.running {
                    ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
                    ui.label(ui::dim(if state.windows {
                        "Downloading and preparing the driver…"
                    } else {
                        "Preparing…"
                    }));
                } else {
                    if ui::primary_button(ui, "Add 3D graphics").clicked() {
                        actions.push(Action::SubmitEnable3d);
                    }
                    if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                        actions.push(Action::CloseModal);
                    }
                }
            });
        }
        Some(Ok(())) => {
            body(ui, "3D is on. One step is left, inside the machine:");
            ui.add_space(10.0);
            if state.windows {
                ui.label(ui::dim(format!(
                    "1. Start the machine.\n2. Open the {DISC_LABEL} drive in File Explorer and \
                     double-click \"{WINDOWS_SCRIPT}\" (it asks for administrator rights).\n\
                     3. Windows restarts by itself when it is done."
                )));
            } else {
                ui.label(ui::dim(
                    "Start the machine, open a terminal in it and run this line, then log out and back in:",
                ));
                ui.add_space(8.0);
                egui::Frame::new()
                    .fill(theme::INSET)
                    .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
                    .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(LINUX_COMMAND)
                                .monospace()
                                .size(12.0)
                                .color(theme::TEXT),
                        );
                    });
                ui.add_space(8.0);
                if ui::secondary_button(ui, "Copy the command", true).clicked() {
                    ui.ctx().copy_text(LINUX_COMMAND.to_string());
                }
            }
            ui.add_space(16.0);
            if ui::primary_button(ui, "Done").clicked() {
                actions.push(Action::CloseModal);
            }
        }
        Some(Err(error)) => {
            body(
                ui,
                "3D could not be added. Nothing in the machine was changed.",
            );
            ui.add_space(8.0);
            ui.label(RichText::new(error).color(theme::ERR).size(12.5));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if ui::secondary_button(ui, "Try again", true).clicked() {
                    actions.push(Action::SubmitEnable3d);
                }
                if ui::text_button(ui, "Close", theme::TEXT_DIM).clicked() {
                    actions.push(Action::CloseModal);
                }
            });
        }
    }
}

/// *Import…*: what the machine is, the name it gets here, and whether its
/// files move into the machines folder or stay where they are.
fn import_body(ui: &mut egui::Ui, state: &mut crate::app::ImportState, actions: &mut Vec<Action>) {
    let plan = &state.plan;
    let family = if plan.config.hyperv_enlightenments {
        launcher::GuestFamily::Windows
    } else {
        launcher::GuestFamily::OtherLinux
    };
    ui.horizontal(|ui| {
        paint_family_mark(ui, family, 44.0);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new(if plan.config.hyperv_enlightenments {
                    "Windows"
                } else {
                    "Linux"
                })
                .size(15.0)
                .color(theme::TEXT)
                .strong(),
            );
            let mut facts = vec![
                format!("{} GiB memory", plan.config.memory_mib / 1024),
                format!("{} processors", plan.config.vcpus),
            ];
            if plan.apparent_bytes > 0 {
                facts.push(format!("{} disk", format_bytes(plan.apparent_bytes)));
            }
            if plan.config.display.venus {
                facts.push("3D".into());
            }
            ui.label(ui::dim(facts.join(" · ")));
        });
    });
    ui.add_space(6.0);
    let home = plan
        .home()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    ui.label(ui::faint(format!("in {}", shorten_middle(&home, 70))));
    ui.add_space(16.0);

    ui::form_scope(ui);
    ui::form_row(
        ui,
        "NAME",
        "What the machine is called here. Letters, digits, '-', '_' and '.', starting \
         with a letter or digit. Its disk keeps its own file name.",
        |ui, field_w| {
            ui.add_enabled(
                !state.running,
                egui::TextEdit::singleline(&mut state.name).desired_width(field_w),
            );
        },
    );
    ui.add_space(14.0);

    if plan.movable.is_empty() {
        ui.label(ui::dim(
            "Its disk is already in the machines folder; only the profile is added.",
        ));
    } else {
        let gap = 12.0;
        let width = (ui.available_width() - gap) / 2.0;
        let move_detail = if !plan.collisions.is_empty() {
            "A file of the same name is already in the machines folder.".to_string()
        } else if plan.same_volume {
            "Instant — the same drive, nothing is copied.".to_string()
        } else {
            format!(
                "Copies {} of data and checks it, then removes the originals.",
                format_bytes(plan.data_bytes)
            )
        };
        let can_move = plan.can_move();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            if option_card(
                ui,
                width,
                "Move it into the machines folder",
                &move_detail,
                state.move_files && can_move,
                can_move && !state.running,
                theme::ACCENT,
            )
            .clicked()
            {
                state.move_files = true;
            }
            if option_card(
                ui,
                width,
                "Leave its files where they are",
                "It runs from that folder; deleting it here leaves them alone.",
                !state.move_files || !can_move,
                !state.running,
                theme::ACCENT,
            )
            .clicked()
            {
                state.move_files = false;
            }
        });
    }

    if let Some((done, total)) = state.progress {
        ui.add_space(14.0);
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

    ui.add_space(18.0);
    ui.horizontal(|ui| {
        if state.running {
            ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
            ui.label(ui::dim("Moving its files…"));
        } else {
            if ui::primary_button(ui, "Add machine").clicked() {
                actions.push(Action::SubmitImport);
            }
            if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                actions.push(Action::CloseModal);
            }
        }
    });
}

/// The wizard's "Advanced options": every setting the presets and the
/// recognised ISO choose for you, by hand — exact cores and memory, the disk,
/// how the installer runs, Windows' drivers and where the machine runs.
#[allow(clippy::too_many_arguments)]
fn advanced_options(
    ui: &mut egui::Ui,
    state: &mut crate::app::WizardState,
    settings: &crate::settings::Settings,
    scan: &crate::discovery::Scan,
    wsl_engine: &wslengine::Status,
    wsl_install_running: bool,
    native_arm: bool,
    actions: &mut Vec<Action>,
) {
    ui::form_scope(ui);
    ui.label(ui::caption("Hardware"));
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
                    // The ARM64 desktop image needs 4 GiB to boot GNOME.
                    if native_arm { 4096 } else { 512 }..=control_api::MAX_MEMORY_MIB,
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
            let maximum = if native_arm {
                std::thread::available_parallelism().map_or(1, |count| count.get().min(64) as u32)
            } else {
                16
            };
            ui.add(egui::Slider::new(&mut state.machine.vcpus, 1..=maximum).suffix(" vCPU"));
        },
    );

    ui.add_space(18.0);
    ui.label(ui::caption("Disk"));
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
            !native_arm,
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

    let family = state.machine.family;
    ui.add_space(18.0);
    ui.label(ui::caption("Setup"));
    ui.add_space(10.0);
    if family == launcher::GuestFamily::Debian {
        ui::form_row(
            ui,
            "INSTALLER",
            "Which Debian installer runs: the graphical one, the fast \
             keyboard-driven one, or the official netinst disc image — each \
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
    if family == launcher::GuestFamily::Windows {
        if ui::path_row(
            ui,
            "DRIVERS",
            "The virtio-win drivers: Windows cannot see this machine's disk \
             or network without them, and they are installed during setup. \
             Leave empty and Entangled downloads the release it was tested \
             with, checked against a digest built into it. Choose an ISO \
             only to use another version.",
            &mut state.machine.drivers_path,
            "downloaded and verified automatically",
            true,
        ) {
            actions.push(Action::PickPath(PickTarget::WizardDrivers));
        }
        ui.add_space(6.0);
        ui.checkbox(
            &mut state.machine.shared_folders,
            "Show shared folders as drives (installs WinFsp)",
        );
    }
    if native_arm {
        ui.label(ui::dim(
            "Ubuntu is installed automatically, in the background.",
        ));
    } else if family != launcher::GuestFamily::OtherLinux {
        if family != launcher::GuestFamily::Windows {
            let mut manual = !state.machine.automated;
            if ui
                .checkbox(&mut manual, "Manual installation")
                .on_hover_text(
                    "Answer the installer's questions yourself in the machine's window, \
                     instead of Entangled answering them",
                )
                .changed()
            {
                state.machine.automated = !manual;
            }
        }
        ui.checkbox(
            &mut state.machine.headless,
            "Install in the background (no window; follow it on the machine's page)",
        );
    } else {
        ui.label(ui::dim(
            "Manual install. After installing, power off and eject the ISO under \
             Settings > Startup.",
        ));
    }

    let choices = Backend::ALL
        .into_iter()
        .filter(|backend| backend.available_on_host())
        .count();
    if choices >= 2 {
        ui.add_space(18.0);
        ui.label(ui::caption("Runs on"));
        ui.add_space(10.0);
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
}

/// One click sizes a machine. The numbers are the ones a desktop guest is
/// comfortable with at each step; "More options" sets any other pair.
pub(super) struct PowerPreset {
    pub(super) name: &'static str,
    pub(super) vcpus: u32,
    pub(super) memory_mib: u64,
    pub(super) hint: &'static str,
}

pub(super) const POWER_PRESETS: [PowerPreset; 3] = [
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
/// an open dot for the one to come, joined by a short rule.
fn wizard_progress(ui: &mut egui::Ui, current: usize) {
    const STEPS: [&str; 2] = ["Installer", "Name & size"];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        for (index, label) in STEPS.into_iter().enumerate() {
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
            if index + 1 < STEPS.len() {
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
pub(super) fn option_card(
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

// `text_row` and `slider_row` used to live here, each with its own hard-coded
// 78 px label column. They are gone: every labelled row in the product now goes
// through `ui::form_row` / `ui::path_row`, which is what keeps the fields in one
// straight column and the explanations in tooltips.

/// Everything in the wizard that is not its scrolling step body: the modal's
/// margins, title and rule, the progress bar, the footer with its buttons, and
/// some air between the dialog and the window edge.
const WIZARD_CHROME_H: f32 = 290.0;

/// The current wizard step's content, between the progress bar and the footer.
#[allow(clippy::too_many_arguments)]
fn wizard_step(
    ui: &mut egui::Ui,
    state: &mut crate::app::WizardState,
    settings: &crate::settings::Settings,
    scan: &crate::discovery::Scan,
    wsl_engine: &wslengine::Status,
    wsl_install_running: bool,
    native_arm: bool,
    cli_label: &str,
    actions: &mut Vec<Action>,
) {
    if state.step == 0 {
        installer_step(ui, state, actions);
    } else {
        setup_step(
            ui,
            state,
            settings,
            scan,
            wsl_engine,
            wsl_install_running,
            native_arm,
            cli_label,
            actions,
        );
    }
    if let Some(error) = &state.error {
        ui.add_space(10.0);
        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
    }
}

/// Step 0: the person gives the ISO and the image says what it is. Getting
/// the installer some other way — Debian, which downloads itself — is the
/// smaller choice under it.
fn installer_step(
    ui: &mut egui::Ui,
    state: &mut crate::app::WizardState,
    actions: &mut Vec<Action>,
) {
    wizard_heading(
        ui,
        "Choose an installer",
        "Give Entangled the ISO of the system you want. It recognises what is on it \
         and sets everything up for you.",
    );
    ui.add_space(18.0);
    match &state.installer {
        Some(installer) if installer.from_iso => {
            installer_card(ui, installer, Some(&state.machine.iso_path), actions);
        }
        Some(installer) => installer_card(ui, installer, None, actions),
        None => {
            if drop_zone(ui).clicked() {
                actions.push(Action::PickPath(PickTarget::WizardIso));
            }
        }
    }
    if let Some(note) = state.installer.as_ref().and_then(|i| i.note) {
        ui.add_space(8.0);
        ui.label(RichText::new(note).size(12.5).color(theme::TEXT_DIM));
    }

    ui.add_space(26.0);
    ui.horizontal(|ui| {
        let width = ui.available_width();
        let label = "No ISO yet?";
        let galley = ui.painter().layout_no_wrap(
            label.to_string(),
            egui::FontId::proportional(12.0),
            theme::TEXT_FAINT,
        );
        let side = ((width - galley.size().x - 24.0) / 2.0).max(0.0);
        let (left, _) = ui.allocate_exact_size(Vec2::new(side, 16.0), egui::Sense::hover());
        ui.painter().hline(
            left.x_range(),
            left.center().y,
            egui::Stroke::new(1.0_f32, theme::DIVIDER),
        );
        ui.label(RichText::new(label).size(12.0).color(theme::TEXT_FAINT));
        let (right, _) = ui.allocate_exact_size(Vec2::new(side, 16.0), egui::Sense::hover());
        ui.painter().hline(
            right.x_range(),
            right.center().y,
            egui::Stroke::new(1.0_f32, theme::DIVIDER),
        );
    });
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Install").size(13.0).color(theme::TEXT_DIM));
        let debian = state
            .installer
            .as_ref()
            .is_some_and(|i| i.family == launcher::GuestFamily::Debian);
        if choice_chip(ui, "Debian", debian)
            .on_hover_text(
                "Debian's verified network installer: nothing to download first, the \
                 system comes over the internet while it installs.",
            )
            .clicked()
        {
            actions.push(Action::WizardInstaller(
                launcher::Installer::debian_download(),
            ));
        }
        ui.label(
            RichText::new("from the internet — Entangled downloads and checks it.")
                .size(13.0)
                .color(theme::TEXT_DIM),
        );
    });
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(
            RichText::new("Windows can't be downloaded for you —")
                .size(12.0)
                .color(theme::TEXT_FAINT),
        );
        ui.hyperlink_to(
            RichText::new("get the ISO from Microsoft")
                .size(12.0)
                .color(theme::ACCENT),
            "https://www.microsoft.com/software-download/windows11",
        );
        ui.label(
            RichText::new("(it is free).")
                .size(12.0)
                .color(theme::TEXT_FAINT),
        );
    });
}

/// The big target an ISO is dropped on or chosen through.
fn drop_zone(ui: &mut egui::Ui) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 180.0), egui::Sense::click());
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("hover"),
        response.hovered(),
        0.12,
    );
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(theme::CARD_RADIUS);
    painter.rect_filled(rect, radius, theme::INSET);
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(
            1.5_f32,
            theme::mix(theme::STROKE_STRONG, theme::ACCENT_DEEP, hover),
        ),
        egui::StrokeKind::Inside,
    );
    let disc = egui::pos2(rect.center().x, rect.top() + 48.0);
    painter.circle_stroke(disc, 17.0, egui::Stroke::new(1.5_f32, theme::ACCENT));
    painter.circle_stroke(disc, 4.5, egui::Stroke::new(1.5_f32, theme::ACCENT));
    let line = |y: f32, text: &str, size: f32, color: egui::Color32| {
        painter.text(
            egui::pos2(rect.center().x, rect.top() + y),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(size),
            color,
        );
    };
    line(92.0, "Drop an ISO file here", 16.0, theme::TEXT);
    line(
        118.0,
        "or click to choose one from this computer",
        13.0,
        theme::ACCENT,
    );
    line(
        146.0,
        "Ubuntu, Fedora, Windows 10 / 11, or any x86-64 Linux installer",
        12.0,
        theme::TEXT_FAINT,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The colour and letter that stand for a system, on the cards and the setup
/// header. Painted, never a logo: no other project's mark is ours to draw.
fn family_mark(family: launcher::GuestFamily) -> (egui::Color32, &'static str) {
    use launcher::GuestFamily;
    match family {
        GuestFamily::Ubuntu => (theme::WARN, "U"),
        GuestFamily::Debian => (theme::ERR, "D"),
        GuestFamily::Fedora => (theme::ACCENT, "F"),
        GuestFamily::Windows => (theme::OK, "W"),
        GuestFamily::OtherLinux => (theme::IDLE, "?"),
    }
}

fn paint_family_mark(ui: &mut egui::Ui, family: launcher::GuestFamily, size: f32) {
    let (tint, letter) = family_mark(family);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(10), tint);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::proportional(size * 0.42),
        theme::BG_DEEP,
    );
}

/// A small rounded label: "Installs automatically", "Experimental".
fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    let font = egui::FontId::proportional(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let (rect, _) =
        ui.allocate_exact_size(galley.size() + Vec2::new(14.0, 6.0), egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(255),
        color.gamma_multiply(0.16),
    );
    ui.painter().galley(
        rect.center() - galley.size() * 0.5,
        galley,
        egui::Color32::TRANSPARENT,
    );
}

/// What step 0 settled, with a way to change it.
fn installer_card(
    ui: &mut egui::Ui,
    installer: &launcher::Installer,
    iso: Option<&str>,
    actions: &mut Vec<Action>,
) {
    egui::Frame::new()
        .fill(theme::SELECTED)
        .stroke(egui::Stroke::new(1.0_f32, theme::ACCENT))
        .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                paint_family_mark(ui, installer.family, 44.0);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&installer.title)
                                .size(15.0)
                                .color(theme::TEXT)
                                .strong(),
                        );
                        if installer.automatic() {
                            badge(ui, "Installs automatically", theme::ACCENT);
                        } else {
                            badge(ui, "Manual install", theme::WARN);
                        }
                    });
                    let detail = match iso {
                        Some(path) => std::path::Path::new(path)
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.to_string()),
                        None => "Downloaded and verified by Entangled while it installs".into(),
                    };
                    ui.label(RichText::new(detail).size(12.0).color(theme::TEXT_FAINT));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let change = if iso.is_some() {
                        "Change"
                    } else {
                        "Use an ISO instead"
                    };
                    if ui::text_button(ui, change, theme::ACCENT).clicked() {
                        actions.push(Action::PickPath(PickTarget::WizardIso));
                    }
                });
            });
        });
}

/// An on/off switch, the size of the line beside it.
fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(44.0, 24.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let value = *on;
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, value, label));
    let t = theme::animate_bool(ui.ctx(), response.id.with("switch"), value, 0.12);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(12),
        theme::mix(theme::STROKE_STRONG, theme::ACCENT_DEEP, t),
    );
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    painter.circle_filled(egui::pos2(x, rect.center().y), 9.0, theme::TEXT);
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The 3D switch in its card: a title, a line of what it does, and — once it
/// is on — what it costs, said plainly.
fn gpu_switch_card(
    ui: &mut egui::Ui,
    on: &mut bool,
    experimental: bool,
    detail: &str,
    when_on: Option<&str>,
) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("3D graphics")
                                .size(14.0)
                                .color(theme::TEXT)
                                .strong(),
                        );
                        if experimental {
                            badge(ui, "Experimental", theme::WARN);
                        }
                    });
                    ui.label(RichText::new(detail).size(12.5).color(theme::TEXT_DIM));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    switch(ui, on, "3D graphics");
                });
            });
            if let (true, Some(text)) = (*on, when_on) {
                ui.add_space(10.0);
                theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());
                ui.add_space(10.0);
                ui.label(RichText::new(text).size(12.5).color(theme::TEXT_DIM));
            }
        });
}

/// The footer's one line about the machine about to be made.
fn machine_summary(state: &crate::app::WizardState) -> String {
    let machine = &state.machine;
    let mut text = format!(
        "{} · {} cores · {}",
        state
            .installer
            .as_ref()
            .map_or(machine.family.label(), |i| i.family.label()),
        machine.vcpus,
        format_bytes(machine.memory_mib * 1024 * 1024)
    );
    if machine.disk_mode == launcher::DiskMode::CreateNew {
        text.push_str(&format!(" · {} GiB disk", machine.disk_gib));
    }
    if gpu_chosen(machine) {
        text.push_str(" · 3D");
    }
    text
}

/// Whether the machine will be made with 3D: the Windows package, or the
/// Ubuntu GPU desktop (which needs the automatic install).
fn gpu_chosen(machine: &launcher::NewMachine) -> bool {
    match machine.family {
        launcher::GuestFamily::Windows => machine.gpu3d_download || !machine.gpu3d_path.is_empty(),
        launcher::GuestFamily::Ubuntu => machine.venus && machine.automated,
        _ => false,
    }
}

/// Step 1: what the machine is called, how big it is, 3D, and everything
/// else under "Advanced options".
#[allow(clippy::too_many_arguments)]
fn setup_step(
    ui: &mut egui::Ui,
    state: &mut crate::app::WizardState,
    settings: &crate::settings::Settings,
    scan: &crate::discovery::Scan,
    wsl_engine: &wslengine::Status,
    wsl_install_running: bool,
    native_arm: bool,
    cli_label: &str,
    actions: &mut Vec<Action>,
) {
    let family = state.machine.family;
    let (title, subtitle) = match &state.installer {
        Some(installer) => {
            let how = if installer.automatic() {
                "installs automatically"
            } else {
                "you install it in the machine's window"
            };
            let from = if installer.from_iso {
                std::path::Path::new(state.machine.iso_path.trim())
                    .file_name()
                    .map(|n| format!("From {}", n.to_string_lossy()))
                    .unwrap_or_default()
            } else {
                "Downloaded and verified by Entangled".to_string()
            };
            (installer.title.clone(), format!("{from} · {how}"))
        }
        None => (family.label().to_string(), String::new()),
    };
    ui.horizontal(|ui| {
        paint_family_mark(ui, family, 44.0);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width() - 240.0);
            ui.add(
                egui::Label::new(RichText::new(title).size(17.0).color(theme::TEXT).strong())
                    .truncate(),
            );
            ui.add(
                egui::Label::new(RichText::new(subtitle).size(12.5).color(theme::TEXT_DIM))
                    .truncate(),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Name").size(12.0).color(theme::TEXT_DIM));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut state.machine.name)
                        .desired_width(220.0)
                        .min_size(Vec2::new(220.0, 30.0))
                        .vertical_align(Align::Center)
                        .hint_text("my-machine"),
                );
                if response.changed() {
                    state.name_suggested = false;
                }
                response.on_hover_text(
                    "Names the machine, its disk file and the guest's own hostname.",
                );
            });
        });
    });

    ui.add_space(22.0);
    ui.label(
        RichText::new("How powerful?")
            .size(14.0)
            .color(theme::TEXT)
            .strong(),
    );
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
    if !POWER_PRESETS
        .iter()
        .any(|p| p.vcpus == state.machine.vcpus && p.memory_mib == state.machine.memory_mib)
    {
        ui.add_space(6.0);
        ui.label(ui::faint(format!(
            "Custom: {} cores · {} — set under Advanced options",
            state.machine.vcpus,
            format_bytes(state.machine.memory_mib * 1024 * 1024)
        )));
    }
    if state.machine.disk_mode == launcher::DiskMode::CreateNew {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Disk size")
                    .size(14.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.add_space(12.0);
            // Windows refuses less than 64 GiB to be comfortable in
            // (`install windows`' own default), so it is not offered less.
            let floor = if family == launcher::GuestFamily::Windows {
                64
            } else {
                0
            };
            for gib in DISK_CHOICES_GIB.into_iter().filter(|&g| g >= floor) {
                if choice_chip(ui, &format!("{gib} GiB"), state.machine.disk_gib == gib).clicked() {
                    state.machine.disk_gib = gib;
                }
            }
            ui.add_space(8.0);
            ui.label(ui::faint("Only grows as files are saved."));
        });
    }

    ui.add_space(20.0);
    let ubuntu_desktop = family == launcher::GuestFamily::Ubuntu
        && state.installer.as_ref().is_some_and(|i| i.desktop)
        && state.machine.automated
        && !native_arm;
    if family == launcher::GuestFamily::Windows {
        gpu_switch_card(
            ui,
            &mut state.machine.gpu3d_download,
            true,
            "Games, Vulkan, Direct3D and WebGL on this computer's graphics card. You can \
             also add it later from the machine's page.",
            Some(
                "Entangled downloads its graphics driver during setup and checks it for \
                 you. Windows then shows a small \"Test Mode\" label in the corner, and \
                 games with anti-cheat may refuse to start.",
            ),
        );
    } else if ubuntu_desktop {
        gpu_switch_card(
            ui,
            &mut state.machine.venus,
            false,
            "A smoother desktop and apps, drawn by this computer's graphics card. Nothing \
             extra to download. You can also add it later.",
            Some(
                "Needs an up-to-date graphics driver on this computer; Health checks it. \
                 The machine gets at least 4 GiB of memory.",
            ),
        );
    } else if !native_arm {
        ui.label(ui::faint(
            "3D graphics can be added after the install, from the machine's page.",
        ));
    }

    ui.add_space(14.0);
    let toggle = if state.more {
        "Hide advanced options"
    } else {
        "Advanced options — exact cores and memory, disk file, installer, where it runs"
    };
    if ui::text_button(ui, toggle, theme::ACCENT).clicked() {
        state.more = !state.more;
    }
    if state.more {
        ui.add_space(10.0);
        advanced_options(
            ui,
            state,
            settings,
            scan,
            wsl_engine,
            wsl_install_running,
            native_arm,
            actions,
        );
    }

    // A machine whose files the chosen backend cannot see is refused here,
    // before the button is pressed, with the fix.
    let runner = launcher::Runner::new(
        state.machine.backend,
        std::path::PathBuf::from(&cli_label),
        settings,
    );
    if let Err(message) = launcher::install_spec(
        &runner,
        &settings.vm_dir,
        settings.child_cwd(),
        &state.machine,
    ) {
        ui.add_space(10.0);
        ui.label(RichText::new(message).color(theme::ERR).size(12.5));
    }
    // …and neither is a backend whose engine is not there: the same refusal
    // `ManagerApp::backend_block` gives the Start button, with the install
    // offer attached.
    if state.machine.backend == Backend::Wsl && wsl_engine.refusal().is_some() && !state.more {
        ui.add_space(6.0);
        wsl_setup_note(ui, wsl_engine, wsl_install_running, 0.0, actions);
    }
}

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
