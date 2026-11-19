//! `entangled-agent`: runs inside a Windows guest, in the user's session, and
//! shares the clipboard's text with the host over the virtio-console port
//! [`protocol::PORT_NAME`] (ADR-0008) — and runs the command lines the host
//! sends it, answering with their exit code and output (the control
//! channel's `exec`, which a profile turns on with `guest_exec = true`).
//!
//! The installer copies it off the answer disc and starts it at every logon
//! (`assets/unattend/first-logon.cmd`). It has no window and no console; it
//! keeps trying to open the port, so it survives the host side coming and
//! going (a VM started without the agent device, a suspend and resume).

#![cfg_attr(windows, windows_subsystem = "windows")]

// Only the Windows build speaks it (and the tests, everywhere), and it holds
// the host's half too (an exec request's encoder, a result's decoder): the
// host includes this very file.
#[allow(dead_code)]
mod protocol;

#[cfg(windows)]
mod agent;
#[cfg(windows)]
mod exec;

#[cfg(windows)]
fn main() {
    agent::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("entangled-agent runs inside a Windows guest");
}
