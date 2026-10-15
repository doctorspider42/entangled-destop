//! One machine's own page: its screen, what it is doing right now, how to
//! reach it, and its setup as plain-language tiles that open the editor on the
//! matching section.
//!
//! Everything a machine can do lives here — the All machines page only offers
//! the one next step a tile has room for.

use egui::{Align, Layout, Rect, RichText, Sense, Vec2};

use crate::app::{Action, EditVmSection, ManagerApp, Status, View};
use crate::discovery::{format_bytes, VmEntry};
use crate::theme;
use crate::ui::cards::{self, Corners};
use crate::ui::{self, Icon};

/// Gap between the page's columns and panels.
const GAP: f32 = 20.0;
/// Page width from which the screen and the two panels sit side by side.
const SIDE_BY_SIDE: f32 = 740.0;

pub fn show(ctx: &egui::Context, app: &ManagerApp, name: &str, actions: &mut Vec<Action>) {
    let time = theme::animation_time(ctx);
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(cards::PAGE_X, 28)),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    cards::banners(ui, app, actions);
                    match app.vm(name) {
                        Some(vm) => page(ui, app, vm, time, actions),
                        None if app.pending.iter().any(|p| p.name == name) => {
                            installing_page(ui, app, name, time, actions)
                        }
                        None => gone(ui, name, actions),
                    }
                });
        });
}

fn page(ui: &mut egui::Ui, app: &ManagerApp, vm: &VmEntry, time: f64, actions: &mut Vec<Action>) {
    let status = app.status_of(&vm.name);
    let missing_disk = vm.disks.iter().any(|d| !d.exists);

    header(ui, app, vm, status, missing_disk, time, actions);
    ui.add_space(18.0);
    notices(ui, app, vm, status, missing_disk);

    // Screen on the left, the two panels on the right; stacked when narrow.
    let width = ui.available_width();
    let side_w = if width > SIDE_BY_SIDE {
        (width * 0.36).clamp(280.0, 400.0)
    } else {
        width
    };
    let screen_w = if width > SIDE_BY_SIDE {
        width - side_w - GAP
    } else {
        width
    };
    let screen_h = (screen_w * 9.0 / 16.0).floor();
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        let (screen, response) =
            ui.allocate_exact_size(Vec2::new(screen_w, screen_h), Sense::hover());
        cards::screen_well(ui, app, &vm.name, status, screen, Corners::All);
        ui.painter().rect_stroke(
            screen,
            egui::CornerRadius::same(theme::CARD_RADIUS),
            egui::Stroke::new(1.0_f32, theme::STROKE),
            egui::StrokeKind::Inside,
        );
        if status == Status::Running {
            response.on_hover_text("The screen, a few seconds ago");
        }
        if width > SIDE_BY_SIDE {
            ui.allocate_ui_with_layout(
                Vec2::new(side_w, screen_h),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = GAP;
                    now_panel(ui, app, vm, status);
                    connect_panel(ui, app, vm, status, actions);
                },
            );
        }
    });
    if width <= SIDE_BY_SIDE {
        ui.add_space(GAP);
        now_panel(ui, app, vm, status);
        ui.add_space(GAP);
        connect_panel(ui, app, vm, status, actions);
    }

    ui.add_space(30.0);
    setup(ui, app, vm, status, actions);
    ui.add_space(24.0);
}

/// Name, state, shape, and the actions this state offers.
fn header(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    status: Status,
    missing_disk: bool,
    time: f64,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&vm.name)
                        .size(26.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                ui.add_space(6.0);
                ui::status_pill(ui, status, &ui::status_line(app, &vm.name), time);
            });
            ui.add_space(2.0);
            ui.label(
                RichText::new(cards::spec_line(app, vm))
                    .size(13.5)
                    .color(theme::TEXT_DIM),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            more_menu(ui, app, vm, status, actions);
            lifecycle_buttons(ui, app, vm, status, missing_disk, actions);
        });
    });
}

