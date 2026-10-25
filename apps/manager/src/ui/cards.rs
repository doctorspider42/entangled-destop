//! All machines (GUI-1601): the running ones large, with their screen and live
//! numbers, and everything else as a tile that opens the machine's own page.
//! With no machines at all, the first-launch page instead.

use egui::{Align, Layout, Rect, RichText, Sense, Vec2};

use crate::app::{Action, ManagerApp, Status, View};
use crate::discovery::{format_bytes, VmEntry};
use crate::launcher::GuestFamily;
use crate::theme;
use crate::ui::{self, Icon};

/// Horizontal page margin.
pub(crate) const PAGE_X: i8 = 40;
/// Smallest width a tile in the grid may take.
const TILE_MIN_W: f32 = 260.0;
/// Height of a tile's screen area.
const TILE_SCREEN_H: f32 = 132.0;
/// A whole tile: the screen plus two lines of text and their padding.
const TILE_H: f32 = TILE_SCREEN_H + 86.0;
/// Gap between tiles.
const GAP: f32 = 16.0;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    let time = theme::animation_time(ctx);

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(PAGE_X, 28)),
        )
        .show(ctx, |ui| {
            theme::paint_backdrop(ui);
            if app.scan.vms.is_empty() && app.pending.is_empty() {
                banners(ui, app, actions);
                welcome(ui, app, actions);
                return;
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    banners(ui, app, actions);
                    heading(ui, app, actions);
                    ui.add_space(24.0);

                    let running: Vec<&VmEntry> = app
                        .scan
                        .vms
                        .iter()
                        .filter(|vm| app.status_of(&vm.name) == Status::Running)
                        .collect();
                    if !running.is_empty() {
                        ui.label(ui::caption("Running now"));
                        ui.add_space(10.0);
                        for vm in &running {
                            running_row(ui, app, vm, time, actions);
                            ui.add_space(14.0);
                        }
                        ui.add_space(14.0);
                    }

                    let rest: Vec<&VmEntry> = app
                        .scan
                        .vms
                        .iter()
                        .filter(|vm| app.status_of(&vm.name) != Status::Running)
                        .collect();
                    let pending: Vec<&crate::app::PendingInstall> = app
                        .pending
                        .iter()
                        .filter(|p| !app.scan.vms.iter().any(|vm| vm.name == p.name))
                        .collect();
                    ui.label(ui::caption(if running.is_empty() {
                        "Machines"
                    } else {
                        "Everything else"
                    }));
                    ui.add_space(10.0);
                    tile_grid(ui, app, &rest, &pending, actions);

                    if !app.scan.problems.is_empty() {
                        problems(ui, app);
                    }
                    ui.add_space(24.0);
                });
        });
}

/// Page title, a one-sentence summary, and the page's actions.
fn heading(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("All machines")
                    .size(24.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(summary(app))
                    .size(13.5)
                    .color(theme::TEXT_DIM),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui::primary_button(ui, "New machine").clicked() {
                actions.push(Action::OpenWizard);
            }
            if ui::secondary_button(ui, "Import…", true)
                .on_hover_text(
                    "Bring in a machine profile (.toml) from another folder. It is \
                     copied here with its paths made absolute; the files stay put. \
                     Dropping a profile on the window does the same.",
                )
                .clicked()
            {
                actions.push(Action::PickPath(crate::picker::PickTarget::ImportProfile));
            }
            let (label, tint) = match app.refresh_state() {
                crate::app::RefreshState::Idle => ("Refresh", theme::TEXT_DIM),
                crate::app::RefreshState::Scanning => ("Refreshing…", theme::TEXT_FAINT),
                crate::app::RefreshState::Complete => ("Up to date", theme::OK),
            };
            if ui::text_button(ui, label, tint)
                .on_hover_text("Look in the machines folder again now")
                .clicked()
            {
                actions.push(Action::Refresh);
            }
        });
    });
}

