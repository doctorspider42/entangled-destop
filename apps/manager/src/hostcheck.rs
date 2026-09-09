//! The Diagnostics view's engine: `entangled doctor`, run on a worker thread
//! and turned into rows the UI can colour.
//!
//! `doctor` already answers exactly the questions a GUI user has when something
//! is missing — is virtualisation usable, is the firmware there, is there an
//! Ubuntu ISO, is there room on the drive — and it answers them per host. So the
//! Diagnostics panel does not reimplement any of it; it runs the command and
//! renders the answer, including the "MISSING …" lines and the fix that follows
//! each one.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::backend::Backend;
use crate::launcher::Runner;
use crate::process::Waker;

/// How a single line of `doctor` output should read on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Ordinary reporting.
    Info,
    /// An artifact this host does not have; the next line is usually the fix.
    Missing,
    /// The command itself failed.
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub level: Level,
}

/// One completed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The exact command line, so a user can repeat it in a terminal.
    pub command: String,
    pub backend: Backend,
    /// `entangled doctor` exited 0.
    pub healthy: bool,
    pub lines: Vec<Line>,
}

impl Report {
    /// The one-line verdict for the panel heading.
    pub fn headline(&self) -> String {
        let missing = self
            .lines
            .iter()
            .filter(|line| line.level == Level::Missing)
            .count();
        match (self.healthy, missing) {
            (false, _) => "This host cannot run machines yet".to_string(),
            (true, 0) => "Everything this host needs is in place".to_string(),
            (true, 1) => "Machines can run; one thing is missing".to_string(),
            (true, n) => format!("Machines can run; {n} things are missing"),
        }
    }
}

/// Splits captured output into rows. `MISSING` is `doctor`'s own marker for an
/// artifact that is not there, and the line after it is the instruction — kept
/// as `Info` so it does not read like a second problem.
pub fn parse(output: &str, healthy: bool) -> Vec<Line> {
    output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(|line| Line {
            text: line.to_string(),
            level: if line.contains("MISSING") {
                Level::Missing
            } else if !healthy && line.starts_with("error:") {
                Level::Failure
            } else {
                Level::Info
            },
        })
        .collect()
}

/// Runs `entangled doctor` through `runner`, on its own thread.
///
/// The failure paths matter as much as the success one: a host with no engine,
/// or a WSL distribution that is not installed, must produce a readable panel
/// rather than an empty one — so a spawn error becomes the report's single
/// `Failure` line instead of being dropped.
pub fn spawn(runner: Runner, cwd: PathBuf, waker: Waker) -> mpsc::Receiver<Report> {
    let (tx, rx) = mpsc::channel();
    let builder = std::thread::Builder::new().name("host-check".to_string());
    let spawned = builder.spawn(move || {
        let report = run_blocking(&runner, &cwd);
        let _ = tx.send(report);
        waker();
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the diagnostics thread");
    }
    rx
}

fn run_blocking(runner: &Runner, cwd: &std::path::Path) -> Report {
    let (program, args) = match runner.doctor_command(cwd) {
        Ok(pair) => pair,
        Err(message) => {
            return Report {
                command: String::new(),
                backend: runner.backend,
                healthy: false,
                lines: vec![Line {
                    text: message,
                    level: Level::Failure,
                }],
            }
        }
    };
    let command_line = std::iter::once(program.display().to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");

    let mut command = std::process::Command::new(&program);
    command.args(&args).current_dir(cwd);
    crate::process::quiet_command(&mut command);

    match command.output() {
        Ok(output) => {
            let healthy = output.status.success();
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                text.push('\n');
                text.push_str(&stderr);
            }
            Report {
                command: command_line,
                backend: runner.backend,
                healthy,
                lines: parse(&text, healthy),
            }
        }
        Err(e) => Report {
            command: command_line,
            backend: runner.backend,
            healthy: false,
            lines: vec![Line {
                text: match runner.backend {
                    Backend::Wsl => format!(
                        "cannot start wsl.exe: {e}. Windows needs the Windows Subsystem for \
                         Linux installed for this backend."
                    ),
                    Backend::Native => format!("cannot start the Entangled engine: {e}"),
                },
                level: Level::Failure,
            }],
        },
    }
}

/// What a mock session shows instead of probing the host. Deliberately a mix:
/// a healthy hypervisor with one missing artifact is the state the panel exists
/// to make legible.
pub fn mock_report() -> Report {
    let output = "entangled doctor\n\
         \x20 hypervisor      : Windows Hypervisor Platform present\n\
         \x20 processor vendor: GenuineIntel\n\
         \x20 interrupt chips : 8259/8254/IOAPIC emulated in this process\n\
         \x20 uefi            : yes — CloudHv firmware via the PVH entry\n\
         \x20 networking      : backend = \"usernet\" — user-mode NAT in this process\n\
         \x20 VMs per process : 1\n\
         \x20 install         : ubuntu — UEFI + verified ISO, offline (no mirror needed)\n\
         \x20                   debian — d-i on the bootstrap kernel, needs the network\n\
         \x20   firmware         artifacts/firmware/CLOUDHV.fd (4.0 MiB, from this installation)\n\
         \x20   bootstrap kernel MISSING — needed by `install debian` only\n\
         \x20                   run `entangled fetch bootstrap-kernel` (~13 MiB, SHA-256 pinned)\n\
         \x20   ubuntu ISO       mock-cache/ubuntu-24.04.1-live-server-amd64.iso (2.9 GiB)\n\
         \x20   VM directory     mock-vms (412.0 GiB free of 953.0 GiB)\n\
         host looks ready to run VMs";
    Report {
        command: "entangled doctor".to_string(),
        backend: Backend::Native,
        healthy: true,
        lines: parse(output, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_artifacts_are_marked_and_their_fix_is_not() {
        let lines = parse(
            "  firmware         MISSING — needed by every UEFI machine\n\
             \x20                  Entangled Desktop can download it: run \
             `entangled fetch firmware`\n\
             host looks ready to run VMs",
            true,
        );
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].level, Level::Missing);
        assert_eq!(lines[1].level, Level::Info, "the fix is not a second fault");
        assert_eq!(lines[2].level, Level::Info);
    }

    #[test]
    fn a_failed_run_marks_its_error_line() {
        let lines = parse("entangled doctor\nerror: /dev/kvm not found", false);
        assert_eq!(lines[1].level, Level::Failure);
    }

    #[test]
    fn the_headline_counts_what_is_missing() {
        let mut report = mock_report();
        assert_eq!(report.headline(), "Machines can run; one thing is missing");

        report.lines.retain(|line| line.level != Level::Missing);
        assert_eq!(report.headline(), "Everything this host needs is in place");

        report.healthy = false;
        assert_eq!(report.headline(), "This host cannot run machines yet");
    }

    /// Blank lines never become empty rows — the panel would show gaps that
    /// look like missing output.
    #[test]
    fn blank_lines_are_dropped() {
        assert!(parse("\n\n   \n", true).is_empty());
    }
}
