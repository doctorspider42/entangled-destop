//! The console/installer log pane (GUI-1605): whatever the tracked children
//! wrote, tailed live from their log files.

use egui::{Align, Layout, RichText};

use crate::app::{Action, ManagerApp};
use crate::process::TaskState;
use crate::theme;
use crate::ui;

/// Lines rendered at once; the whole log stays on disk.
const VISIBLE_LINES: usize = 600;

pub fn show(ctx: &egui::Context, app: &mut ManagerApp, actions: &mut Vec<Action>) {
    let ManagerApp {
        supervisor,
        log_selected,
        log_follow,
        ..
    } = app;

    egui::TopBottomPanel::bottom("log")
        .resizable(true)
        .default_height(230.0)
        .min_height(120.0)
        .max_height(560.0)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .inner_margin(egui::Margin::symmetric(18, 12)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("CONSOLE").size(12.0).color(theme::TEXT_DIM));
                ui.add_space(10.0);

                let selected_label = log_selected
                    .and_then(|id| supervisor.task(id))
                    .map(describe)
                    .unwrap_or_else(|| "no task".to_string());
                egui::ComboBox::from_id_salt("log-task")
                    .selected_text(selected_label)
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for task in supervisor.tasks().iter().rev() {
                            let mut selected = *log_selected == Some(task.id);
                            if ui
                                .selectable_value(&mut selected, true, describe(task))
                                .clicked()
                            {
                                *log_selected = Some(task.id);
                            }
                        }
                    });

                ui.checkbox(log_follow, "Follow");

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::ghost_button(ui, "Hide", true, theme::TEXT_DIM).clicked() {
                        actions.push(Action::ToggleLogPane);
                    }
                    if let Some(task) = log_selected.and_then(|id| supervisor.task(id)) {
                        let path = task.log_path.display().to_string();
                        if ui::ghost_button(ui, "Copy log path", true, theme::TEXT_DIM)
                            .on_hover_text(&path)
                            .clicked()
                        {
                            actions.push(Action::CopyProfilePath(path));
                        }
                        ui.label(ui::faint(&task.command_line))
                            .on_hover_text(&task.command_line);
                    }
                });
            });

            ui.add_space(8.0);

            let Some(task) = log_selected.and_then(|id| supervisor.task(id)) else {
                ui.label(ui::dim(
                    "Nothing has been started yet. Start a machine or create a new one and its \
                     console appears here.",
                ));
                return;
            };
            let (lines, dropped) = task.log_tail(VISIBLE_LINES);

            egui::Frame::new()
                .fill(theme::INSET)
                .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(*log_follow)
                        .show(ui, |ui| {
                            ui.style_mut().spacing.item_spacing.y = 1.0;
                            if dropped > 0 {
                                ui.label(ui::faint(format!(
                                    "… {dropped} earlier lines are only in {}",
                                    task.log_path.display()
                                )));
                            }
                            if lines.is_empty() {
                                ui.label(ui::faint("waiting for output…"));
                            }
                            for line in &lines {
                                ui.label(
                                    RichText::new(line)
                                        .monospace()
                                        .size(12.0)
                                        .color(line_color(line)),
                                );
                            }
                        });
                });
        });
}

fn describe(task: &crate::process::Task) -> String {
    let state = match task.state() {
        TaskState::Running => format!("running {}", elapsed(task.started_at.elapsed())),
        TaskState::Stopping => "stopping".to_string(),
        TaskState::Finished(outcome) => outcome.detail,
    };
    format!("{} {} · {state}", task.kind.label(), task.vm)
}

/// Compact duration ("18s", "4m12s", "1h07m").
fn elapsed(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

/// Colour by severity, recognising both the CLI's `tracing` output and the
/// guest's kernel log.
fn line_color(line: &str) -> egui::Color32 {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("panic") || lower.contains("failed") {
        theme::ERR
    } else if lower.contains("warn") {
        theme::WARN
    } else if lower.contains(" info ") || lower.starts_with("info") {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_compact() {
        use std::time::Duration;
        assert_eq!(elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(elapsed(Duration::from_secs(252)), "4m12s");
        assert_eq!(elapsed(Duration::from_secs(3600 + 7 * 60)), "1h07m");
    }

    #[test]
    fn severity_colouring_picks_out_failures() {
        assert_eq!(line_color("error: cannot open TAP"), theme::ERR);
        assert_eq!(line_color("WARN cannot open a window"), theme::WARN);
        assert_eq!(line_color("2026-08-19 INFO vm running"), theme::TEXT);
        assert_eq!(line_color("[    0.000000] Linux version"), theme::TEXT_DIM);
    }
}