/// "One running, one saved for later, one installing." — the page in words.
fn summary(app: &ManagerApp) -> String {
    let mut counts = [0usize; 4];
    for vm in &app.scan.vms {
        match app.status_of(&vm.name) {
            Status::Running | Status::Stopping => counts[0] += 1,
            Status::Suspended | Status::Suspending => counts[1] += 1,
            Status::Installing => counts[2] += 1,
            Status::Stopped => counts[3] += 1,
        }
    }
    counts[2] += app
        .pending
        .iter()
        .filter(|p| !app.scan.vms.iter().any(|vm| vm.name == p.name))
        .count();
    summarise(counts)
}

/// The sentence for `[running, saved, installing, off]`.
fn summarise(counts: [usize; 4]) -> String {
    let word = |n: usize| match n {
        1 => "one".to_string(),
        2 => "two".to_string(),
        3 => "three".to_string(),
        n => n.to_string(),
    };
    let labels = ["running", "saved for later", "installing", "off"];
    let parts: Vec<String> = counts
        .iter()
        .zip(labels)
        .filter(|(n, _)| **n > 0)
        .map(|(n, label)| format!("{} {label}", word(*n)))
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    let sentence = parts.join(", ");
    let mut chars = sentence.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// A running machine, large: its live screen, the numbers that matter right
/// now and the controls you reach for while it runs.
fn running_row(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    time: f64,
    actions: &mut Vec<Action>,
) {
    let width = ui.available_width();
    let screen_w = (width * 0.42).clamp(280.0, 520.0);
    let screen_h = screen_w * 9.0 / 16.0;
    let height = screen_h.max(210.0);
    let id = egui::Id::new(("running-row", &vm.name));
    ui::card(ui, id, Vec2::new(width, height), |ui, _| {
        let inner = ui.max_rect().expand(ui::CARD_PAD);
        let screen = Rect::from_min_size(inner.min, Vec2::new(screen_w, height));
        let open = ui.interact(screen, id.with("screen"), Sense::click());
        screen_well(ui, app, &vm.name, Status::Running, screen, Corners::Left);
        if open.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if open.on_hover_text("Open this machine's page").clicked() {
            actions.push(Action::SwitchView(View::Machine(vm.name.clone())));
        }

        let info = Rect::from_min_max(
            egui::pos2(screen.right() + 28.0, inner.top() + 22.0),
            egui::pos2(inner.right() - 22.0, inner.bottom() - 20.0),
        );
        let mut ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(info)
                .layout(Layout::top_down(Align::Min)),
        );
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&vm.name)
                    .size(19.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.add_space(6.0);
            ui::status_pill(ui, Status::Running, &ui::status_line(app, &vm.name), time);
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(spec_line(app, vm))
                .size(13.0)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(16.0);
        live_numbers(&mut ui, app, vm);
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                running_controls(ui, app, vm, false, actions);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::icon_button(ui, Icon::Chevron, "Open this machine's page").clicked() {
                        actions.push(Action::SwitchView(View::Machine(vm.name.clone())));
                    }
                });
            });
        });
    });
}