/// The buttons for this state, laid out right to left (so the primary one,
/// added first, ends up nearest the More menu).
fn lifecycle_buttons(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    status: Status,
    missing_disk: bool,
    actions: &mut Vec<Action>,
) {
    match status {
        Status::Stopped => {
            if !missing_disk && ui::primary_button(ui, "Start").clicked() {
                actions.push(Action::Start(vm.name.clone()));
            }
        }
        // The third resting state (ADR-0006). Resume is the primary action,
        // and only bright when the saved session could really go back — the
        // reasons it could not are on the hover, not in a toast after a click.
        Status::Suspended => {
            let verdict = app
                .snapshot_for(&vm.name)
                .map(|row| app.snapshot_verdict(row))
                .unwrap_or_default();
            if ui::secondary_button(ui, "Start fresh…", !missing_disk)
                .on_hover_text(
                    "Boot the machine from scratch. The saved session cannot survive \
                     that — the guest writes to the disk it was pinned to — so it is \
                     thrown away first, and you are asked before anything happens.",
                )
                .clicked()
            {
                actions.push(Action::AskDiscardSnapshot(vm.name.clone()));
            }
            let response = ui::primary_button_enabled(ui, "Resume", verdict.resumable());
            let response = if verdict.resumable() {
                response.on_hover_text(
                    "Put the machine back exactly as you left it — the same programs, \
                     the same windows.",
                )
            } else {
                response.on_hover_text(verdict.blocked.join("\n\n"))
            };
            if response.clicked() {
                actions.push(Action::ResumeVm(vm.name.clone()));
            }
        }
        Status::Running => cards::running_controls(ui, app, vm, true, actions),
        Status::Stopping => {
            if ui::ghost_button(ui, "Force stop", true, theme::ERR)
                .on_hover_text("Shutdown was requested; pull the plug instead")
                .clicked()
            {
                actions.push(Action::Stop(vm.name.clone()));
            }
        }
        Status::Installing => {
            if ui::ghost_button(ui, "Cancel install", true, theme::ERR).clicked() {
                actions.push(Action::Stop(vm.name.clone()));
            }
            activity_button(ui, app, &vm.name, actions);
        }
        // One-way, and nothing to press: the machine is being written to a
        // file and stops when it has.
        Status::Suspending => activity_button(ui, app, &vm.name, actions),
    }
}

fn activity_button(ui: &mut egui::Ui, app: &ManagerApp, name: &str, actions: &mut Vec<Action>) {
    if let Some(task) = app.supervisor.tasks().iter().rev().find(|t| t.vm == name) {
        if ui::secondary_button(ui, "View activity", true).clicked() {
            actions.push(Action::SelectLog(task.id));
        }
    }
}

