//! The sidebar: the mark, the machines themselves (not a list of features),
//! and the few tools that are about the whole host — Storage, Health,
//! Activity, Settings.
//!
//! A machine is what people come here for, so it is what the sidebar lists:
//! each one with its state in a dot and a few words, one click from its own
//! page.

use egui::{Align, Layout, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::{Action, ManagerApp, Status, View};
use crate::logo;
use crate::theme;
use crate::ui::{self, Icon};

/// The sidebar's width. Wide enough for a machine name and its status line.
const WIDTH: f32 = 248.0;
/// Height of a navigation row: a 44 px target, the minimum for a pointer.
const ROW_H: f32 = 44.0;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    egui::SidePanel::left("sidebar")
        .exact_width(WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .inner_margin(egui::Margin {
                    left: 12,
                    right: 12,
                    top: 18,
                    bottom: 14,
                }),
        )
        .show(ctx, |ui| {
            // The panel's right edge, in the divider colour rather than the
            // stronger widget stroke egui would use.
            let edge = ui.max_rect().right() + 12.0;
            ui.painter().vline(
                edge,
                ui.ctx().screen_rect().y_range(),
                Stroke::new(1.0_f32, theme::DIVIDER),
            );

            brand(ui, actions);
            ui.add_space(14.0);

            // The tools claim the bottom of the column first; the machine list
            // scrolls in whatever is left, so a long list never slides an item
            // underneath them.
            egui::TopBottomPanel::bottom("sidebar-tools")
                .frame(egui::Frame::new())
                .show_separator_line(false)
                .show_inside(ui, |ui| tools(ui, app, actions));

            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(ui::caption("Machines"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if small_icon_button(ui, Icon::Plus, "New machine").clicked() {
                        actions.push(Action::OpenWizard);
                    }
                });
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| machines(ui, app, actions));
        });
}

/// The mark and the wordmark, with Settings beside them: Settings is a dialog
/// about the whole application, not a place to go, so it does not sit in the
/// list of places.
fn brand(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let (mark, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
        logo::paint_mark(ui.painter(), mark, theme::BG_PANEL);
        ui.add_space(2.0);
        ui.label(
            RichText::new("entangled")
                .size(17.0)
                .color(theme::TEXT)
                .strong(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if small_icon_button(ui, Icon::Gear, "Settings").clicked() {
                actions.push(Action::OpenSettings);
            }
        });
    });
}

fn machines(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.spacing_mut().item_spacing.y = 2.0;
    let all = app.view == View::Machines;
    if nav_row(ui, Lead::Icon(Icon::Grid), "All machines", None, all).clicked() && !all {
        actions.push(Action::SwitchView(View::Machines));
    }
    for vm in &app.scan.vms {
        let status = app.status_of(&vm.name);
        let active = matches!(&app.view, View::Machine(name) if *name == vm.name);
        let sub = ui::status_line(app, &vm.name);
        if nav_row(ui, Lead::Status(status), &vm.name, Some(&sub), active).clicked() && !active {
            actions.push(Action::SwitchView(View::Machine(vm.name.clone())));
        }
    }
    for pending in &app.pending {
        if app.scan.vms.iter().any(|vm| vm.name == pending.name) {
            continue;
        }
        let active = matches!(&app.view, View::Machine(name) if *name == pending.name);
        if nav_row(
            ui,
            Lead::Status(Status::Installing),
            &pending.name,
            Some("Installing"),
            active,
        )
        .clicked()
            && !active
        {
            actions.push(Action::SwitchView(View::Machine(pending.name.clone())));
        }
    }
}

/// The host-wide tools at the foot of the column, and one quiet line of host
/// numbers under them.
fn tools(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.add_space(8.0);
    let top = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().x_range(),
        top,
        Stroke::new(1.0_f32, theme::DIVIDER),
    );
    ui.add_space(10.0);
    ui.spacing_mut().item_spacing.y = 2.0;

    let storage = matches!(app.view, View::Disks | View::Snapshots);
    let saved = app.scan.snapshots.len();
    let storage_tag = (saved > 0).then(|| Tag::quiet(saved_label(saved)));
    if tool_row(ui, Icon::Disk, "Storage", storage_tag, storage).clicked() && !storage {
        actions.push(Action::SwitchView(View::Disks));
    }

    let health = app.view == View::Diagnostics;
    if tool_row(ui, Icon::Pulse, "Health", health_tag(app), health).clicked() && !health {
        actions.push(Action::SwitchView(View::Diagnostics));
    }

    // Activity is a panel that opens under the page, not a place: it never
    // takes the "you are here" highlight, which belongs to Storage or Health
    // or a machine. Its tag says whether it is open, and otherwise whether
    // something is producing output.
    let activity_tag = if app.log_open {
        Some(Tag {
            text: "open".to_string(),
            color: theme::ACCENT,
        })
    } else {
        app.supervisor
            .any_active()
            .then(|| Tag::quiet("live".to_string()))
    };
    if tool_row(ui, Icon::List, "Activity", activity_tag, false)
        .on_hover_text(if app.log_open {
            "Hide the activity panel"
        } else {
            "Show installs, starts and console output in a panel under the page"
        })
        .clicked()
    {
        actions.push(Action::ToggleLogPane);
    }

    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        ui.add_space(10.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        let mut parts = Vec::new();
        if let Some(cpu) = app.stats.host.cpu_percent {
            parts.push(format!("CPU {cpu:.0}%"));
        }
        if let Some((free, _)) = app.stats.host.vm_dir_space {
            parts.push(format!("{} free", crate::discovery::format_bytes(free)));
        }
        if !parts.is_empty() {
            ui.label(
                RichText::new(parts.join(" · "))
                    .monospace()
                    .size(11.0)
                    .color(theme::TEXT_FAINT),
            )
            .on_hover_text(format!(
                "This computer's processor, and the space left where machines live ({})",
                app.settings.vm_dir.display()
            ));
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(
            RichText::new(format!("v{}", crate::VERSION))
                .monospace()
                .size(10.5)
                .color(theme::TEXT_FAINT),
        );
        if app.mock_mode {
            ui::chip(ui, "MOCK DATA", theme::WARN);
        }
    });
}

