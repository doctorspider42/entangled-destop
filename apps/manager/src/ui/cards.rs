//! The machine grid (GUI-1601): one card per discovered VM, plus a card for a
//! machine that is still being installed.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp, Status};
use crate::discovery::{format_bytes, VmEntry};
use crate::theme;
use crate::ui;

const CARD_HEIGHT: f32 = 232.0;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    let time = ctx.input(|i| i.time);

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            banners(ui, app, actions);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
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
                    });

                    if app.scan.vms.is_empty() && app.pending.is_empty() {
                        empty_state(ui, app, time, actions);
                    }
                    if !app.scan.problems.is_empty() {
                        problems(ui, app);
                    }
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
    if let Err(message) = &app.cli {
        any = true;
        if ui::banner(ui, theme::ERR, message, Some("Settings")) {
            actions.push(Action::OpenSettings);
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
            });
            ui.add_space(8.0);

            if missing_disk {
                ui.label(
                    RichText::new("disk image missing")
                        .color(theme::ERR)
                        .size(12.5),
                );
            } else {
                let mut text = format!("{} image", format_bytes(vm.size_bytes()));
                if let Some(allocated) = vm.allocated_bytes() {
                    text.push_str(&format!(" · {} on disk", format_bytes(allocated)));
                }
                ui.label(ui::dim(text));
            }
            let path = vm.profile_path.display().to_string();
            ui.label(ui::faint(shorten(&path, 46))).on_hover_text(&path);
            live_stats_line(ui, app, vm, status, accent_at);
            ui.add_space(6.0);

            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    match status {
                        Status::Stopped => {
                            if ui::ghost_button(ui, "Start", !missing_disk, theme::OK).clicked() {
                                actions.push(Action::Start(vm.name.clone()));
                            }
                        }
                        Status::Running => {
                            if ui::ghost_button(ui, "Stop", true, theme::WARN).clicked() {
                                actions.push(Action::Stop(vm.name.clone()));
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
                    if ui::ghost_button(ui, "Delete", status == Status::Stopped, theme::ERR)
                        .on_hover_text(if status == Status::Stopped {
                            "Remove the profile and its disk"
                        } else {
                            "Stop the machine first"
                        })
                        .clicked()
                    {
                        actions.push(Action::AskDelete(vm.name.clone()));
                    }
                    if ui::ghost_button(ui, "Profile", true, theme::TEXT_DIM)
                        .on_hover_text(format!("Copy {path}"))
                        .clicked()
                    {
                        actions.push(Action::CopyProfilePath(path.clone()));
                    }
                    if let Some(task) = app
                        .supervisor
                        .tasks()
                        .iter()
                        .rev()
                        .find(|t| t.vm == vm.name)
                    {
                        if ui::ghost_button(ui, "Log", true, theme::CYAN).clicked() {
                            actions.push(Action::SelectLog(task.id));
                        }
                    }
                });
            });
        },
    );
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
                    &format!("{} GiB disk", pending.machine.disk_gib),
                    theme::TEXT_DIM,
                );
                ui::chip(ui, &pending.machine.variant, theme::OK);
            });
            ui.add_space(8.0);
            ui.label(ui::dim(
                "Debian installer is running — the profile appears when it finishes",
            ));

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
                        if ui::ghost_button(ui, "Log", true, theme::CYAN).clicked() {
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
    fn shorten_keeps_the_tail_of_a_path() {
        assert_eq!(shorten("/vms/a.toml", 40), "/vms/a.toml");
        let long = "/home/spider/entangled-vms/some-really-long-name.toml";
        let short = shorten(long, 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.starts_with('…'));
        assert!(short.ends_with("name.toml"));
    }
}
