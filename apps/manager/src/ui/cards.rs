//! The machine grid (GUI-1601): one card per discovered VM, plus a card for a
//! machine that is still being installed.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp, Status};
use crate::discovery::{format_bytes, VmEntry};
use crate::theme;
use crate::ui;

// Six lifecycle buttons do not fit on one line at [`theme::CARD_WIDTH`], and a
// running machine now has six. The action row wraps instead, and the card is
// tall enough for the second line — measured against the widest state (Running,
// with a control channel), not guessed.
const CARD_HEIGHT: f32 = 276.0;
/// Height of one line of card buttons, and the gap between two wrapped lines.
/// Measured against `ui::ghost_button` (its galley plus 13 px of padding), not
/// guessed — the strip's height has to be known before the row is laid out.
const ACTION_ROW_H: f32 = 31.0;
const ACTION_WRAP_GAP: f32 = 6.0;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    let time = theme::animation_time(ctx);

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            theme::paint_backdrop(ui);
            banners(ui, app, actions);
            dashboard_heading(ui, app);
            ui.add_space(14.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.with_layout(
                        Layout::left_to_right(Align::Min).with_main_wrap(true),
                        |ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(16.0, 16.0);
                            for vm in &app.scan.vms {
                                vm_card(ui, app, vm, time, actions);
                            }
                            for pending in &app.pending {
                                if app.scan.vms.iter().any(|vm| vm.name == pending.name) {
                                    continue;
                                }
                                pending_card(ui, app, pending, time, actions);
                            }
                        },
                    );

                    if app.scan.vms.is_empty() && app.pending.is_empty() {
                        empty_state(ui, app, time, actions);
                    }
                    if !app.scan.problems.is_empty() {
                        problems(ui, app);
                    }
                });
        });
}

fn dashboard_heading(ui: &mut egui::Ui, app: &ManagerApp) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Machines")
                    .size(24.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.label(ui::dim(
                "Start, install and care for your virtual computers.",
            ));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let active = app
                .scan
                .vms
                .iter()
                .filter(|vm| app.is_busy(&vm.name))
                .count();
            ui::chip(ui, &format!("{active} active"), theme::OK);
            ui::chip(ui, &format!("{} total", app.scan.vms.len()), theme::CYAN);
        });
    });
}

/// The persistent notices (update, startup warning, scan errors, missing CLI).
/// Shared with the Disks view, which shows the same product state.
pub(crate) fn banners(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    let mut any = false;
    if let Some(update) = &app.update {
        any = true;
        update_banner(ui, app, update, actions);
    }
    if let Some(warning) = &app.startup_warning {
        any = true;
        ui::banner(ui, theme::WARN, warning, None);
    }
    if let Some(error) = &app.scan_error {
        any = true;
        let missing = !app.settings.vm_dir.exists();
        let action = missing.then_some("Create directory");
        if ui::banner(ui, theme::ERR, error, action) {
            actions.push(Action::CreateVmDir);
        }
    }
    // A missing engine is a problem with a fix, not a field to fill in: the
    // button opens the file browser straight away rather than depositing the
    // user in a settings panel to work out what to type.
    if let Err(message) = &app.engine {
        any = true;
        if ui::banner(ui, theme::ERR, message, Some("Locate it…")) {
            actions.push(Action::PickPath(crate::picker::PickTarget::EngineBinary));
        }
    }
    if any {
        ui.add_space(12.0);
    }
}

/// The non-intrusive "a newer version exists" notice (quantum theme): a card
/// surface with the cyan→violet hairline, the version pair, one accent action
/// and a quiet Skip. It never opens on its own and never blocks anything.
fn update_banner(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    update: &crate::update::UpdateInfo,
    actions: &mut Vec<Action>,
) {
    egui::Frame::new()
        .fill(theme::mix(theme::CARD, theme::CYAN, 0.05))
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
        .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            // The entanglement hairline along the top edge marks this as a
            // product message, not an error.
            let bar = egui::Rect::from_min_size(
                ui.max_rect().left_top() - Vec2::new(0.0, 8.0),
                Vec2::new(ui.available_width(), 2.0),
            );
            theme::gradient_rect(
                ui.painter(),
                bar,
                theme::CYAN.gamma_multiply(0.8),
                theme::VIOLET.gamma_multiply(0.8),
            );
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    RichText::new(format!("Entangled Desktop v{}", update.version))
                        .color(theme::TEXT)
                        .strong()
                        .size(13.5),
                );
                ui.label(ui::dim(format!(
                    "is available — this is v{}",
                    crate::VERSION
                )));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::ghost_button(ui, "Skip", !app.update_downloading, theme::TEXT_DIM)
                        .clicked()
                    {
                        actions.push(Action::DismissUpdate);
                    }
                    let installable = cfg!(windows) && update.installer_url.is_some();
                    if installable {
                        let label = if app.update_downloading {
                            "Downloading…"
                        } else {
                            "Download & install"
                        };
                        if ui::ghost_button(ui, label, !app.update_downloading, theme::CYAN)
                            .on_hover_text(
                                "Saves the installer to Downloads and starts it; \
                                 it upgrades this installation in place",
                            )
                            .clicked()
                        {
                            actions.push(Action::InstallUpdate);
                        }
                    } else if ui::ghost_button(ui, "Release page", true, theme::CYAN).clicked() {
                        actions.push(Action::OpenReleasePage);
                    }
                });
            });
        });
}

