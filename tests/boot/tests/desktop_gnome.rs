//! Ubuntu Desktop live session on the virtio-gpu scanout (the wave-G cdrom
//! deliverable, EPIC 8's real acceptance run).
//!
//! Boots the Desktop ISO exactly the way `entangled run --cdrom` does — the
//! ISO read-only on virtio-blk over PCI, UEFI firmware, 4 GiB (the high-RAM
//! split) and 4 vCPUs — but with one extra move: the ISO's own kernel command
//! line has no `console=` clause, so the test types `console=ttyS0` into GRUB
//! over the serial console (the same mechanism `entangled install ubuntu`
//! uses, and it works for the same reason: CloudHv has no GOP, so the EFI
//! console *is* ttyS0 and GRUB reads it). That turns the guest kernel's own
//! log into the evidence:
//!
//! * `smp: Brought up 1 node, 4 CPUs` — SMP in the full UEFI run path;
//! * `virtio_gpu` bound and `[drm] features: … +edid` — the driver negotiated
//!   the EDID feature this device now offers (MVP-811);
//! * systemd reaching the graphical target — GNOME is actually starting, not
//!   just the kernel drawing a console.
//!
//! What the desktop then *looks* like is on the scanout:
//! `ENTANGLED_DESKTOP_SHOT=<path>` writes it as PNG after the linger.
//!
//! `#[ignore]`d: needs `/dev/kvm`, the firmware and the ~6 GiB verified
//! Desktop ISO, and GNOME under llvmpipe takes several minutes.
//!
//! ```bash
//! bash guest/firmware/build-cloudhv.sh
//! bash scripts/fetch-ubuntu-iso.sh desktop
//! cargo test -p boot-tests --test desktop_gnome -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio_pci::VirtioPciBus;
use virtio_core::VirtioDevice;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

/// Firmware + GRUB ~40 s, then a 6 GiB squashfs and GNOME on llvmpipe. The
/// interesting markers all land in the first ~3 minutes; the deadline is the
/// hang-vs-slow line.
const DEADLINE: Duration = Duration::from_secs(10 * 60);

/// GRUB drew the Desktop ISO's menu (its entry is "Try or Install Ubuntu" —
/// no "Server").
const MENU_MARKER: &str = "Try or Install Ubuntu";
const PROMPT: &str = "grub>";

/// The guest kernel found all four vCPUs through our MADT, in the UEFI boot
/// path (`acpi.rs` asserts the same line for direct-Linux guests).
const SMP_MARKER: &str = "smp: Brought up 1 node, 4 CPUs";

/// systemd is starting the graphical session — GNOME, not a text console.
const GRAPHICAL_MARKER: &str = "Graphical Interface";

const VCPUS: u32 = 4;
const MEMORY_MIB: u64 = 4096;

const SHOT_ENV: &str = "ENTANGLED_DESKTOP_SHOT";
/// Extra seconds to keep the guest running after every marker has appeared —
/// how long the scanout gets to become a full desktop before the screenshot.
const LINGER_ENV: &str = "ENTANGLED_DESKTOP_LINGER";

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

