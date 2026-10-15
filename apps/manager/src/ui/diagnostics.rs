//! Health: whether this computer is ready to run machines, as a short
//! checklist a person can act on — each line a plain sentence, each problem
//! with its fix as a button.
//!
//! The facts still come from `entangled doctor`: the CLI knows them per host,
//! and duplicating that knowledge here would guarantee the two drift. What this
//! view adds is the translation — a `MISSING` line becomes "Startup files for
//! Ubuntu are missing · Download", not a command to type. The raw report stays
//! one click away under Technical details, for whoever wants it.

use egui::{Align, Layout, RichText, Sense, Vec2};

use crate::app::{Action, FetchKind, ManagerApp};
use crate::backend::Backend;
use crate::hostcheck::Level;
use crate::theme;
use crate::ui;

/// How much a line of the checklist matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ok,
    /// Works without it; offered, never nagged about.
    Optional,
    /// Something a common task will need.
    Attention,
    /// Machines cannot run until it is fixed.
    Problem,
    /// Still being found out.
    Pending,
}

/// One line of the checklist.
pub struct Check {
    pub state: State,
    pub title: String,
    pub detail: String,
    pub action: Option<CheckAction>,
}

pub struct CheckAction {
    pub label: &'static str,
    pub action: Option<Action>,
    pub hover: String,
}

/// Every line of the checklist, worst first. Shared with the sidebar, whose
/// "N to fix" counts exactly the lines shown here as needing a fix.
pub fn checks(app: &ManagerApp) -> Vec<Check> {
    let mut out = Vec::new();

    match &app.engine {
        Ok(engine) => out.push(Check {
            state: State::Ok,
            title: "Entangled is installed".to_string(),
            detail: match &engine.version {
                Some(version) => format!("Version {version}"),
                None => "Ready to run machines".to_string(),
            },
            action: None,
        }),
        Err(_) => out.push(Check {
            state: State::Problem,
            title: "Part of Entangled is missing".to_string(),
            detail: "Machines cannot start until it is back. Reinstalling Entangled Desktop \
                     fixes it."
                .to_string(),
            action: Some(CheckAction {
                label: "Locate it…",
                action: Some(Action::PickPath(crate::picker::PickTarget::EngineBinary)),
                hover: "Point Entangled at a copy you already have".to_string(),
            }),
        }),
    }

    match &app.doctor {
        None if app.doctor_running || app.engine.is_ok() => out.push(Check {
            state: State::Pending,
            title: "Checking this computer…".to_string(),
            detail: "Virtualization, startup files and installer media".to_string(),
            action: None,
        }),
        None => {}
        Some(report) => {
            if !report.healthy {
                let reason = report
                    .lines
                    .iter()
                    .find(|line| line.level == Level::Failure)
                    .map(|line| plain(&line.text))
                    .unwrap_or_else(|| "The check did not finish.".to_string());
                out.push(Check {
                    state: State::Problem,
                    title: "This computer cannot run machines yet".to_string(),
                    detail: reason,
                    action: None,
                });
            } else if let Some(line) = report
                .lines
                .iter()
                .find(|line| line.text.trim_start().starts_with("hypervisor"))
            {
                out.push(Check {
                    state: State::Ok,
                    title: "Virtualization is turned on".to_string(),
                    detail: value_of(&line.text),
                    action: None,
                });
            }
            for line in report.lines.iter().filter(|l| l.level == Level::Missing) {
                if let Some(check) = missing_check(app, &line.text) {
                    out.push(check);
                }
            }
        }
    }

    if let Some((free, _)) = app.stats.host.vm_dir_space {
        out.push(Check {
            state: if free < 20 * 1024 * 1024 * 1024 {
                State::Attention
            } else {
                State::Ok
            },
            title: "Room for machines".to_string(),
            detail: format!(
                "{} free in {}",
                crate::discovery::format_bytes(free),
                app.settings.vm_dir.display()
            ),
            action: None,
        });
    }

    if Backend::Wsl.available_on_host() {
        out.push(wsl_check(app));
    }

    out.sort_by_key(|check| match check.state {
        State::Problem => 0,
        State::Attention => 1,
        State::Pending => 2,
        State::Optional => 3,
        State::Ok => 4,
    });
    out
}