/// Everything else a machine can do, in one menu: the rarer lifecycle moves,
/// the copy-this helpers and the file operations.
fn more_menu(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    status: Status,
    actions: &mut Vec<Action>,
) {
    let path = vm.profile_path.display().to_string();
    let response = ui::icon_button(ui, Icon::More, "More actions");
    egui::Popup::menu(&response).show(|ui| {
        ui.set_min_width(220.0);
        if status == Status::Running {
            let controllable = app.has_control(&vm.name);
            if ui
                .add_enabled(controllable, egui::Button::new("Restart"))
                .on_hover_text("Reboot the machine in place, as the guest's own Restart does")
                .clicked()
            {
                actions.push(Action::Reset(vm.name.clone()));
            }
            if ui
                .add_enabled(controllable, egui::Button::new("Suspend"))
                .on_hover_text(
                    "Write the whole machine to a file and stop it. Opening it again \
                     puts you back exactly here.",
                )
                .clicked()
            {
                actions.push(Action::Suspend(vm.name.clone()));
            }
            if ui
                .button(RichText::new("Force stop").color(theme::ERR))
                .on_hover_text(
                    "Power the machine off now, like pulling the plug. Anything the \
                     guest had not written to disk is lost.",
                )
                .clicked()
            {
                actions.push(Action::Stop(vm.name.clone()));
            }
            ui.separator();
        }
        if let Some(task) = app
            .supervisor
            .tasks()
            .iter()
            .rev()
            .find(|t| t.vm == vm.name)
        {
            if ui.button("Open activity log").clicked() {
                actions.push(Action::SelectLog(task.id));
            }
        }
        if ui.button("Copy profile path").clicked() {
            actions.push(Action::CopyProfilePath(path.clone()));
        }
        let mut autostart = app.settings.autostart.contains(&vm.name);
        if ui
            .checkbox(&mut autostart, "Start when the manager opens")
            .on_hover_text(
                "Boots this machine (or resumes its saved session) every time the \
                 manager starts. A machine already running is left alone.",
            )
            .changed()
        {
            actions.push(Action::ToggleAutostart(vm.name.clone()));
        }
        ui.separator();
        if app.snapshot_for(&vm.name).is_some()
            && ui
                .add_enabled(
                    status == Status::Suspended,
                    egui::Button::new("Forget the saved session"),
                )
                .on_hover_text(
                    "Delete the snapshot and leave the machine stopped. Its disks and \
                     settings are untouched.",
                )
                .clicked()
        {
            if let Some(row) = app.snapshot_for(&vm.name) {
                actions.push(Action::AskDeleteSnapshot(row.path.clone()));
            }
        }
        if ui
            .add_enabled(status == Status::Stopped, egui::Button::new("Rename…"))
            .on_hover_text(
                "A new name for the machine, and for its profile and the disks and \
                 UEFI store named after it.",
            )
            .clicked()
        {
            actions.push(Action::AskRenameVm(vm.profile_path.clone()));
        }
        let at_rest = matches!(status, Status::Stopped | Status::Suspended);
        if ui
            .add_enabled(at_rest, egui::Button::new("Clone…"))
            .on_hover_text(
                "A second machine from this one: its disks copied (only the data they \
                 hold), its UEFI boot entries, its settings, and a network address of \
                 its own.",
            )
            .clicked()
        {
            actions.push(Action::AskCloneVm(vm.name.clone()));
        }
        if ui
            .add_enabled(
                at_rest,
                egui::Button::new(RichText::new("Delete machine…").color(theme::ERR)),
            )
            .clicked()
        {
            actions.push(Action::AskDelete(vm.name.clone()));
        }
    });
}

/// Persistent problems with this machine, each with the reason in words.
fn notices(ui: &mut egui::Ui, app: &ManagerApp, vm: &VmEntry, status: Status, missing_disk: bool) {
    let mut any = false;
    if missing_disk {
        any = true;
        let missing: Vec<String> = vm
            .disks
            .iter()
            .filter(|d| !d.exists)
            .map(|d| d.declared.display().to_string())
            .collect();
        ui::banner(
            ui,
            theme::ERR,
            &format!("A disk image is missing: {}", missing.join(", ")),
            None,
        );
    }
    let twins = app.name_count(&vm.name);
    if twins > 1 {
        any = true;
        ui::banner(
            ui,
            theme::WARN,
            &format!(
                "{twins} profiles share this name, so they share its status, logs and \
                 saved session. Rename one from its … menu."
            ),
            None,
        );
    }
    if status == Status::Suspended {
        if let Some(row) = app.snapshot_for(&vm.name) {
            let verdict = app.snapshot_verdict(row);
            if !verdict.resumable() {
                any = true;
                ui::banner(
                    ui,
                    theme::WARN,
                    &format!(
                        "The saved session cannot resume here: {}",
                        verdict.blocked.join(" ")
                    ),
                    None,
                );
            }
        }
    }
    if any {
        ui.add_space(14.0);
    }
}

/// A titled panel on the card surface.
fn panel(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(egui::Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(ui::caption(title));
            ui.add_space(12.0);
            add(ui);
        });
}