fn vm_card(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    time: f64,
    actions: &mut Vec<Action>,
) {
    let status = app.status_of(&vm.name);
    let accent_at = accent_for(&vm.name);
    let id = egui::Id::new(("vm-card", &vm.name));
    let missing_disk = vm.disks.iter().any(|d| !d.exists);

    ui::card(
        ui,
        id,
        Vec2::new(theme::CARD_WIDTH, CARD_HEIGHT),
        accent_at,
        |ui, _hover| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&vm.name)
                        .size(17.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui::status_badge(ui, status, time);
                });
            });
            ui.add_space(8.0);

            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui::chip(ui, &format!("{} MiB", vm.memory_mib), theme::CYAN);
                ui::chip(ui, &format!("{} vCPU", vm.vcpus), theme::VIOLET);
                ui::chip(
                    ui,
                    &format!("{}×{}", vm.display.0, vm.display.1),
                    theme::TEXT_DIM,
                );
                if let Some(interface) = &vm.network_interface {
                    ui::chip(ui, interface, theme::OK);
                }
                // Where this machine runs, but only where there is a choice to
                // make — on Linux the chip would say the same thing on every
                // card, which is noise.
                let backend = app.backend_of(&vm.name);
                if crate::backend::Backend::Wsl.available_on_host() {
                    ui::chip(ui, backend.label(), theme::accent(0.5));
                }
            });
            ui.add_space(8.0);

            if missing_disk {
                ui.label(
                    RichText::new("disk image missing")
                        .color(theme::ERR)
                        .size(12.5),
                );
            } else {
                let mut text = format!("Storage {}", format_bytes(vm.size_bytes()));
                if let Some(allocated) = vm.allocated_bytes() {
                    text.push_str(&format!(" · {} on disk", format_bytes(allocated)));
                }
                ui.label(ui::dim(text));
            }
            let path = vm.profile_path.display().to_string();
            ui.label(ui::faint(shorten(&path, 46))).on_hover_text(&path);
            live_stats_line(ui, app, vm, status, accent_at);
            suspend_line(ui, app, vm, status);
            ui.add_space(6.0);

            // The action strip sits on the bottom edge, and its height is
            // *reserved* rather than discovered: a wrapped row inside a
            // bottom-up layout grows downward, straight through the card's
            // border. A running machine has six buttons and they do not fit on
            // one line at CARD_WIDTH, so that state gets two.
            let lines = if status == Status::Running { 2.0 } else { 1.0 };
            let strip = lines * ACTION_ROW_H + (lines - 1.0) * ACTION_WRAP_GAP;
            ui.add_space((ui.available_height() - strip).max(0.0));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(7.0, ACTION_WRAP_GAP);
                match status {
                    Status::Stopped => {
                        if !missing_disk && ui::primary_button(ui, "Start").clicked() {
                            actions.push(Action::Start(vm.name.clone()));
                        }
                    }
                    // The third resting state (ADR-0006). Resume is the
                    // primary action, and it is only bright when the saved
                    // session could really go back — the reasons it could
                    // not are on the hover, not in a toast after the click.
                    Status::Suspended => {
                        let verdict = app
                            .snapshot_for(&vm.name)
                            .map(|row| app.snapshot_verdict(row))
                            .unwrap_or_default();
                        let hover = if verdict.resumable() {
                            "Put the machine back exactly as you left it — the same \
                             programs, the same windows."
                                .to_string()
                        } else {
                            verdict.blocked.join("\n\n")
                        };
                        if verdict.resumable() {
                            if ui::primary_button(ui, "Resume")
                                .on_hover_text(hover)
                                .clicked()
                            {
                                actions.push(Action::ResumeVm(vm.name.clone()));
                            }
                        } else if ui::ghost_button(ui, "Resume", false, theme::VIOLET)
                            .on_hover_text(hover)
                            .clicked()
                        {
                            // Unreachable while disabled; kept so the arm
                            // stays one shape.
                        }
                        if ui::ghost_button(ui, "Start fresh…", !missing_disk, theme::WARN)
                            .on_hover_text(
                                "Boot the machine from scratch. The saved session cannot \
                                 survive that — the guest writes to the disk it was \
                                 pinned to — so it is thrown away first, and you are \
                                 asked before anything happens.",
                            )
                            .clicked()
                        {
                            actions.push(Action::AskDiscardSnapshot(vm.name.clone()));
                        }
                    }
                    // One-way, and nothing to press: the machine is being
                    // written to a file and will stop when it has.
                    Status::Suspending => {
                        if let Some(task) = app.supervisor.active_task(&vm.name) {
                            if ui::ghost_button(ui, "View activity", true, theme::CYAN).clicked() {
                                actions.push(Action::SelectLog(task.id));
                            }
                        }
                    }
                    Status::Running => {
                        if ui::ghost_button(ui, "Stop", true, theme::WARN).clicked() {
                            actions.push(Action::Stop(vm.name.clone()));
                        }
                        // Pause and Restart (ADR-0005). Only for a VM this
                        // manager started: the control channel is a pipe to
                        // a child, so a VM launched from a terminal has
                        // none and the buttons say so rather than lying.
                        let controllable = app.has_control(&vm.name);
                        let paused = app.is_paused(&vm.name);
                        let label = if paused { "Resume" } else { "Pause" };
                        if ui::ghost_button(ui, label, controllable, theme::VIOLET)
                            .on_hover_text(if !controllable {
                                "This VM was not started from here"
                            } else if paused {
                                "Let the machine continue exactly where it stopped"
                            } else {
                                "Freeze the machine; nothing in it makes progress"
                            })
                            .clicked()
                        {
                            actions.push(Action::TogglePause(vm.name.clone()));
                        }
                        if ui::ghost_button(ui, "Restart", controllable, theme::VIOLET)
                            .on_hover_text(if controllable {
                                "Reboot the machine in place, as the guest's own Restart does"
                            } else {
                                "This VM was not started from here"
                            })
                            .clicked()
                        {
                            actions.push(Action::Reset(vm.name.clone()));
                        }
                        // Suspend (ADR-0006): close the lid. Sits beside
                        // Stop because that is the choice being made — this
                        // is the other way to stop a machine, and the one
                        // that keeps everything it was doing.
                        if ui::ghost_button(ui, "Suspend", controllable, theme::VIOLET)
                            .on_hover_text(if controllable {
                                "Write the whole machine to a file and stop it. Opening it \
                                 again puts you back exactly here. A machine with 2 GiB of \
                                 memory takes a few seconds; a desktop takes longer."
                            } else {
                                "This VM was not started from here"
                            })
                            .clicked()
                        {
                            actions.push(Action::Suspend(vm.name.clone()));
                        }
                    }
                    Status::Stopping => {
                        if ui::ghost_button(ui, "Kill", true, theme::ERR)
                            .on_hover_text("Shutdown was requested; force it")
                            .clicked()
                        {
                            actions.push(Action::Stop(vm.name.clone()));
                        }
                    }
                    Status::Installing => {
                        if ui::ghost_button(ui, "Abort", true, theme::ERR).clicked() {
                            actions.push(Action::Stop(vm.name.clone()));
                        }
                    }
                }
                // A suspended machine can be configured — it is not
                // running — but changing its hardware is exactly what makes
                // the saved session unrestorable, so the hover says so
                // instead of the refusal arriving at resume time.
                let configurable = matches!(status, Status::Stopped | Status::Suspended);
                if ui::ghost_button(ui, "Configure", configurable, theme::VIOLET)
                    .on_hover_text(match status {
                        Status::Stopped => "Memory, vCPUs, boot, network, disks, display",
                        Status::Suspended => {
                            "Memory, vCPUs, boot, network, disks, display. Changing the \
                             hardware makes the saved session unrestorable — a snapshot \
                             only goes back onto the machine it came off."
                        }
                        _ => "Stop the machine first",
                    })
                    .clicked()
                {
                    actions.push(Action::AskEditVm(vm.name.clone()));
                }
                ui.menu_button("More", |ui| {
                    if ui.button("Copy profile path").clicked() {
                        actions.push(Action::CopyProfilePath(path.clone()));
                        ui.close();
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
                            ui.close();
                        }
                    }
                    ui.separator();
                    if app.snapshot_for(&vm.name).is_some()
                        && ui
                            .add_enabled(
                                status == Status::Suspended,
                                egui::Button::new("Forget the saved session"),
                            )
                            .on_hover_text(
                                "Delete the snapshot and leave the machine stopped. Its \
                                 disks and settings are untouched.",
                            )
                            .clicked()
                    {
                        if let Some(row) = app.snapshot_for(&vm.name) {
                            actions.push(Action::AskDeleteSnapshot(row.path.clone()));
                        }
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            matches!(status, Status::Stopped | Status::Suspended),
                            egui::Button::new("Delete machine"),
                        )
                        .clicked()
                    {
                        actions.push(Action::AskDelete(vm.name.clone()));
                        ui.close();
                    }
                });
            });
        },
    );
}