/// Suspend and Shut down for a running machine. Only for a machine this manager
/// started: the control channel is a pipe to a child, so a VM launched from a
/// terminal has none and the buttons say so rather than lying.
///
/// Reads "Suspend · Shut down" in either layout direction: `right_to_left` is
/// for a caller laying out from the right edge.
pub(crate) fn running_controls(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vm: &VmEntry,
    right_to_left: bool,
    actions: &mut Vec<Action>,
) {
    let controllable = app.has_control(&vm.name);
    let cpu_block = app.backend_of(&vm.name).cpu_lifecycle_block();
    let suspend = |ui: &mut egui::Ui| {
        ui::primary_button_enabled(ui, "Suspend", controllable && cpu_block.is_none())
            .on_hover_text(if let Some(reason) = cpu_block {
                reason
            } else if !controllable {
                "This VM was not started from here"
            } else {
                "Save the session to disk and stop the machine. Resume returns to this session."
            })
            .clicked()
    };
    let shut_down = |ui: &mut egui::Ui| {
        ui::secondary_button(ui, "Shut down", controllable)
            .on_hover_text(if controllable {
                "Press the machine's power button: it shuts itself down cleanly, the \
                 way it would on real hardware. A desktop may ask first."
            } else {
                "This VM was not started from here"
            })
            .clicked()
    };
    let (suspend_clicked, shut_clicked) = if right_to_left {
        let shut = shut_down(ui);
        (suspend(ui), shut)
    } else {
        let save = suspend(ui);
        (save, shut_down(ui))
    };
    if suspend_clicked {
        actions.push(Action::Suspend(vm.name.clone()));
    }
    if shut_clicked {
        actions.push(Action::Shutdown(vm.name.clone()));
    }
}

/// Processor, memory and disk as three labelled meters.
pub(crate) fn live_numbers(ui: &mut egui::Ui, app: &ManagerApp, vm: &VmEntry) {
    let stats = app.stats.vms.get(&vm.name);
    let cpu = stats.and_then(|s| s.cpu_percent);
    let rss = stats.and_then(|s| s.rss_bytes);
    let memory = vm.memory_mib * 1024 * 1024;
    let disk = vm.size_bytes();
    let used = vm.allocated_bytes();
    let width = ((ui.available_width() - 2.0 * 20.0) / 3.0).max(80.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 20.0;
        meter_cell(
            ui,
            width,
            "Processor",
            cpu.map(|c| format!("{c:.0}%")),
            cpu.map(|c| c / (100.0 * vm.vcpus.max(1) as f32)),
        );
        meter_cell(
            ui,
            width,
            "Memory",
            rss.map(|r| of(r, memory)),
            rss.map(|r| r as f32 / memory.max(1) as f32),
        );
        meter_cell(
            ui,
            width,
            "Disk",
            used.map(|u| of(u, disk)),
            used.map(|u| u as f32 / disk.max(1) as f32),
        );
    });
}

fn meter_cell(
    ui: &mut egui::Ui,
    width: f32,
    label: &str,
    value: Option<String>,
    fraction: Option<f32>,
) {
    ui.allocate_ui_with_layout(Vec2::new(width, 52.0), Layout::top_down(Align::Min), |ui| {
        ui.spacing_mut().item_spacing.y = 5.0;
        ui.label(RichText::new(label).size(12.0).color(theme::TEXT_FAINT));
        ui.label(
            RichText::new(value.unwrap_or_else(|| "—".to_string()))
                .size(14.0)
                .color(theme::TEXT),
        );
        ui::meter(ui, fraction.unwrap_or(0.0), width, theme::ACCENT);
    });
}

/// "5.3 of 8.0 GiB": a part of a whole, the unit said once.
pub(crate) fn of(part: u64, whole: u64) -> String {
    let whole_text = format_bytes(whole);
    let part_text = format_bytes(part);
    match (part_text.rsplit_once(' '), whole_text.rsplit_once(' ')) {
        (Some((number, unit)), Some((_, whole_unit))) if unit == whole_unit => {
            format!("{number} of {whole_text}")
        }
        _ => format!("{part_text} of {whole_text}"),
    }
}

/// "4 vCPU · 8.0 GiB" — the short shape a tile has room for.
fn short_spec(vm: &VmEntry) -> String {
    format!(
        "{} vCPU · {}",
        vm.vcpus,
        format_bytes(vm.memory_mib * 1024 * 1024)
    )
}