/// What the machine is doing right now — or, at rest, what it last was.
fn now_panel(ui: &mut egui::Ui, app: &ManagerApp, vm: &VmEntry, status: Status) {
    let title = if matches!(status, Status::Running | Status::Stopping) {
        "Right now"
    } else {
        "At rest"
    };
    panel(ui, title, |ui| {
        let width = ui.available_width();
        let stats = app.stats.vms.get(&vm.name);
        if matches!(
            status,
            Status::Running | Status::Stopping | Status::Installing
        ) {
            let cpu = stats.and_then(|s| s.cpu_percent);
            row_value(ui, "Processor", cpu.map(|c| format!("{c:.0}%")));
            if let Some(stats) = stats {
                if stats.history.len() >= 2 {
                    ui::sparkline(ui, &stats.history, Vec2::new(width, 30.0), theme::ACCENT);
                }
            }
            ui.add_space(8.0);
            let memory = vm.memory_mib * 1024 * 1024;
            let rss = stats.and_then(|s| s.rss_bytes);
            row_value(ui, "Memory", rss.map(|r| cards::of(r, memory)));
            ui::meter(
                ui,
                rss.map(|r| r as f32 / memory.max(1) as f32).unwrap_or(0.0),
                width,
                theme::ACCENT,
            );
            ui.add_space(8.0);
        } else if status == Status::Suspending {
            // No progress to report — the engine writes the memory in one pass
            // and says nothing until it is done — so the honest thing is the
            // time it has been going. A bar filling at a guessed rate would be
            // worse than a number that is simply true.
            let elapsed = app
                .supervisor
                .active_task(&vm.name)
                .and_then(|task| task.suspending_for())
                .unwrap_or_default();
            row_value(
                ui,
                "Writing memory to disk",
                Some(format!("{:.0} s", elapsed.as_secs_f32())),
            );
            ui.add_space(8.0);
        } else if status == Status::Suspended {
            if let Some(row) = app.snapshot_for(&vm.name) {
                let when = match &row.facts {
                    Ok(facts) => vm_snapshot::meta::describe_age(
                        facts.created_unix,
                        vm_snapshot::meta::now_unix(),
                    ),
                    Err(_) => "unreadable".to_string(),
                };
                row_value(ui, "Saved session", Some(when));
                row_value(ui, "Its size", Some(format_bytes(row.apparent_bytes)));
                ui.add_space(8.0);
            }
        } else {
            ui.label(
                RichText::new("Off. Starting it boots the guest from scratch.")
                    .size(13.0)
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(10.0);
        }
        let disk = vm.size_bytes();
        let used = vm.allocated_bytes();
        row_value(
            ui,
            "Disk",
            Some(match used {
                Some(used) => cards::of(used, disk),
                None => format_bytes(disk),
            }),
        );
        ui::meter(
            ui,
            used.map(|u| u as f32 / disk.max(1) as f32).unwrap_or(0.0),
            width,
            theme::ACCENT,
        );
    });
}

/// "Label ............ value" on one line.
fn row_value(ui: &mut egui::Ui, label: &str, value: Option<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(13.0).color(theme::TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(value.unwrap_or_else(|| "—".to_string()))
                    .size(13.5)
                    .color(theme::TEXT),
            );
        });
    });
}

/// How to reach the machine: its SSH command, its shared folders.
fn connect_panel(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    status: Status,
    actions: &mut Vec<Action>,
) {
    panel(ui, "Connect", |ui| {
        let ssh = vm.ssh_command();
        if let Some(ssh) = &ssh {
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin {
                    left: 12,
                    right: 4,
                    top: 3,
                    bottom: 3,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(ssh).monospace().size(12.5).color(theme::TEXT),
                            )
                            .truncate(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui::secondary_button(ui, "Copy", true)
                                .on_hover_text("Through the machine's port forward to port 22")
                                .clicked()
                            {
                                actions.push(Action::CopyProfilePath(ssh.clone()));
                            }
                        });
                    });
                });
            ui.add_space(6.0);
        }
        for share in &vm.shares {
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                ui::paint_icon(ui.painter(), icon, Icon::Folder, theme::TEXT_DIM);
                ui.add(
                    egui::Label::new(RichText::new(&share.tag).size(13.5).color(theme::TEXT))
                        .truncate(),
                )
                .on_hover_text(share.path.display().to_string());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::text_button(ui, "Open", theme::ACCENT)
                        .on_hover_text(share.path.display().to_string())
                        .clicked()
                    {
                        actions.push(Action::Reveal(share.path.clone()));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.label(
                    RichText::new(format!("/mnt/{}", share.tag))
                        .size(12.0)
                        .color(theme::TEXT_FAINT),
                )
                .on_hover_text("Where the folder appears inside the guest");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::text_button(ui, "Copy mount", theme::TEXT_DIM)
                        .on_hover_text("Copy the command that opens the folder inside the guest")
                        .clicked()
                    {
                        actions.push(Action::CopyProfilePath(format!(
                            "sudo mkdir -p /mnt/{tag} && sudo mount -t 9p -o \
                             trans=virtio,version=9p2000.L,msize=524288 {tag} /mnt/{tag}",
                            tag = share.tag
                        )));
                    }
                });
            });
        }
        if ssh.is_none() && vm.shares.is_empty() {
            ui.label(
                RichText::new("No SSH forward and no shared folders yet.")
                    .size(13.0)
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(4.0);
            let at_rest = matches!(status, Status::Stopped | Status::Suspended);
            if ui::text_button(ui, "Set one up", theme::ACCENT).clicked() && at_rest {
                actions.push(Action::AskEditVmAt(
                    vm.name.clone(),
                    EditVmSection::NetworkDevices,
                ));
            }
        }
        let _ = app;
    });
}

