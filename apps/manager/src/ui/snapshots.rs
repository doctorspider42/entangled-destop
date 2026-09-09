//! The Snapshots view: every saved session in the VM directory — which machine
//! it is of, when it was taken, how big it is on paper and on disk, whether it
//! could be resumed here, and the two things that can be done with one
//! ([ADR-0006](../../../../docs/adr/0006-suspend-restore.md)).
//!
//! Shaped like the Disks view on purpose: full-width rows, a title line with
//! badges and sizes, a detail line, the path, then actions on the right. A
//! snapshot is another large file in the same directory that a person has to
//! reason about and eventually delete, and giving it a second visual language
//! would only mean learning two.
//!
//! The one thing this view does that Disks does not is **explain a refusal
//! before the click**. A snapshot is bound to its host, its build and its disks
//! (ADR-0006 §5), so a row is often something that cannot be used; each reason
//! is a plain sentence under the row rather than an error a child process
//! produces afterwards.

use egui::{Align, Layout, RichText, Vec2};

use crate::app::{Action, ManagerApp, View};
use crate::discovery::format_bytes;
use crate::snapshots::SnapshotRow;
use crate::theme;
use crate::ui;

const ROW_HEIGHT: f32 = 146.0;

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            theme::paint_backdrop(ui);
            super::cards::banners(ui, app, actions);
            toolbar(ui, app);
            ui.add_space(10.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for row in &app.scan.snapshots {
                        snapshot_row(ui, app, row, actions);
                        ui.add_space(12.0);
                    }
                    if app.scan.snapshots.is_empty() {
                        empty_state(ui, app, actions);
                    }
                });
        });
}

/// Heading and the totals, because "how much is this costing me" is the
/// question a list of large files is usually opened with.
fn toolbar(ui: &mut egui::Ui, app: &ManagerApp) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Saved sessions")
                .size(19.0)
                .color(theme::TEXT)
                .strong(),
        );
        ui.add_space(4.0);
        let count = app.scan.snapshots.len();
        ui.label(ui::dim(format!(
            "{count} snapshot{}",
            if count == 1 { "" } else { "s" }
        )));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let apparent: u64 = app.scan.snapshots.iter().map(|r| r.apparent_bytes).sum();
            let allocated: u64 = app
                .scan
                .snapshots
                .iter()
                .filter_map(|r| r.allocated_bytes)
                .sum();
            if apparent > 0 {
                ui::chip(
                    ui,
                    &format!("{} on disk", format_bytes(allocated)),
                    theme::VIOLET,
                );
                ui::chip(
                    ui,
                    &format!("{} apparent", format_bytes(apparent)),
                    theme::TEXT_DIM,
                );
            }
        });
    });
    ui.add_space(6.0);
    ui.label(ui::dim(
        "A suspended machine is not shut down: everything it had open is in one of these \
         files. Resuming puts it back; deleting one loses only what it was doing.",
    ));
}

fn snapshot_row(ui: &mut egui::Ui, app: &ManagerApp, row: &SnapshotRow, actions: &mut Vec<Action>) {
    let id = egui::Id::new(("snapshot-row", &row.path));
    let accent_at = accent_for(&row.file_name);
    let verdict = app.snapshot_verdict(row);
    let vm_name = row.vm_name().unwrap_or("unknown machine").to_string();
    let busy = app.is_busy(&vm_name);
    let width = ui.available_width();

    // A refusal is a whole sentence and it wraps: how many lines depends on the
    // reason, the window width and the font, so the row's height is measured
    // rather than declared. A fixed height was the first attempt — the
    // foreign-hypervisor refusal, which is two lines at any ordinary window
    // size, ran straight under the buttons.
    let height = {
        let mut probe = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("snapshot-row-sizing", &row.path))
                .max_rect(egui::Rect::from_min_size(
                    ui.cursor().min,
                    Vec2::new(width - 2.0 * ui::CARD_PAD, 4000.0),
                ))
                .layout(Layout::top_down(Align::Min))
                .sizing_pass()
                .invisible(),
        );
        snapshot_row_body(&mut probe, row, &verdict, &vm_name, busy, &mut Vec::new());
        (probe.min_rect().height() + 2.0 * ui::CARD_PAD).max(ROW_HEIGHT)
    };

    ui::card(ui, id, Vec2::new(width, height), accent_at, |ui, _| {
        snapshot_row_body(ui, row, &verdict, &vm_name, busy, actions);
    });
}

