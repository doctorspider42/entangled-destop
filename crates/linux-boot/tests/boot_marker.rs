//! Boot-to-marker integration test (backlog MVP-208): boots the real Debian
//! netboot kernel with our test initramfs and waits for
//! `VMHOST_GUEST_READY` on the captured serial console.
//!
//! Requires artifacts produced by:
//!   scripts/fetch-test-kernel.sh
//!   scripts/build-test-initramfs.sh
//! Skips (with a note) when the artifacts or /dev/kvm are unavailable.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

const BOOT_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

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
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().map(|v| v.clone()).unwrap_or_default()).into_owned()
    }
}

fn artifact(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts/tests")
        .join(name);
    path.exists().then_some(path)
}

#[test]
fn debian_kernel_boots_to_ready_marker() {
    let hv = match Hypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let (Some(kernel), Some(initramfs)) = (artifact("vmlinuz"), artifact("test-initramfs.cpio.gz"))
    else {
        eprintln!(
            "skipping: test artifacts missing — run scripts/fetch-test-kernel.sh and \
             scripts/build-test-initramfs.sh"
        );
        return;
    };

    let machine = MachineConfig {
        memory_mib: 256,
        vcpu_count: 1,
    };
    let mut vm = Vm::new(&hv, &machine).unwrap();
    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).unwrap();

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).unwrap();
    let bus = MachineBus::new(serial);

    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: "console=ttyS0 earlyprintk=serial panic=1 reboot=k".into(),
    };
    let loaded = linux_boot::load(vm.memory(), &boot, machine.memory_mib << 20).unwrap();

    let mut vcpus = vm.take_vcpus();
    let vcpu = &mut vcpus[0];
    x86_boot::setup_long_mode_sregs(vm.memory(), vcpu.fd()).unwrap();
    x86_boot::setup_boot_regs(vcpu.fd(), loaded.entry, loaded.boot_params_addr).unwrap();

    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    let start = Instant::now();
    let mut ready = false;
    while start.elapsed() < BOOT_DEADLINE {
        let text = capture.text();
        if text.contains(GUEST_READY_MARKER) {
            ready = true;
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            let _ = threads.stop();
            panic!("kernel panicked before the marker; serial log:\n{text}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = threads.stop();

    assert!(
        ready,
        "no {GUEST_READY_MARKER} within {BOOT_DEADLINE:?}; serial log:\n{}",
        capture.text()
    );
}