/// The verified Desktop ISO, from the fetch script's cache (or
/// `$ENTANGLED_UBUNTU_DESKTOP_ISO`).
fn desktop_iso() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ENTANGLED_UBUNTU_DESKTOP_ISO").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cache/entangled"),
    };
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(cache.join("ubuntu"))
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|f| f.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains("desktop"))
                        && p.extension().is_some_and(|e| e == "iso")
                })
                .collect::<Vec<_>>()
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Types the boot commands into GRUB, one prompt at a time (the
/// `install_ubuntu::GrubScript` discipline: line n goes out only once prompt
/// n+1 has been printed, so the exchange is self-synchronising).
struct GrubTyping {
    commands: Vec<String>,
    entered: bool,
    sent: usize,
}

impl GrubTyping {
    fn new() -> Self {
        Self {
            commands: vec![
                // The Desktop ISO's menu entry is `linux /casper/vmlinuz  ---`;
                // paths and the `---` separator kept verbatim, console added.
                "linux /casper/vmlinuz console=ttyS0,115200n8 ---".into(),
                "initrd /casper/initrd".into(),
                "boot".into(),
            ],
            entered: false,
            sent: 0,
        }
    }

    fn step(&mut self, log: &str) -> Option<Vec<u8>> {
        if !self.entered {
            if !log.contains(MENU_MARKER) {
                return None;
            }
            self.entered = true;
            return Some(b"c".to_vec());
        }
        if self.sent >= self.commands.len() || log.matches(PROMPT).count() <= self.sent {
            return None;
        }
        let line = self.commands[self.sent].clone();
        self.sent += 1;
        println!("typing into GRUB: {line}");
        Some(format!("{line}\r").into_bytes())
    }
}

#[test]
#[ignore = "boots the GNOME live desktop: needs KVM, the firmware and the ~6 GiB Desktop ISO"]
fn the_desktop_iso_reaches_gnome_on_the_scanout() {
    if !kvm_available() {
        return;
    }
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    };
    let Some(iso) = desktop_iso() else {
        eprintln!("skipping: no Desktop ISO — run scripts/fetch-ubuntu-iso.sh desktop");
        return;
    };
    eprintln!("booting {} with {}", firmware.display(), iso.display());

    let machine = MachineConfig {
        memory_mib: MEMORY_MIB,
        vcpu_count: VCPUS,
    };
    let hv = Hypervisor::open().expect("kvm");
    let mut vm = Vm::new(&hv, &machine).expect("vm");
    let mem_size = machine.memory_mib << 20;

    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).expect("mp table");
    machine_x86::acpi::write(vm.memory(), machine.vcpu_count).expect("acpi tables");

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).expect("serial");

    // The same machine `entangled run --cdrom <iso>` builds from a diskless
    // profile: the ISO read-only at 00:01.0, then GPU and the two inputs.
    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();
    let cdrom = virtio_block::BlockDevice::open(&iso, false).expect("cdrom");
    assert!(cdrom.is_read_only(), "the cdrom must be read-only");
    devices.push(Box::new(cdrom));
    let display = display::DisplayHandle::detached(1920, 1080).expect("detached scanout");
    devices.push(Box::new(virtio_gpu::GpuDevice::new(display.clone())));
    devices.push(Box::new(virtio_input::InputDevice::keyboard()));
    devices.push(Box::new(virtio_input::InputDevice::absolute_pointer()));

    let mem = Arc::new(vm.memory().clone());
    let pci = VirtioPciBus::attach(vm.fd_shared(), mem, devices).expect("virtio-pci bus");
    let bus = MachineBus::with_virtio_pci(serial, pci).with_firmware_platform();

    let image = uefi_boot::FirmwareConfig {
        firmware: firmware.clone(),
    }
    .open()
    .expect("firmware image");
    let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size).expect("load firmware");

    let vcpus = vm.take_vcpus();
    for vcpu in &vcpus {
        x86_boot::setup_pvh_sregs(vm.memory(), vcpu).expect("pvh sregs");
        if vcpu.index == 0 {
            x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr).expect("pvh regs");
        }
    }

    let linger = std::env::var(LINGER_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default();

    let started = Instant::now();
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).expect("vcpus");
    let typing = Mutex::new(GrubTyping::new());
    let done_at = std::cell::Cell::new(None);
    let outcomes = threads.join_or_stop(
        || {
            let text = capture.text();
            if let Ok(mut typing) = typing.lock() {
                if let Some(keys) = typing.step(&text) {
                    bus.push_serial_input(&keys);
                }
            }
            let all_seen = text.contains(SMP_MARKER)
                && text.contains("virtio_gpu")
                && text.contains(GRAPHICAL_MARKER);
            if done_at.get().is_none() && all_seen {
                done_at.set(Some(started.elapsed()));
            }
            match done_at.get() {
                Some(at) => started.elapsed() >= at + linger,
                None => started.elapsed() >= DEADLINE,
            }
        },
        Duration::from_millis(50),
    );
    let log = capture.text();

    println!("--- guest evidence ({:?}) ---", started.elapsed());
    for line in log.lines().filter(|line| {
        [
            "smp:",
            "smpboot",
            "virtio_gpu",
            "[drm]",
            "Command line:",
            GRAPHICAL_MARKER,
            "GNOME",
            "gdm",
        ]
        .iter()
        .any(|needle| line.contains(needle))
    }) {
        println!("{}", line.trim());
    }
    println!("--- vCPU outcomes: {outcomes:?} ---");

    if let Some(path) = std::env::var_os(SHOT_ENV) {
        match display.screenshot(&path) {
            Ok(()) => println!("scanout written to {}", PathBuf::from(&path).display()),
            Err(e) => println!("could not write the scanout: {e}"),
        }
    }

    assert!(
        log.contains("console=ttyS0"),
        "the typed GRUB command line never reached the kernel; full log:\n{log}"
    );
    assert!(
        log.contains(SMP_MARKER),
        "the guest did not bring up all {VCPUS} vCPUs; its smp lines:\n{}",
        log.lines()
            .filter(|l| l.contains("smp"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        log.contains("virtio_gpu"),
        "the virtio_gpu driver never bound; drm lines:\n{}",
        log.lines()
            .filter(|l| l.contains("drm"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        log.contains(GRAPHICAL_MARKER),
        "systemd never reached the graphical target within {DEADLINE:?}"
    );
}