/// What a machine in one of the two snapshot states has to say for itself.
///
/// **Suspending** has no progress to report — the engine writes the memory in
/// one pass and says nothing until it is done — so the honest thing is the time
/// it has been going plus what to expect. A fake progress bar filling at a rate
/// nobody measured would be worse than a number that is simply true.
///
/// **Suspended** says when, how big, and — when there is one — the first reason
/// it could not go back, which is the thing the user most needs before they
/// reach for the button.
fn suspend_line(ui: &mut egui::Ui, app: &ManagerApp, vm: &VmEntry, status: Status) {
    match status {
        Status::Suspending => {
            let elapsed = app
                .supervisor
                .active_task(&vm.name)
                .and_then(|task| task.suspending_for())
                .unwrap_or_default();
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!(
                    "writing memory to disk — {:.0} s",
                    elapsed.as_secs_f32()
                ))
                .color(theme::VIOLET)
                .size(12.0),
            )
            .on_hover_text(
                "Only the pages the guest has actually touched are written, which is \
                 usually about a quarter of its memory. A 2 GiB machine takes a few \
                 seconds; a desktop-sized one longer.",
            );
        }
        Status::Suspended => {
            let Some(row) = app.snapshot_for(&vm.name) else {
                return;
            };
            let verdict = app.snapshot_verdict(row);
            ui.add_space(2.0);
            let mut line = match &row.facts {
                Ok(facts) => format!(
                    "saved {}",
                    vm_snapshot::meta::describe_age(
                        facts.created_unix,
                        vm_snapshot::meta::now_unix()
                    )
                ),
                Err(_) => "saved session unreadable".to_string(),
            };
            line.push_str(&format!(" · {}", format_bytes(row.apparent_bytes)));
            ui.label(RichText::new(line).color(theme::VIOLET).size(12.0))
                .on_hover_text(row.path.display().to_string());
            if let Some(reason) = verdict.blocked.first() {
                ui.label(
                    egui::RichText::new(shorten_end(reason, 52))
                        .color(theme::WARN)
                        .size(11.5),
                )
                .on_hover_text(verdict.blocked.join("\n\n"));
            }
        }
        _ => {}
    }
}