/// The setup, as tiles that each open the editor on their own section.
fn setup(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    status: Status,
    actions: &mut Vec<Action>,
) {
    let editable = matches!(status, Status::Stopped | Status::Suspended);
    ui.horizontal(|ui| {
        ui.label(ui::caption("Setup"));
        ui.add_space(8.0);
        ui.label(
            RichText::new(match status {
                Status::Stopped => "Click a tile to change it.".to_string(),
                Status::Suspended => {
                    "Changing the hardware makes the saved session unrestorable — it only \
                     goes back onto the machine it came off."
                        .to_string()
                }
                _ => format!("Shut {} down to change these.", vm.name),
            })
            .size(12.5)
            .color(theme::TEXT_FAINT),
        );
    });
    ui.add_space(10.0);

    let disks = vm.disks.len();
    let network = match vm.network_interface.as_deref() {
        Some(interface) if interface != "usernet" => format!("Bridged ({interface})"),
        _ => match vm.forwards.len() {
            0 => "Internet, private".to_string(),
            1 => "Internet · 1 port forward".to_string(),
            n => format!("Internet · {n} port forwards"),
        },
    };
    let devices = match vm.shares.len() {
        0 => network,
        1 => format!("{network} · 1 shared folder"),
        n => format!("{network} · {n} shared folders"),
    };
    let startup = format!(
        "{}{}",
        if vm.uefi {
            "UEFI"
        } else {
            "Direct Linux kernel"
        },
        if app.settings.autostart.contains(&vm.name) {
            " · starts with the manager"
        } else {
            ""
        }
    );
    let tiles = [
        (
            Icon::Chip,
            "Processor & memory",
            format!(
                "{} cores · {}",
                vm.vcpus,
                format_bytes(vm.memory_mib * 1024 * 1024)
            ),
            EditVmSection::Hardware,
        ),
        (
            Icon::Screen,
            "Display & graphics",
            format!("{} × {}", vm.display.0, vm.display.1),
            EditVmSection::DisplayGpu,
        ),
        (
            Icon::Globe,
            "Network & devices",
            devices,
            EditVmSection::NetworkDevices,
        ),
        (
            Icon::Disk,
            "Storage",
            format!(
                "{} · {}",
                if disks == 1 {
                    "1 disk".to_string()
                } else {
                    format!("{disks} disks")
                },
                format_bytes(vm.size_bytes())
            ),
            EditVmSection::Storage,
        ),
        (Icon::Power, "Startup", startup, EditVmSection::BootMedia),
    ];

    let width = ui.available_width();
    let gap = 12.0;
    let columns = ((width + gap) / (280.0 + gap)).floor().clamp(1.0, 3.0);
    let tile_w = ((width - gap * (columns - 1.0)) / columns).floor();
    ui.with_layout(
        Layout::left_to_right(Align::Min).with_main_wrap(true),
        |ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(gap);
            for (icon, title, summary, section) in tiles {
                let response = setup_tile(ui, tile_w, icon, title, &summary, editable);
                let response = if editable {
                    response
                } else {
                    response.on_hover_text("Stop the machine first")
                };
                if response.clicked() && editable {
                    actions.push(Action::AskEditVmAt(vm.name.clone(), section));
                }
            }
        },
    );
}