/// A `MISSING` line from `doctor`, as a sentence and a button. The WSL line is
/// left to [`wsl_check`], which knows more than the report does.
fn missing_check(app: &ManagerApp, text: &str) -> Option<Check> {
    let lower = text.to_lowercase();
    if lower.trim_start().starts_with("wsl") || lower.contains("engines") {
        return None;
    }
    let fetching = app.fetch_running;
    let fetch = |kind: FetchKind, label: &'static str| CheckAction {
        label: if fetching == Some(kind) {
            "Downloading…"
        } else {
            label
        },
        action: (fetching.is_none()).then_some(Action::Fetch(kind)),
        hover: "Downloaded from this version's release and checked against the fingerprint \
                built into Entangled"
            .to_string(),
    };
    if lower.contains("bootstrap kernel") {
        return Some(Check {
            state: State::Optional,
            title: "Debian installer files are not downloaded".to_string(),
            detail: "Needed only to install Debian · about 13 MB".to_string(),
            action: Some(fetch(FetchKind::BootstrapKernel, "Download")),
        });
    }
    if lower.contains("firmware") {
        return Some(Check {
            state: State::Attention,
            title: "Startup files are missing".to_string(),
            detail: "Ubuntu and every machine that starts like a PC need them · 4 MB".to_string(),
            action: Some(fetch(FetchKind::Firmware, "Download")),
        });
    }
    if lower.contains("iso") {
        return Some(Check {
            state: State::Optional,
            title: "No Ubuntu installer downloaded yet".to_string(),
            detail: "The New machine wizard downloads and checks it when you need it".to_string(),
            action: None,
        });
    }
    Some(Check {
        state: State::Attention,
        title: plain(text),
        detail: String::new(),
        action: None,
    })
}

/// WSL, which only some machines need: ready, or one button away.
fn wsl_check(app: &ManagerApp) -> Check {
    use crate::wslengine::Status;
    let used = app.settings.default_backend == Backend::Wsl
        || app
            .scan
            .vms
            .iter()
            .any(|vm| app.backend_of(&vm.name) == Backend::Wsl);
    let needed = if used {
        State::Attention
    } else {
        State::Optional
    };
    match &app.wsl_engine {
        Status::Ready(_) => Check {
            state: State::Ok,
            title: "WSL is set up".to_string(),
            detail: format!("Machines can also run in {}", app.settings.wsl_distro),
            action: None,
        },
        Status::Checking | Status::Unknown => Check {
            state: State::Pending,
            title: "Checking WSL…".to_string(),
            detail: String::new(),
            action: None,
        },
        Status::Failed(fault) if app.wsl_engine.installable() => {
            let block = crate::wslengine::install_block();
            Check {
                state: needed,
                title: if used {
                    "WSL is not set up yet".to_string()
                } else {
                    "WSL is not set up".to_string()
                },
                detail: if used {
                    "A machine is set to run in WSL; it needs a one-time setup first".to_string()
                } else {
                    "Optional — only for OpenGL 3D and bridged networking".to_string()
                },
                action: Some(CheckAction {
                    label: if app.wsl_install_running {
                        "Setting up…"
                    } else {
                        "Set up WSL"
                    },
                    action: (!app.wsl_install_running && block.is_none())
                        .then_some(Action::InstallWslEngine),
                    hover: block.unwrap_or_else(|| {
                        format!(
                            "Copies the Linux part of Entangled into {}. {}",
                            app.settings.wsl_distro,
                            plain(&fault.what)
                        )
                    }),
                }),
            }
        }
        Status::Failed(fault) => Check {
            state: needed,
            title: "WSL is not available".to_string(),
            detail: plain(&fault.what),
            action: Some(CheckAction {
                label: "Check again",
                action: Some(Action::CheckWslEngine),
                hover: plain(&fault.fix),
            }),
        },
    }
}