/// One quiet line of live numbers for an active machine: uptime, CPU (of the
/// `entangled run` child — the guest lives inside it), resident RAM, and a
/// sparkline of the last minute and a half. Numbers the sampler cannot answer
/// simply stay away.
fn live_stats_line(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    status: Status,
    accent_at: f32,
) {
    if !matches!(
        status,
        Status::Running | Status::Installing | Status::Stopping
    ) {
        return;
    }
    let Some(task) = app.supervisor.active_task(&vm.name) else {
        return;
    };
    let mut parts = vec![format!(
        "up {}",
        crate::metrics::format_uptime(task.started_at.elapsed())
    )];
    let stats = app.stats.vms.get(&vm.name);
    if let Some(stats) = stats {
        if let Some(cpu) = stats.cpu_percent {
            parts.push(format!("CPU {cpu:.0}%"));
        }
        if let Some(rss) = stats.rss_bytes {
            parts.push(format!("{} RAM", format_bytes(rss)));
        }
    }
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            RichText::new(parts.join(" · "))
                .color(theme::accent(accent_at))
                .size(12.0),
        );
        if let Some(stats) = stats {
            if stats.history.len() >= 2 {
                ui::sparkline(
                    ui,
                    &stats.history,
                    Vec2::new(76.0, 15.0),
                    theme::accent(accent_at),
                );
            }
        }
    });
}

