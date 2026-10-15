//! The Disks view: every disk image the VM profiles reference plus the loose
//! `*.raw` files in the VM directory — sizes (apparent vs actually on disk),
//! partition summary (linked from `disk-image`, never shelled out), the
//! `.nvram` sidecar badge, and the attach/detach/delete/reveal actions.
//!
//! Same rules as every view: pure function of state, intent goes into
//! [`Action`]s, and mutations of a disk whose VM is running are greyed out.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp, View};
use crate::discovery::{format_bytes, DiskRow};
use crate::theme;
use crate::ui;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(super::cards::PAGE_X, 28)),
        )
        .show(ctx, |ui| {
            theme::paint_backdrop(ui);
            super::cards::banners(ui, app, actions);
            ui::storage_header(ui, app, actions);
            toolbar(ui, app, actions);
            ui.add_space(10.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for row in &app.scan.disks {
                        disk_row(ui, app, row, actions);
                        ui.add_space(12.0);
                    }
                    if app.scan.disks.is_empty() {
                        empty_state(ui, app, actions);
                    }
                });
        });
}

/// Heading, the New-disk action and the storage summary of the directories
/// that actually hold the images.
fn toolbar(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(ui::caption("Disk images"));
        ui.add_space(4.0);
        let count = app.scan.disks.len();
        ui.label(ui::dim(format!(
            "{count} image{}",
            if count == 1 { "" } else { "s" }
        )));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui::primary_button(ui, "+  New disk").clicked() {
                actions.push(Action::OpenCreateDisk);
            }
        });
    });

    // One storage chip per directory holding images (plus the VM directory):
    // "…/entangled-vms — 213.4 GiB free of 476.9 GiB".
    let mut dirs: Vec<std::path::PathBuf> = vec![app.settings.vm_dir.clone()];
    for row in &app.scan.disks {
        if let Some(parent) = row.path.parent() {
            if !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_path_buf());
            }
        }
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for dir in dirs {
            if let Some((free, total)) = disk_image::disk_space(&dir) {
                // Space pressure tints the chip: quiet below 80%, warning
                // colour above, error colour above 95%.
                let used = total.saturating_sub(free) as f64 / total.max(1) as f64;
                let tint = if used > 0.95 {
                    theme::ERR
                } else if used > 0.80 {
                    theme::WARN
                } else {
                    theme::TEXT_DIM
                };
                ui::chip(
                    ui,
                    &format!(
                        "{} — {} free of {}",
                        shorten_start(&dir.display().to_string(), 34),
                        format_bytes(free),
                        format_bytes(total)
                    ),
                    tint,
                );
            }
        }
    });
}