/// What a `key : value` line of the report says, without the key.
fn value_of(text: &str) -> String {
    match text.split_once(':') {
        Some((_, value)) => plain(value),
        None => plain(text),
    }
}

/// Report text, tidied for a sentence: no markers, no surrounding space, a
/// capital first letter.
fn plain(text: &str) -> String {
    let text = text.replace("MISSING —", "").replace("MISSING", "");
    let text = text.trim().trim_start_matches("error:").trim();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// How many lines need fixing — what the sidebar shows beside Health.
pub fn to_fix(app: &ManagerApp) -> (usize, bool) {
    let checks = checks(app);
    let problems = checks.iter().filter(|c| c.state == State::Problem).count();
    let attention = checks
        .iter()
        .filter(|c| c.state == State::Attention)
        .count();
    (problems + attention, problems > 0)
}

pub fn show(ctx: &egui::Context, app: &ManagerApp, actions: &mut Vec<Action>) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin::symmetric(super::cards::PAGE_X, 28)),
        )
        .show(ctx, |ui| {
            theme::paint_backdrop(ui);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let checks = checks(app);
                    heading(ui, app, actions);
                    ui.add_space(20.0);
                    summary(ui, &checks);
                    ui.add_space(16.0);
                    checklist(ui, &checks, actions);
                    ui.add_space(20.0);
                    technical_details(ui, app, actions);
                    ui.add_space(24.0);
                });
        });
}

fn heading(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Health")
                    .size(24.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new("Whether this computer is ready to run machines.")
                    .size(13.5)
                    .color(theme::TEXT_DIM),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if app.doctor_running {
                "Checking…"
            } else {
                "Check again"
            };
            if ui::secondary_button(ui, label, !app.doctor_running).clicked() && !app.doctor_running
            {
                actions.push(Action::RunDiagnostics);
            }
        });
    });
}

/// The verdict in one sentence, with a ring that fills with the share of
/// lines that are fine.
fn summary(ui: &mut egui::Ui, checks: &[Check]) {
    let problems = checks.iter().filter(|c| c.state == State::Problem).count();
    let attention = checks
        .iter()
        .filter(|c| c.state == State::Attention)
        .count();
    let pending = checks.iter().any(|c| c.state == State::Pending);
    let (headline, detail, color) = if problems > 0 {
        (
            "Machines cannot run yet".to_string(),
            "Fix the line marked in red below.".to_string(),
            theme::ERR,
        )
    } else if attention > 0 {
        (
            "Ready, with one thing to finish".to_string(),
            "Machines run; the item below is what a common task will ask for.".to_string(),
            theme::WARN,
        )
    } else if pending {
        (
            "Checking…".to_string(),
            "This takes a few seconds.".to_string(),
            theme::TEXT_DIM,
        )
    } else {
        (
            "Ready to run machines".to_string(),
            "Everything Entangled needs is in place.".to_string(),
            theme::OK,
        )
    };
    let headline = if attention > 1 && problems == 0 {
        format!("Ready, with {attention} things to finish")
    } else {
        headline
    };
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(22, 18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
                let c = rect.center();
                let painter = ui.painter();
                painter.circle_stroke(c, 22.0, egui::Stroke::new(4.0_f32, theme::STROKE));
                let fine = checks
                    .iter()
                    .filter(|c| matches!(c.state, State::Ok | State::Optional))
                    .count();
                let share = fine as f32 / checks.len().max(1) as f32;
                let steps = (48.0 * share).round() as usize;
                if steps >= 1 {
                    let points: Vec<egui::Pos2> = (0..=steps)
                        .map(|i| {
                            let a = -std::f32::consts::FRAC_PI_2
                                + std::f32::consts::TAU * share * i as f32 / steps as f32;
                            c + Vec2::angled(a) * 22.0
                        })
                        .collect();
                    painter.add(egui::Shape::line(points, egui::Stroke::new(4.0_f32, color)));
                }
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(headline)
                            .size(18.0)
                            .color(theme::TEXT)
                            .strong(),
                    );
                    ui.label(RichText::new(detail).size(13.5).color(theme::TEXT_DIM));
                });
            });
        });
}