fn setup_tile(
    ui: &mut egui::Ui,
    width: f32,
    icon: Icon,
    title: &str,
    summary: &str,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 76.0), sense);
    let t = theme::animate_bool(
        ui.ctx(),
        response.id.with("hover"),
        enabled && response.hovered(),
        0.12,
    );
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(theme::CARD_RADIUS);
    painter.rect_filled(rect, radius, theme::mix(theme::CARD, theme::CARD_HOVER, t));
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0_f32, theme::mix(theme::STROKE, theme::STROKE_STRONG, t)),
        egui::StrokeKind::Inside,
    );
    let well = Rect::from_center_size(
        egui::pos2(rect.left() + 36.0, rect.center().y),
        Vec2::splat(36.0),
    );
    painter.rect_filled(well, egui::CornerRadius::same(8), theme::SELECTED);
    ui::paint_icon(
        painter,
        Rect::from_center_size(well.center(), Vec2::splat(18.0)),
        icon,
        if enabled {
            theme::ACCENT
        } else {
            theme::TEXT_FAINT
        },
    );
    let x = well.right() + 14.0;
    let text_w = rect.right() - 36.0 - x;
    painter.text(
        egui::pos2(x, rect.center().y - 10.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(14.0),
        if enabled {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        },
    );
    let galley = painter.layout_no_wrap(
        summary.to_owned(),
        egui::FontId::proportional(12.5),
        theme::TEXT_DIM,
    );
    let clip = Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(x + text_w, rect.bottom()),
    );
    painter.with_clip_rect(clip).galley(
        egui::pos2(x, rect.center().y + 2.0),
        galley,
        theme::TEXT_DIM,
    );
    if enabled {
        ui::paint_icon(
            painter,
            Rect::from_center_size(
                egui::pos2(rect.right() - 22.0, rect.center().y),
                Vec2::splat(14.0),
            ),
            Icon::Chevron,
            theme::TEXT_FAINT,
        );
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    response
}

/// A machine still being installed has no profile yet, so it has no setup to
/// show — only the install itself.
fn installing_page(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    name: &str,
    time: f64,
    actions: &mut Vec<Action>,
) {
    let pending = app.pending.iter().find(|p| p.name == name);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(name).size(26.0).color(theme::TEXT).strong());
                ui.add_space(6.0);
                ui::status_pill(ui, Status::Installing, "Installing", time);
            });
            if let Some(pending) = pending {
                let m = &pending.machine;
                ui.label(
                    RichText::new(format!(
                        "{} · {} vCPU · {} · {}",
                        m.family.label(),
                        m.vcpus,
                        format_bytes(m.memory_mib * 1024 * 1024),
                        if m.disk_mode == crate::launcher::DiskMode::CreateNew {
                            format!("{} GiB disk", m.disk_gib)
                        } else {
                            "existing disk".to_string()
                        }
                    ))
                    .size(13.5)
                    .color(theme::TEXT_DIM),
                );
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ui::ghost_button(
                ui,
                "Cancel install",
                app.supervisor.is_busy(name),
                theme::ERR,
            )
            .clicked()
            {
                actions.push(Action::Stop(name.to_string()));
            }
            activity_button(ui, app, name, actions);
        });
    });
    ui.add_space(24.0);
    panel(ui, "Installing", |ui| {
        ui.label(
            RichText::new(
                "The installer is running in its own window. When it finishes, this \
                 page becomes the machine's page — with its screen and its setup.",
            )
            .size(13.5)
            .color(theme::TEXT_DIM),
        );
        ui.add_space(10.0);
        ui.label(
            RichText::new(
                "You can close Entangled: the install keeps going, and this page picks \
                 it up again.",
            )
            .size(12.5)
            .color(theme::TEXT_FAINT),
        );
    });
}

/// The page of a machine that is no longer in the folder (deleted, renamed,
/// moved away by hand).
fn gone(ui: &mut egui::Ui, name: &str, actions: &mut Vec<Action>) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(format!("'{name}' is no longer here"))
                .size(20.0)
                .color(theme::TEXT),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new("Its profile was removed, renamed or moved out of the machines folder.")
                .size(13.5)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(16.0);
        if ui::secondary_button(ui, "All machines", true).clicked() {
            actions.push(Action::SwitchView(View::Machines));
        }
    });
}