fn disk_row(ui: &mut egui::Ui, app: &ManagerApp, row: &DiskRow, actions: &mut Vec<Action>) {
    let busy = app.disk_busy(row);

    // A frame that grows with its content, not a fixed-size card: one image
    // can be attached to several profiles, and every attachment adds a chip
    // and a Detach button. A fixed height once ran them under the actions.
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(egui::Margin::same(ui::CARD_PAD as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Line 1: name, badges, sizes on the right.
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&row.file_name)
                        .size(16.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                if row.nvram {
                    ui::chip(ui, "NVRAM", theme::ACCENT);
                }
                if !row.exists {
                    ui::chip(ui, "missing", theme::ERR);
                }
                if busy {
                    ui::chip(ui, "in use", theme::OK);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut text = format!("{} apparent", format_bytes(row.apparent_bytes));
                    if let Some(allocated) = row.allocated_bytes {
                        text.push_str(&format!(" · {} on disk", format_bytes(allocated)));
                    }
                    ui.label(ui::dim(text));
                });
            });

            // Line 2: partition summary (or why inspection refused the image).
            match &row.summary {
                Ok(summary) => {
                    ui.label(ui::dim(summary.clone()));
                }
                Err(error) => {
                    ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                }
            }
            let path = row.path.display().to_string();
            ui.label(ui::faint(shorten_start(&path, 72)))
                .on_hover_text(&path);
            ui.add_space(4.0);

            // The attachments, wrapping onto as many lines as they need…
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(7.0, 6.0);

                if row.attachments.is_empty() {
                    ui.label(ui::faint("attached to no machine"));
                } else {
                    for attachment in &row.attachments {
                        let mode = if attachment.writable { "rw" } else { "ro" };
                        let running = app.is_busy(&attachment.vm);
                        ui::chip(
                            ui,
                            &format!("{} ({mode})", attachment.vm),
                            if running { theme::OK } else { theme::ACCENT },
                        );
                        if ui::ghost_button(ui, "Detach", !running, theme::WARN)
                            .on_hover_text(if running {
                                "Stop the machine first".to_string()
                            } else {
                                format!("Remove the [[disk]] entry from {}", attachment.vm)
                            })
                            .clicked()
                        {
                            actions.push(Action::DetachDisk {
                                vm: attachment.vm.clone(),
                                profile: attachment.profile.clone(),
                                declared: attachment.declared.clone(),
                            });
                        }
                    }
                }
            });

            // …and the actions on a line of their own, at the right.
            // One button-height line: a bare `with_layout` here would claim
            // the whole remaining height of the page and stretch the card to it.
            ui.add_space(8.0);
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), ui::BUTTON_H),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    if ui::ghost_button(ui, "Delete", row.exists && !busy, theme::ERR)
                        .on_hover_text(if busy {
                            "Stop the machine first"
                        } else {
                            "Remove the image (and its .nvram sidecar)"
                        })
                        .clicked()
                    {
                        actions.push(Action::AskDeleteDisk(row.path.clone()));
                    }
                    if ui::ghost_button(ui, "Grow…", row.exists && !busy, theme::TEXT)
                        .on_hover_text(if busy {
                            "Stop the machine first"
                        } else {
                            "Make the disk look larger to the guest. Sparse, so nothing is \
                             written now; shrinking is never offered because it would \
                             destroy data."
                        })
                        .clicked()
                    {
                        actions.push(Action::AskResizeDisk(row.path.clone()));
                    }
                    if ui::ghost_button(ui, "Move…", row.exists && !busy, theme::TEXT)
                        .on_hover_text(if busy {
                            "Stop the machine first"
                        } else {
                            "Relocate the image (and its .nvram sidecar) to another \
                             directory or drive — sparse-preserving, verified"
                        })
                        .clicked()
                    {
                        actions.push(Action::AskMoveDisk(row.path.clone()));
                    }
                    if ui::ghost_button(ui, "Attach…", row.exists, theme::TEXT)
                        .on_hover_text("Add this image to a stopped machine's profile")
                        .clicked()
                    {
                        actions.push(Action::OpenAttachDisk(row.path.clone()));
                    }
                    if ui::ghost_button(ui, "Reveal", row.exists, theme::TEXT_DIM)
                        .on_hover_text("Show the file in the system file manager")
                        .clicked()
                    {
                        actions.push(Action::Reveal(row.path.clone()));
                    }
                },
            );
        });
}

fn empty_state(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new("No disk images yet")
                .size(19.0)
                .color(theme::TEXT),
        );
        ui.label(ui::dim(format!(
            "{} holds no *.raw images and no profile references one",
            app.settings.vm_dir.display()
        )));
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                if ui::primary_button(ui, "Create a disk").clicked() {
                    actions.push(Action::OpenCreateDisk);
                }
                ui.add_space(6.0);
                if ui::ghost_button(ui, "Machines view", true, theme::TEXT_DIM).clicked() {
                    actions.push(Action::SwitchView(View::Machines));
                }
            });
        });
    });
}

/// Middle-ellipsis keeping the *tail* (the file name matters most).
fn shorten_start(text: &str, max: usize) -> String {
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
    fn shortening_keeps_the_tail() {
        let long = "D:/very/long/path/to/some/machine/desktop.raw";
        let short = shorten_start(long, 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.ends_with("desktop.raw"));
    }
}