fn checklist(ui: &mut egui::Ui, checks: &[Check], actions: &mut Vec<Action>) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (index, check) in checks.iter().enumerate() {
                if index > 0 {
                    theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());
                }
                check_row(ui, check, actions);
            }
        });
}

fn check_row(ui: &mut egui::Ui, check: &Check, actions: &mut Vec<Action>) {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(20, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                state_mark(ui.painter(), rect, check.state, ui.ctx());
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(&check.title).size(14.0).color(theme::TEXT));
                    if !check.detail.is_empty() {
                        ui.label(
                            RichText::new(&check.detail)
                                .size(12.5)
                                .color(theme::TEXT_DIM),
                        );
                    }
                });
                if let Some(action) = &check.action {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let enabled = action.action.is_some();
                        let button = if check.state == State::Problem && enabled {
                            ui::primary_button(ui, action.label)
                        } else {
                            ui::secondary_button(ui, action.label, enabled)
                        };
                        if button.on_hover_text(&action.hover).clicked() {
                            if let Some(action) = &action.action {
                                actions.push(action.clone());
                            }
                        }
                    });
                }
            });
        });
}

/// A check for done, a dot for optional, an exclamation for the rest, a
/// turning arc while it is being found out.
fn state_mark(painter: &egui::Painter, rect: egui::Rect, state: State, ctx: &egui::Context) {
    let c = rect.center();
    let (color, fill) = match state {
        State::Ok => (theme::OK, 0.14),
        State::Optional => (theme::TEXT_DIM, 0.10),
        State::Attention => (theme::WARN, 0.16),
        State::Problem => (theme::ERR, 0.18),
        State::Pending => (theme::TEXT_DIM, 0.0),
    };
    if state == State::Pending {
        crate::logo::paint_spinner(painter, rect, theme::animation_time(ctx), color);
        return;
    }
    painter.circle_filled(c, 13.0, theme::mix(theme::CARD, color, fill));
    let stroke = egui::Stroke::new(1.8_f32, color);
    match state {
        State::Ok => {
            painter.add(egui::Shape::line(
                vec![
                    c + Vec2::new(-5.0, 0.5),
                    c + Vec2::new(-1.5, 4.0),
                    c + Vec2::new(5.5, -3.5),
                ],
                stroke,
            ));
        }
        State::Optional => {
            painter.circle_filled(c, 2.5, color);
        }
        _ => {
            painter.line_segment([c + Vec2::new(0.0, -5.5), c + Vec2::new(0.0, 1.5)], stroke);
            painter.circle_filled(c + Vec2::new(0.0, 5.0), 1.2, color);
        }
    }
}

/// One line of the compact report: a check's name and its verdict, with the
/// whole paragraph `doctor` wrote about it kept for the hover.
#[derive(Debug, PartialEq, Eq)]
struct ReportRow {
    key: String,
    summary: String,
    full: String,
    missing: bool,
}

/// Folds `doctor`'s report into one row per check. A top-level line is
/// `  key : value`; every more-indented line after it belongs to the same
/// check. The verdict is the value up to its first explanation (` — `, ` (`,
/// `, `), which is where the CLI starts telling a developer *why*. A check
/// with anything missing says so instead.
fn compact_report(lines: &[crate::hostcheck::Line]) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = Vec::new();
    for line in lines {
        let text = line.text.as_str();
        let missing = line.level == Level::Missing;
        match report_entry(text) {
            Some((key, value)) => {
                let own_missing = value.trim_start().starts_with("MISSING");
                rows.push(ReportRow {
                    key,
                    summary: if own_missing {
                        "missing".to_string()
                    } else {
                        verdict(&value)
                    },
                    full: value.trim().to_string(),
                    missing,
                });
            }
            None => {
                let indent = text.len() - text.trim_start().len();
                let Some(row) = rows.last_mut().filter(|_| indent > 4) else {
                    continue;
                };
                row.full.push('\n');
                row.full.push_str(text.trim());
                if missing && !row.missing {
                    row.missing = true;
                    let what = text.split_whitespace().next().unwrap_or("something");
                    row.summary = format!("{what} missing");
                }
            }
        }
    }
    rows
}