/// "4 vCPU · 8.0 GiB · 1920×1080 · Windows (WHP)" — the machine's shape.
pub(crate) fn spec_line(app: &ManagerApp, vm: &VmEntry) -> String {
    let mut parts = vec![
        format!("{} vCPU", vm.vcpus),
        format_bytes(vm.memory_mib * 1024 * 1024),
        format!("{}×{}", vm.display.0, vm.display.1),
    ];
    // Where it runs, but only where there is a choice to make — on Linux it
    // would say the same thing for every machine, which is noise.
    if crate::backend::Backend::Wsl.available_on_host() {
        parts.push(app.backend_of(&vm.name).label().to_string());
    }
    parts.join(" · ")
}

/// The grid of everything that is not running, plus machines still being
/// installed, plus a dashed "New machine" tile at the end.
fn tile_grid(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    vms: &[&VmEntry],
    pending: &[&crate::app::PendingInstall],
    actions: &mut Vec<Action>,
) {
    let width = ui.available_width();
    let columns = ((width + GAP) / (TILE_MIN_W + GAP)).floor().max(1.0);
    let tile_w = ((width - GAP * (columns - 1.0)) / columns).floor();
    ui.with_layout(
        Layout::left_to_right(Align::Min).with_main_wrap(true),
        |ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(GAP);
            for vm in vms {
                let status = app.status_of(&vm.name);
                tile(ui, app, &vm.name, status, tile_w, Some(vm), actions);
            }
            for pending in pending {
                tile(
                    ui,
                    app,
                    &pending.name,
                    Status::Installing,
                    tile_w,
                    None,
                    actions,
                );
            }
            new_tile(ui, tile_w, actions);
        },
    );
}

/// One machine at rest (or in transition): its screen as it was left, its name
/// and state, and the one action it most likely needs.
fn tile(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    name: &str,
    status: Status,
    width: f32,
    vm: Option<&VmEntry>,
    actions: &mut Vec<Action>,
) {
    let id = egui::Id::new(("tile", name));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, TILE_H), Sense::hover());
    let open = ui.interact(rect, id, Sense::click());
    let t = theme::animate_bool(ui.ctx(), id.with("hover"), open.hovered(), 0.12);
    let radius = egui::CornerRadius::same(theme::CARD_RADIUS);
    ui.painter().rect_filled(rect, radius, theme::CARD);
    let screen = Rect::from_min_size(rect.min, Vec2::new(width, TILE_SCREEN_H));
    screen_well(ui, app, name, status, screen, Corners::Top);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0_f32, theme::mix(theme::STROKE, theme::STROKE_STRONG, t)),
        egui::StrokeKind::Inside,
    );
    if open.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if open.clicked() {
        actions.push(Action::SwitchView(View::Machine(name.to_string())));
    }

    let body = Rect::from_min_max(
        egui::pos2(rect.left() + 16.0, screen.bottom() + 14.0),
        egui::pos2(rect.right() - 14.0, rect.bottom() - 12.0),
    );
    let mut ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(body)
            .layout(Layout::top_down(Align::Min)),
    );
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::new(10.0, 22.0), Sense::hover());
        ui::status_dot(ui.painter(), dot.center(), status);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            tile_action(ui, app, name, status, vm, actions);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(
                    egui::Label::new(RichText::new(name).size(15.0).color(theme::TEXT).strong())
                        .selectable(false)
                        .truncate(),
                );
            });
        });
    });
    let line = match vm {
        Some(vm) => format!("{} · {}", ui::status_line(app, name), short_spec(vm)),
        None => "The installer is running".to_string(),
    };
    ui.add(
        egui::Label::new(RichText::new(line).size(12.5).color(theme::TEXT_DIM))
            .selectable(false)
            .truncate(),
    );
}