fn snapshot_row_body(
    ui: &mut egui::Ui,
    row: &SnapshotRow,
    verdict: &crate::snapshots::Verdict,
    vm_name: &str,
    busy: bool,
    actions: &mut Vec<Action>,
) {
    // Line 1: the machine it is of, badges, sizes on the right.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(vm_name)
                .size(16.0)
                .color(theme::TEXT)
                .strong(),
        );
        match &row.facts {
            Ok(facts) => {
                ui::chip(ui, facts.host.as_str(), theme::CYAN);
                if verdict.resumable() {
                    ui::chip(ui, "resumable", theme::OK);
                } else {
                    ui::chip(ui, "cannot resume here", theme::WARN);
                }
            }
            Err(_) => ui::chip(ui, "unreadable", theme::ERR),
        }
        if busy {
            ui::chip(ui, "machine is running", theme::OK);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut text = format!("{} apparent", format_bytes(row.apparent_bytes));
            if let Some(allocated) = row.allocated_bytes {
                text.push_str(&format!(" · {} on disk", format_bytes(allocated)));
            }
            ui.label(ui::dim(text)).on_hover_text(
                "A snapshot holds only the memory the guest had actually touched, and is \
                     written sparse — so what it occupies is smaller again than its size.",
            );
        });
    });

    // Line 2: when, and the machine's shape. A file that cannot be read has
    // neither, and its one sentence is the refusal below — printing the raw
    // parser error here as well said the same thing twice, in two voices.
    if let Ok(facts) = &row.facts {
        let taken = vm_snapshot::meta::format_unix(facts.created_unix);
        ui.label(ui::dim(format!(
            "{} · taken {}",
            row.shape_line(),
            vm_snapshot::meta::describe_age(facts.created_unix, vm_snapshot::meta::now_unix())
        )))
        .on_hover_text(format!("{taken}\nwritten by {}", facts.writer));
    }
    let path = row.path.display().to_string();
    ui.label(ui::faint(shorten_start(&path, 72)))
        .on_hover_text(&path);

    // Line 3: why it cannot be used, or what is merely worth knowing. Whole
    // sentences, wrapped — this is the one surface where a refusal is read
    // rather than hovered, and the row grows to hold it.
    ui.add_space(4.0);
    for reason in verdict.blocked.iter().take(2) {
        ui.label(RichText::new(reason).color(theme::WARN).size(11.5));
    }
    if verdict.blocked.is_empty() {
        for note in verdict.notes.iter().take(1) {
            ui.label(ui::faint(note.clone()));
        }
    }

    // Line 4: the actions, on the row's own last line.
    ui.add_space(8.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        if ui::ghost_button(ui, "Delete", !busy, theme::ERR)
            .on_hover_text(if busy {
                "The machine is running — it may be about to write here"
            } else {
                "Throw the saved session away. The machine and its disks stay."
            })
            .clicked()
        {
            actions.push(Action::AskDeleteSnapshot(row.path.clone()));
        }
        if ui::ghost_button(ui, "Reveal", true, theme::TEXT_DIM)
            .on_hover_text("Show the file in the system file manager")
            .clicked()
        {
            actions.push(Action::Reveal(row.path.clone()));
        }
        let resumable = verdict.resumable() && !busy;
        let hover = if busy {
            format!("'{vm_name}' is already running")
        } else if resumable {
            format!("Start '{vm_name}' from this session instead of booting it")
        } else {
            verdict.blocked.join("\n\n")
        };
        if ui::ghost_button(ui, "Resume", resumable, theme::VIOLET)
            .on_hover_text(hover)
            .clicked()
        {
            actions.push(Action::ResumeSnapshot(row.path.clone()));
        }
    });
}

fn empty_state(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new("No saved sessions")
                .size(19.0)
                .color(theme::TEXT),
        );
        ui.label(ui::dim(format!(
            "{} holds no *.{} files",
            app.settings.vm_dir.display(),
            vm_snapshot::EXTENSION
        )));
        ui.add_space(6.0);
        ui.label(ui::faint(
            "Suspend a running machine — from its card, or with Ctrl+Alt+S in its own \
             window — and it appears here.",
        ));
        ui.add_space(14.0);
        if ui::ghost_button(ui, "Machines view", true, theme::CYAN).clicked() {
            actions.push(Action::SwitchView(View::Machines));
        }
    });
}

/// Stable per-file position on the accent ramp (same trick as the VM cards).
fn accent_for(name: &str) -> f32 {
    let sum: u32 = name.bytes().map(u32::from).sum();
    (sum % 100) as f32 / 100.0
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
    fn accents_are_stable_and_shortening_keeps_the_file_name() {
        assert_eq!(accent_for("a.esnap"), accent_for("a.esnap"));
        assert!((0.0..1.0).contains(&accent_for("ubuntu-lab.esnap")));
        let long = "D:/very/long/path/to/some/machine/ubuntu-lab.esnap";
        let short = shorten_start(long, 24);
        assert_eq!(short.chars().count(), 24);
        assert!(short.ends_with("ubuntu-lab.esnap"));
    }
}
