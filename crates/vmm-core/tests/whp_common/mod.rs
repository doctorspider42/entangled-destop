//! Shared plumbing for the WHP boot tests (`whp_boot.rs`, `whp_virtio_blk.rs`,
//! `whp_smp.rs`).
//!
//! Not a test target of its own — a `tests/<dir>/mod.rs` is only ever compiled
//! into the test binaries that `mod` it, which is why each of them silences
//! `dead_code` here rather than using everything.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use vmm_core::MachineConfig;

/// How long a guest gets to reach its marker.
///
/// Generous next to the KVM tests' 60 s: a WHP guest takes every `hlt`, every
/// MMIO access *and* every virtio queue kick through userspace, and the run is a
/// debug build.
pub const BOOT_DEADLINE: Duration = Duration::from_secs(180);

/// The machine every boot test here builds, unless it wants more vCPUs.
pub const MACHINE: MachineConfig = MachineConfig {
    memory_mib: 512,
    vcpu_count: 1,
};

/// Serialises against WHP's one-mapped-partition-per-process limit: a second
/// partition is created fine and then fails its first `WHvMapGpaRange` with
/// `0xC0370008`. Separate test binaries are separate processes, but two tests in
/// one file share one.
pub fn whp_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Serial sink that keeps everything the guest printed.
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut inner) = self.0.lock() {
            inner.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().map(|v| v.clone()).unwrap_or_default()).into_owned()
    }

    /// Extracts a `VMHOST_TEST_OK <name> key=value …` probe line's fields, the
    /// same contract `tests/boot`'s harness parses on Linux.
    pub fn probe(&self, name: &str) -> Option<Vec<(String, String)>> {
        let text = self.text();
        let prefix = format!("VMHOST_TEST_OK {name} ");
        text.lines()
            .find(|line| line.trim_start().starts_with(&prefix))
            .map(|line| {
                line.trim()
                    .split_ascii_whitespace()
                    .skip(2)
                    .filter_map(|field| field.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
    }

    /// One numeric field of a probe line. `irqs` is deliberately signed in the
    /// guest's output (-1 means "no virtio interrupt line exists at all"), so
    /// this parses `i64`.
    pub fn probe_value(&self, name: &str, key: &str) -> Option<i64> {
        self.probe(name)?
            .into_iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse().ok())
    }
}

/// True once `needle` has appeared *and* its line is terminated, so a reader is
/// guaranteed to see all of the fields on it. Stopping the VM mid-line would
/// truncate the very numbers a probe test came for.
pub fn complete_line_with(text: &str, needle: &str) -> bool {
    match text.find(needle) {
        Some(at) => text[at..].contains('\n'),
        None => false,
    }
}

pub fn artifact(relative: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts")
        .join(relative);
    path.exists().then_some(path)
}

/// The bootstrap kernel if it is built, otherwise the Debian netboot kernel.
pub fn kernel() -> Option<(PathBuf, &'static str)> {
    artifact("bootstrap/vmlinuz")
        .map(|p| (p, "bootstrap"))
        .or_else(|| artifact("tests/vmlinuz").map(|p| (p, "debian-netboot")))
}

/// The last `lines` lines of a serial log, for a failure message that is
/// readable rather than a wall of boot output.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Writes the whole serial log to `$ENTANGLED_WHP_BOOT_LOG` when it is set.
///
/// A passing boot is not the same as a *clean* boot: the interesting lines are
/// the ones about TSC calibration, the APIC timer and `check_timer()`, and they
/// scroll past long before any marker.
pub fn dump_log(text: &str) {
    let Ok(path) = std::env::var("ENTANGLED_WHP_BOOT_LOG") else {
        return;
    };
    match std::fs::write(&path, text) {
        Ok(()) => eprintln!("wrote the serial log to {path}"),
        Err(e) => eprintln!("could not write the serial log to {path}: {e}"),
    }
}