/// The one button a tile carries, when its state has an obvious next step.
fn tile_action(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    name: &str,
    status: Status,
    vm: Option<&VmEntry>,
    actions: &mut Vec<Action>,
) {
    let missing_disk = vm.is_some_and(|vm| vm.disks.iter().any(|d| !d.exists));
    match status {
        Status::Stopped if vm.is_some() && !missing_disk => {
            if ui::secondary_button(ui, "Start", true).clicked() {
                actions.push(Action::Start(name.to_string()));
            }
        }
        Status::Suspended => {
            let verdict = app
                .snapshot_for(name)
                .map(|row| app.snapshot_verdict(row))
                .unwrap_or_default();
            let response = ui::secondary_button(ui, "Resume", verdict.resumable());
            let response = if verdict.resumable() {
                response.on_hover_text("Put the machine back exactly as you left it")
            } else {
                response.on_hover_text(verdict.blocked.join("\n\n"))
            };
            if response.clicked() {
                actions.push(Action::ResumeVm(name.to_string()));
            }
        }
        _ => {}
    }
}

/// The dashed tile at the end of the grid.
fn new_tile(ui: &mut egui::Ui, width: f32, actions: &mut Vec<Action>) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, TILE_H), Sense::click());
    let t = theme::animate_bool(
        ui.ctx(),
        response.id.with("hover"),
        response.hovered(),
        0.12,
    );
    let painter = ui.painter();
    let stroke = egui::Stroke::new(
        1.0_f32,
        theme::mix(theme::STROKE_STRONG, theme::TEXT_FAINT, t),
    );
    // egui has no dashed rectangle: four dashed edges, stopping short of the
    // corners so the outline reads as rounded.
    let r = rect.shrink(0.5);
    let k = theme::CARD_RADIUS as f32;
    for (a, b) in [
        (
            r.left_top() + Vec2::new(k, 0.0),
            r.right_top() - Vec2::new(k, 0.0),
        ),
        (
            r.left_bottom() + Vec2::new(k, 0.0),
            r.right_bottom() - Vec2::new(k, 0.0),
        ),
        (
            r.left_top() + Vec2::new(0.0, k),
            r.left_bottom() - Vec2::new(0.0, k),
        ),
        (
            r.right_top() + Vec2::new(0.0, k),
            r.right_bottom() - Vec2::new(0.0, k),
        ),
    ] {
        painter.extend(egui::Shape::dashed_line(&[a, b], stroke, 6.0, 5.0));
    }
    let fg = theme::mix(theme::TEXT_DIM, theme::TEXT, t);
    ui::paint_icon(
        painter,
        Rect::from_center_size(rect.center() - Vec2::new(0.0, 14.0), Vec2::splat(22.0)),
        Icon::Plus,
        fg,
    );
    painter.text(
        rect.center() + Vec2::new(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        "New machine",
        egui::FontId::proportional(13.5),
        fg,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        actions.push(Action::OpenWizard);
    }
}

/// Which corners of a screen well are rounded: the well sits at the top of a
/// tile, at the left of a wide row, or alone.
#[derive(Clone, Copy)]
pub(crate) enum Corners {
    Top,
    Left,
    All,
}