fn pending_card(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    pending: &crate::app::PendingInstall,
    time: f64,
    actions: &mut Vec<Action>,
) {
    let id = egui::Id::new(("pending-card", &pending.name));
    ui::card(
        ui,
        id,
        Vec2::new(theme::CARD_WIDTH, CARD_HEIGHT),
        accent_for(&pending.name),
        |ui, _hover| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&pending.name)
                        .size(17.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui::status_badge(ui, Status::Installing, time);
                });
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui::chip(
                    ui,
                    &format!("{} MiB", pending.machine.memory_mib),
                    theme::CYAN,
                );
                ui::chip(
                    ui,
                    &format!("{} vCPU", pending.machine.vcpus),
                    theme::VIOLET,
                );
                ui::chip(
                    ui,
                    &if pending.machine.disk_mode == crate::launcher::DiskMode::CreateNew {
                        format!("{} GiB disk", pending.machine.disk_gib)
                    } else {
                        "existing disk".to_string()
                    },
                    theme::TEXT_DIM,
                );
                ui::chip(ui, pending.machine.family.label(), theme::OK);
            });
            ui.add_space(8.0);
            ui.label(ui::dim(format!(
                "{} installer is running — this card becomes a machine when it finishes",
                pending.machine.family.label()
            )));

            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    let busy = app.supervisor.is_busy(&pending.name);
                    if ui::ghost_button(ui, "Abort", busy, theme::ERR).clicked() {
                        actions.push(Action::Stop(pending.name.clone()));
                    }
                    if let Some(task) = app
                        .supervisor
                        .tasks()
                        .iter()
                        .rev()
                        .find(|t| t.vm == pending.name)
                    {
                        if ui::ghost_button(ui, "View activity", true, theme::CYAN).clicked() {
                            actions.push(Action::SelectLog(task.id));
                        }
                    }
                });
            });
        },
    );
}

fn empty_state(ui: &mut egui::Ui, app: &ManagerApp, time: f64, actions: &mut Vec<Action>) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(180.0, 120.0), egui::Sense::hover());
        crate::logo::paint_mark(ui.painter(), rect, time, 0.5);
        ui.add_space(10.0);
        ui.label(
            RichText::new("No machines yet")
                .size(19.0)
                .color(theme::TEXT),
        );
        ui.label(ui::dim(format!(
            "{} holds no VM profiles",
            app.settings.vm_dir.display()
        )));
        ui.add_space(14.0);
        if ui::primary_button(ui, "Create the first machine").clicked() {
            actions.push(Action::OpenWizard);
        }
    });
}

fn problems(ui: &mut egui::Ui, app: &ManagerApp) {
    ui.add_space(18.0);
    ui.label(ui::dim("Profiles that could not be read"));
    ui.add_space(4.0);
    for problem in &app.scan.problems {
        ui.label(ui::faint(format!(
            "{}: {}",
            problem
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| problem.path.display().to_string()),
            problem.message
        )));
    }
}

/// Stable per-VM position on the cyan→violet ramp, so each card keeps its own
/// hue instead of the grid looking uniform.
fn accent_for(name: &str) -> f32 {
    let sum: u32 = name.bytes().map(u32::from).sum();
    (sum % 100) as f32 / 100.0
}

/// Trailing ellipsis, for a sentence that has a tooltip carrying the whole of
/// it. The opposite end from [`shorten`], because a refusal's first words are
/// the ones that identify it.
fn shorten_end(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// Middle-ellipsis for long paths.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_are_stable_and_in_range() {
        for name in ["debian-demo", "a", "very-long-machine-name-1234"] {
            let value = accent_for(name);
            assert_eq!(value, accent_for(name));
            assert!((0.0..1.0).contains(&value), "{name} → {value}");
        }
    }

    #[test]
    fn shorten_end_keeps_the_start_of_a_refusal() {
        let reason = "This was saved on Linux/KVM and the machine is set to run on \
                      Windows/WHP.";
        let short = shorten_end(reason, 30);
        assert!(short.starts_with("This was saved on"), "{short}");
        assert!(short.ends_with('…'), "{short}");
        assert!(short.chars().count() <= 30, "{short}");
        assert_eq!(shorten_end("short", 30), "short");
    }

    #[test]
    fn shorten_keeps_the_tail_of_a_path() {
        assert_eq!(shorten("/vms/a.toml", 40), "/vms/a.toml");
        let long = "/home/spider/entangled-vms/some-really-long-name.toml";
        let short = shorten(long, 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.starts_with('…'));
        assert!(short.ends_with("name.toml"));
    }
}