/// `(key, value)` when `text` starts a check of its own. Two shapes:
///
/// - `  key : value` — a top-level check. The separator is a colon followed by
///   a space, so the `C:` of a Windows path inside a value never splits it.
/// - `    key              value` — an item of a section (`install`'s
///   firmware, ISO, VM directory), four spaces in.
fn report_entry(text: &str) -> Option<(String, String)> {
    let indent = text.len() - text.trim_start().len();
    if (1..=3).contains(&indent) {
        let (key, value) = text.trim_start().split_once(": ")?;
        let key = key.trim();
        return (!key.is_empty() && key.len() <= 20).then(|| (key.to_string(), value.to_string()));
    }
    if indent == 4 {
        // Items are padded into a column, but not always the same one, and
        // "bootstrap kernel MISSING" has a single space before its value. So:
        // the key ends at the first run of two spaces, or before MISSING.
        let rest = &text[4..];
        let end = rest.find("  ").or_else(|| rest.find(" MISSING"))?;
        let key = rest[..end].trim();
        return (!key.is_empty()).then(|| (key.to_string(), rest[end..].to_string()));
    }
    None
}

/// The first clause of a value: what it is, not why.
fn verdict(value: &str) -> String {
    let value = value.trim();
    let cut = [" — ", " (", ", ", "; "]
        .iter()
        .filter_map(|sep| value.find(sep))
        .min()
        .unwrap_or(value.len());
    let short = value[..cut].trim().trim_end_matches(',');
    // Columns line up in a terminal and look like gaps here.
    let short = short.split_whitespace().collect::<Vec<_>>().join(" ");
    if short.chars().count() > 48 {
        let kept: String = short.chars().take(47).collect();
        format!("{kept}…")
    } else {
        short.to_string()
    }
}

/// The report in short, and where Entangled found itself — collapsed, for
/// whoever is asked to send them. The full text is one click away.
fn technical_details(ui: &mut egui::Ui, app: &ManagerApp, actions: &mut Vec<Action>) {
    egui::CollapsingHeader::new(
        RichText::new("Technical details")
            .size(12.5)
            .color(theme::TEXT_DIM),
    )
    .id_salt("health-technical")
    .default_open(false)
    .show(ui, |ui| {
        let Some(report) = &app.doctor else {
            return;
        };
        let rows = compact_report(&report.lines);
        egui::Frame::new()
            .fill(theme::INSET)
            .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // A list of facts, not of controls: rows as tall as their text.
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.spacing_mut().interact_size.y = 18.0;
                if let Ok(engine) = &app.engine {
                    let version = engine.version.as_deref().unwrap_or("?");
                    detail_row(
                        ui,
                        "program",
                        &format!("Entangled {version} · {}", engine.origin.label()),
                        &engine.path.display().to_string(),
                        false,
                    );
                }
                for row in &rows {
                    detail_row(ui, &row.key, &row.summary, &row.full, row.missing);
                }
            });
        ui.add_space(6.0);
        let full = std::iter::once(report.command.clone())
            .chain(report.lines.iter().map(|line| line.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        if ui::text_button(ui, "Copy full report", theme::ACCENT)
            .on_hover_text("For a bug report: every line, as the check wrote it")
            .clicked()
        {
            actions.push(Action::CopyProfilePath(full));
        }
    });
}