/// A machine's screen in `rect`: the live picture while it runs, the last one
/// dimmed when it does not, or a quiet placeholder that says what state it is
/// in.
pub(crate) fn screen_well(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    name: &str,
    status: Status,
    rect: Rect,
    corners: Corners,
) {
    let r = theme::CARD_RADIUS;
    let radius = match corners {
        Corners::Top => egui::CornerRadius {
            nw: r,
            ne: r,
            sw: 0,
            se: 0,
        },
        Corners::Left => egui::CornerRadius {
            nw: r,
            ne: 0,
            sw: r,
            se: 0,
        },
        Corners::All => egui::CornerRadius::same(r),
    };
    ui.painter().rect_filled(rect, radius, theme::INSET);
    let live = status == Status::Running;
    if let Some(texture) = app.thumbs.get(name) {
        let size = texture.size_vec2();
        if size.x > 0.0 && size.y > 0.0 {
            // Cover the well, cropping what does not fit, so every tile in a
            // row has the same shape whatever the guest's resolution.
            let scale = (rect.width() / size.x).max(rect.height() / size.y);
            let shown = size * scale;
            let uv = Rect::from_center_size(
                egui::pos2(0.5, 0.5),
                Vec2::new(
                    (rect.width() / shown.x).min(1.0),
                    (rect.height() / shown.y).min(1.0),
                ),
            );
            egui::Image::new(texture)
                .uv(uv)
                .corner_radius(radius)
                .tint(if live {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(96)
                })
                .paint_at(ui, rect);
            return;
        }
    }
    let words = match status {
        Status::Running => "Waiting for the first picture",
        Status::Stopped => "Off",
        Status::Suspended => "Saved session",
        Status::Suspending => "Saving…",
        Status::Installing => "Installing",
        Status::Stopping => "Shutting down",
    };
    let painter = ui.painter();
    let mark = Rect::from_center_size(rect.center() - Vec2::new(0.0, 12.0), Vec2::splat(30.0));
    crate::logo::paint_mark(painter, mark, theme::INSET);
    // Dim the mark: a placeholder must not compete with real screens.
    painter.rect_filled(
        mark.expand(3.0),
        egui::CornerRadius::ZERO,
        theme::INSET.gamma_multiply(0.55),
    );
    painter.text(
        rect.center() + Vec2::new(0.0, 18.0),
        egui::Align2::CENTER_CENTER,
        words,
        egui::FontId::monospace(11.5),
        theme::TEXT_FAINT,
    );
}

/// The first-launch page: what this is, and the two systems to start with.
fn welcome(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    let width = ui.available_width().min(720.0);
    let top = ((ui.available_height() - 470.0) * 0.4).max(24.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.set_max_width(width);
        let (mark, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
        crate::logo::paint_mark(ui.painter(), mark, theme::BG_DEEP);
        ui.add_space(22.0);
        ui.label(
            RichText::new("Linux, right on your desktop")
                .size(30.0)
                .color(theme::TEXT)
                .strong(),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Pick a system and Entangled sets up the rest: the disk, the network \
                 and a window at your monitor's resolution.",
            )
            .size(14.5)
            .color(theme::TEXT_DIM),
        );
        ui.add_space(26.0);
        let gap = 16.0;
        let card_w = ((width - gap) / 2.0).floor();
        ui.allocate_ui_with_layout(
            Vec2::new(width, 150.0),
            Layout::left_to_right(Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.x = gap;
                let ubuntu_first = GuestFamily::default_for_host() == GuestFamily::Ubuntu;
                for (family, title, text) in [
                    (
                        GuestFamily::Ubuntu,
                        "Ubuntu desktop",
                        "A full desktop with GPU acceleration. Installs offline from a \
                         verified image.",
                    ),
                    (
                        GuestFamily::Debian,
                        "Debian",
                        "A smaller system that downloads its packages while it installs.",
                    ),
                ] {
                    let recommended = (family == GuestFamily::Ubuntu) == ubuntu_first;
                    if choice_card(ui, card_w, title, text, recommended).clicked() {
                        actions.push(Action::OpenWizardFor(family));
                    }
                }
            },
        );
        ui.add_space(20.0);
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            ui.horizontal(|ui| {
                for family in [GuestFamily::Fedora, GuestFamily::OtherLinux] {
                    if ui::secondary_button(ui, family.label(), true).clicked() {
                        actions.push(Action::OpenWizardFor(family));
                    }
                }
            });
            ui.add_space(12.0);
        }
        ui.horizontal(|ui| {
            ui.add_space((width * 0.5 - 140.0).max(0.0));
            ui.label(
                RichText::new("Already have one?")
                    .size(13.0)
                    .color(theme::TEXT_DIM),
            );
            if ui::text_button(ui, "Import a machine", theme::ACCENT).clicked() {
                actions.push(Action::PickPath(crate::picker::PickTarget::ImportProfile));
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "Machines live in {}",
                app.settings.vm_dir.display()
            ))
            .size(11.5)
            .color(theme::TEXT_FAINT),
        );
    });
}