fn saved_label(saved: usize) -> String {
    if saved == 1 {
        "1 saved".to_string()
    } else {
        format!("{saved} saved")
    }
}

/// What Health has to say from the sidebar: nothing when all is well, the
/// worst problem otherwise.
/// It counts exactly the lines the Health page marks as needing a fix, so the
/// two can never disagree.
fn health_tag(app: &ManagerApp) -> Option<Tag> {
    let (count, problem) = crate::ui::diagnostics::to_fix(app);
    if count == 0 {
        return None;
    }
    Some(Tag {
        text: if problem {
            "needs attention".to_string()
        } else {
            format!("{count} to finish")
        },
        color: if problem { theme::ERR } else { theme::WARN },
    })
}

/// A few words at the right end of a tool row.
struct Tag {
    text: String,
    color: egui::Color32,
}

impl Tag {
    fn quiet(text: String) -> Self {
        Self {
            text,
            color: theme::TEXT_FAINT,
        }
    }
}

/// What leads a navigation row: an icon for a place, a dot for a machine.
enum Lead {
    Icon(Icon),
    Status(Status),
}

/// One row of the machine list: lead, name, and an optional second line.
fn nav_row(
    ui: &mut egui::Ui,
    lead: Lead,
    title: &str,
    sub: Option<&str>,
    active: bool,
) -> egui::Response {
    let height = if sub.is_some() { ROW_H + 4.0 } else { ROW_H };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    paint_row_back(ui, rect, &response, active);
    let painter = ui.painter();
    let lead_rect = Rect::from_center_size(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        Vec2::splat(16.0),
    );
    match lead {
        Lead::Icon(icon) => ui::paint_icon(
            painter,
            lead_rect,
            icon,
            if active { theme::TEXT } else { theme::TEXT_DIM },
        ),
        Lead::Status(status) => ui::status_dot(painter, lead_rect.center(), status),
    }
    let x = rect.left() + 38.0;
    let max_w = rect.right() - x - 8.0;
    let title_galley = painter.layout_no_wrap(
        title.to_owned(),
        egui::FontId::proportional(13.5),
        if active || sub.is_some() {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        },
    );
    match sub {
        Some(sub) => {
            let sub_galley = painter.layout(
                sub.to_owned(),
                egui::FontId::proportional(11.5),
                theme::TEXT_FAINT,
                max_w,
            );
            let total = title_galley.size().y + 1.0 + sub_galley.size().y;
            let top = rect.center().y - total * 0.5;
            let sub_top = top + title_galley.size().y + 1.0;
            painter
                .with_clip_rect(rect)
                .galley(egui::pos2(x, top), title_galley, theme::TEXT);
            painter.with_clip_rect(rect).galley(
                egui::pos2(x, sub_top),
                sub_galley,
                theme::TEXT_FAINT,
            );
        }
        None => {
            let y = rect.center().y - title_galley.size().y * 0.5;
            painter
                .with_clip_rect(rect)
                .galley(egui::pos2(x, y), title_galley, theme::TEXT);
        }
    }
    response
}

/// One host-wide tool: icon, name, an optional tag at the right.
fn tool_row(
    ui: &mut egui::Ui,
    icon: Icon,
    title: &str,
    tag: Option<Tag>,
    active: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    paint_row_back(ui, rect, &response, active);
    let painter = ui.painter();
    let fg = if active { theme::TEXT } else { theme::TEXT_DIM };
    ui::paint_icon(
        painter,
        Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            Vec2::splat(16.0),
        ),
        icon,
        fg,
    );
    painter.text(
        egui::pos2(rect.left() + 38.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(13.5),
        fg,
    );
    if let Some(tag) = tag {
        painter.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            tag.text,
            egui::FontId::proportional(11.5),
            tag.color,
        );
    }
    response
}

/// The shared row background: a hover fill, and for the current place a
/// slightly stronger fill with a two-pixel accent edge.
fn paint_row_back(ui: &egui::Ui, rect: Rect, response: &egui::Response, active: bool) {
    let hover = theme::animate_bool(
        ui.ctx(),
        response.id.with("hover"),
        response.hovered(),
        0.10,
    );
    let fill = if active {
        theme::SELECTED
    } else {
        theme::mix(theme::BG_PANEL, theme::CARD_HOVER, hover)
    };
    let radius = egui::CornerRadius::same(8);
    ui.painter().rect_filled(rect, radius, fill);
    if active {
        ui.painter().rect_filled(
            Rect::from_min_size(
                rect.left_top() + Vec2::new(0.0, 10.0),
                Vec2::new(2.0, rect.height() - 20.0),
            ),
            egui::CornerRadius::same(1),
            theme::ACCENT,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// The compact square button beside the Machines caption.
fn small_icon_button(ui: &mut egui::Ui, icon: Icon, hover: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    let t = theme::animate_bool(ui.ctx(), response.id.with("hot"), response.hovered(), 0.1);
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(6),
        Stroke::new(1.0_f32, theme::mix(theme::STROKE, theme::STROKE_STRONG, t)),
        egui::StrokeKind::Inside,
    );
    ui::paint_icon(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(12.0)),
        icon,
        theme::mix(theme::TEXT_DIM, theme::TEXT, t),
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.on_hover_text(hover)
}