/// `key ........ verdict`, the whole text on hover.
fn detail_row(ui: &mut egui::Ui, key: &str, summary: &str, full: &str, missing: bool) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(130.0, 16.0), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            key,
            egui::FontId::monospace(11.5),
            theme::TEXT_FAINT,
        );
        let label = ui.add(
            egui::Label::new(RichText::new(summary).size(12.5).color(if missing {
                theme::WARN
            } else {
                theme::TEXT_DIM
            }))
            .selectable(false)
            .truncate(),
        );
        if !full.is_empty() {
            label.on_hover_text(full);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_folds_into_one_row_per_check() {
        let report = crate::hostcheck::mock_report();
        let rows = compact_report(&report.lines);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert!(keys.contains(&"hypervisor"), "{keys:?}");
        assert!(
            !keys.contains(&"entangled doctor"),
            "the header is not a check"
        );
        let hv = rows
            .iter()
            .find(|r| r.key == "hypervisor")
            .expect("hypervisor");
        assert_eq!(hv.summary, "Windows Hypervisor Platform present");
        let net = rows
            .iter()
            .find(|r| r.key == "networking")
            .expect("networking");
        assert_eq!(net.summary, "backend = \"usernet\"");
        let engines = rows.iter().find(|r| r.key == "engines").expect("engines");
        assert!(
            engines.missing && engines.summary == "wsl missing",
            "{engines:?}"
        );
        assert!(
            engines.full.contains("Settings > Linux engine"),
            "the hover keeps it all"
        );
    }

    /// The shape of a real Windows report: paths with drive letters inside
    /// values, and `install`'s items at the second level.
    #[test]
    fn windows_paths_and_section_items_fold_correctly() {
        // Built line by line: a `\` continuation would strip the indentation
        // the parser reads. Columns copied from a real report.
        let text = [
            "entangled doctor",
            "  hypervisor      : Windows Hypervisor Platform present",
            r"  engines         : windows  F:\x\entangled.exe (0.2.0) — this program",
            "                    wsl      MISSING — Ubuntu has no Entangled engine",
            "  3D              : venus    ready — `[display] venus = true`",
            "  install         : ubuntu — UEFI + verified ISO, offline",
            r"    firmware         C:\Users\p\CLOUDHV.fd (4.0 MiB, from the verified cache)",
            "    bootstrap kernel MISSING — needed by `install debian` only",
            "                    run `entangled fetch bootstrap-kernel`",
            r"    ubuntu ISO      MISSING in C:\Users\p\ubuntu",
            r"    VM directory    F:\VMs\Entangled (56.2 GiB free of 931 GiB)",
            "host looks ready to run VMs",
        ]
        .join("\n");
        let rows = compact_report(&crate::hostcheck::parse(&text, true));
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "hypervisor",
                "engines",
                "3D",
                "install",
                "firmware",
                "bootstrap kernel",
                "ubuntu ISO",
                "VM directory"
            ]
        );
        let by = |key: &str| rows.iter().find(|r| r.key == key).expect(key);
        assert_eq!(by("engines").summary, "wsl missing");
        assert_eq!(by("3D").summary, "venus ready");
        assert_eq!(by("firmware").summary, r"C:\Users\p\CLOUDHV.fd");
        assert_eq!(by("bootstrap kernel").summary, "missing");
        assert!(by("bootstrap kernel").missing);
        assert_eq!(by("ubuntu ISO").summary, "missing");
        assert_eq!(by("VM directory").summary, r"F:\VMs\Entangled");
    }

    #[test]
    fn a_verdict_stops_before_the_explanation() {
        assert_eq!(verdict(" yes — CloudHv firmware via the PVH entry"), "yes");
        assert_eq!(verdict("1 (one partition)"), "1");
        assert_eq!(verdict("both transports, decoded here"), "both transports");
    }

    #[test]
    fn report_text_becomes_a_sentence() {
        assert_eq!(
            plain("  bootstrap kernel MISSING — needed by `install debian` only"),
            "Bootstrap kernel  needed by `install debian` only"
        );
        assert_eq!(plain("error: /dev/kvm not found"), "/dev/kvm not found");
        assert_eq!(
            value_of("  hypervisor      : Windows Hypervisor Platform present"),
            "Windows Hypervisor Platform present"
        );
    }
}