fn choice_card(
    ui: &mut egui::Ui,
    width: f32,
    title: &str,
    text: &str,
    recommended: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 150.0), Sense::click());
    let t = theme::animate_bool(
        ui.ctx(),
        response.id.with("hover"),
        response.hovered(),
        0.12,
    );
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(12);
    painter.rect_filled(rect, radius, theme::MODAL);
    let edge = if recommended {
        theme::mix(theme::STROKE_STRONG, theme::ACCENT, 0.35 + 0.65 * t)
    } else {
        theme::mix(theme::STROKE, theme::ACCENT, t)
    };
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0_f32, edge),
        egui::StrokeKind::Inside,
    );
    let badge = Rect::from_min_size(rect.min + Vec2::new(22.0, 22.0), Vec2::splat(40.0));
    painter.rect_filled(badge, egui::CornerRadius::same(10), theme::SELECTED);
    let initial: String = title.chars().take(1).collect();
    painter.text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(17.0),
        theme::TEXT,
    );
    painter.text(
        egui::pos2(badge.right() + 12.0, badge.top() + 1.0),
        egui::Align2::LEFT_TOP,
        title,
        egui::FontId::proportional(16.0),
        theme::TEXT,
    );
    painter.text(
        egui::pos2(badge.right() + 12.0, badge.bottom() - 1.0),
        egui::Align2::LEFT_BOTTOM,
        if recommended {
            "Recommended"
        } else {
            "Also available"
        },
        egui::FontId::proportional(12.0),
        if recommended {
            theme::ACCENT
        } else {
            theme::TEXT_FAINT
        },
    );
    let galley = painter.layout(
        text.to_owned(),
        egui::FontId::proportional(13.0),
        theme::TEXT_DIM,
        width - 44.0,
    );
    painter.galley(
        egui::pos2(rect.left() + 22.0, badge.bottom() + 16.0),
        galley,
        theme::TEXT_DIM,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
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

/// The non-intrusive "a newer version exists" notice: a card surface with the
/// version pair, one action and a quiet Skip. It never opens on its own and
/// never blocks anything.
fn update_banner(
    ui: &mut egui::Ui,
    app: &ManagerApp,
    update: &crate::update::UpdateInfo,
    actions: &mut Vec<Action>,
) {
    egui::Frame::new()
        .fill(theme::mix(theme::CARD, theme::ACCENT, 0.05))
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
        .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
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
                    if ui::text_button(ui, "Skip", theme::TEXT_DIM).clicked()
                        && !app.update_downloading
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
                        if ui::secondary_button(ui, label, !app.update_downloading)
                            .on_hover_text(
                                "Saves the installer to Downloads and starts it; \
                                 it upgrades this installation in place",
                            )
                            .clicked()
                        {
                            actions.push(Action::InstallUpdate);
                        }
                    } else if ui::secondary_button(ui, "Release page", true).clicked() {
                        actions.push(Action::OpenReleasePage);
                    }
                });
            });
        });
}

fn problems(ui: &mut egui::Ui, app: &ManagerApp) {
    ui.add_space(28.0);
    ui.label(ui::caption("Profiles that could not be read"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_of_a_whole_says_the_unit_once() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(of(5 * gib, 8 * gib), "5.0 of 8.0 GiB");
        assert_eq!(of(512 * 1024 * 1024, 8 * gib), "512 MiB of 8.0 GiB");
    }

    #[test]
    fn the_summary_reads_as_a_sentence() {
        assert_eq!(
            summarise([1, 1, 1, 0]),
            "One running, one saved for later, one installing."
        );
        assert_eq!(summarise([0, 0, 0, 4]), "4 off.");
        assert_eq!(summarise([0, 2, 0, 1]), "Two saved for later, one off.");
        assert_eq!(summarise([0, 0, 0, 0]), "");
    }
}
