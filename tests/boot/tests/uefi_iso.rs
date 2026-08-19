//! The UEFI ISO boot chain (UEFI-1803, ADR-0003 phase 3): EDK2 CloudHv must find
//! an Ubuntu installer ISO on a read-only virtio-blk over PCI and hand off to its
//! bootloader.
//!
//! This is the one test that exercises the whole stack the way a person does —
//! firmware, a real PCI bus, two virtio-blk devices, virtio-gpu, virtio-input,
//! and 3 GiB of media nobody at this project wrote. Everything it asserts was a
//! machine gap first:
//!
//! | Stage the log reaches | What had to be fixed to get there |
//! |---|---|
//! | `PciBus: Discovered PCI @ [00\|01\|00] [VID = 0x1AF4, DID = 0x1042]` | a PCI root bus and the modern virtio-pci transport (EPIC 19) |
//! | `VirtioBlkInit: LbaSize=0x200[B]` | PCI subsystem device id ≥ 0x40, or `Virtio10Dxe` refuses to bind (`Virtio10BindingSupported`) |
//! | `FSOpen: Open '\EFI\BOOT\BOOTX64.EFI' Success` | queue-notify ioeventfds that follow a BAR `PciBusDxe` reassigned (`machine_x86::notify::DeviceNotifier::rebase`) |
//! | GRUB's menu, then the Ubuntu kernel | — |
//!
//! # What it does *not* assert
//!
//! Anything past the bootloader. The kernel boots with the ISO's own command
//! line, which carries no `console=` clause, so what Linux does next is visible
//! on the virtio-gpu scanout and not on ttyS0. Criterion (c) — the installer
//! itself — is checked by the *host* side of the log in a manual run
//! (`.claude/skills/vm-testing/SKILL.md`), because the assertion would otherwise
//! be "the guest painted some pixels", which is a screenshot-comparison test
//! (MVP-1405) and belongs with the graphical tier. `ENTANGLED_UEFI_ISO_SHOT`
//! dumps the scanout so that run has an artifact.
//!
//! Ignored by default: it needs a 2.9 GiB verified ISO, a 4 MiB firmware build
//! and about a minute of wall clock.
//!
//! ```bash
//! bash guest/firmware/build-cloudhv.sh
//! bash scripts/fetch-ubuntu-iso.sh
//! cargo test -p boot-tests --test uefi_iso -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available, make_raw_disk};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio_pci::VirtioPciBus;
use virtio_core::VirtioDevice;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

/// Wall clock for the whole chain. A healthy run reaches GRUB in ~25 s
/// (firmware ~3 s, then `PciBusDxe` + the FAT/GPT reads off a 2.9 GiB image) and
/// spends 30 s more in GRUB's own menu timeout, which is why this is generous.
const DEADLINE: Duration = Duration::from_secs(150);

/// Criterion (a): the firmware loaded the ISO's removable-media boot loader.
///
/// Two independent halves, both required. The `FSOpen` line proves a FAT driver
/// opened the file, i.e. the GPT partition table and the EFI System Partition on
/// an isohybrid image were read correctly over virtio-blk. The `BdsDxe: starting`
/// line proves BDS actually transferred control to it — an image that fails
/// `LoadImage` logs the first and not the second.
const OPENED_BOOTX64: &str = r"FSOpen: Open '\EFI\BOOT\BOOTX64.EFI' Success";
const STARTED_BOOT_OPTION: &str = "BdsDxe: starting Boot";

/// The device path BDS booted from must be the ISO's ESP: partition 2 of a GPT on
/// the *second* virtio function (`Pci(0x2,0x0)` — the ISO is /dev/vdb). Without
/// this the test would pass on a target disk that happened to be bootable.
const BOOTED_FROM_ISO_ESP: &str = "Pci(0x2,0x0)/HD(2,GPT";

/// Criterion (b): GRUB is running. Its version banner is the first thing it
/// paints, and `menuentry` text proves it read the ISO's own configuration
/// rather than falling into its rescue shell.
const GRUB_BANNER: &str = "GNU GRUB  version";
const GRUB_UBUNTU_ENTRY: &str = "Try or Install Ubuntu Server";

/// Criterion (c), as far as ttyS0 can see it: GRUB called `ExitBootServices`,
/// which only happens once it has a kernel loaded and is jumping into it.
const HANDED_OFF_TO_KERNEL: &str = "MpInitChangeApLoopCallback() done!";

/// A firmware `ASSERT` is a missing machine feature (ADR-0003's bring-up table
/// is a list of them), so it fails the test with the offending lines rather than
/// timing out fifty lines later.
const ASSERT: &str = "ASSERT";

/// Environment overrides.
const ISO_ENV: &str = "ENTANGLED_UBUNTU_ISO";
const SHOT_ENV: &str = "ENTANGLED_UEFI_ISO_SHOT";
const SCRATCH_ENV: &str = "ENTANGLED_SCRATCH_DIR";
/// Keep the guest running this many seconds *past* `ExitBootServices` before
/// screenshotting. This is the knob that makes the test useful for criterion (c)
/// and for UEFI-1804: the assertions above end at the bootloader, but the
/// installer needs about a minute to reach its first screen, and by then the only
/// place to look is the scanout.
const LINGER_ENV: &str = "ENTANGLED_UEFI_ISO_LINGER";

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

/// The verified installer ISO: `$ENTANGLED_UBUNTU_ISO`, else the newest release
/// directory `scripts/fetch-ubuntu-iso.sh` has populated.
fn ubuntu_iso() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(ISO_ENV).map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
        eprintln!(
            "skipping: {ISO_ENV} is set but {} is not a file",
            path.display()
        );
        return None;
    }
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cache/entangled"),
    };
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(cache.join("ubuntu"))
        .ok()?
        .filter_map(|entry| entry.ok())
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(|f| f.ok())
                .map(|f| f.path())
                .filter(|p| p.extension().is_some_and(|e| e == "iso"))
                .collect::<Vec<_>>()
        })
        .collect();
    // Newest release directory last, so a machine with several cached releases
    // boots the one it most recently fetched.
    candidates.sort();
    candidates.pop()
}

/// The install target, `/dev/vda`. Sparse and never written by this test — the
/// firmware only reads its (empty) partition table — but it has to exist so the
/// device ordering is the one `examples/ubuntu-uefi.toml` describes.
fn scratch_target() -> Option<PathBuf> {
    let dir = match std::env::var_os(SCRATCH_ENV) {
        Some(dir) => PathBuf::from(dir),
        // Never the repository: a drvfs mount cannot make sparse files.
        None => PathBuf::from(std::env::var_os("HOME")?).join("entangled-vms"),
    };
    let path = dir.join("uefi-iso-target.raw");
    match make_raw_disk(&path, 4096) {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!("skipping: cannot create the scratch target disk: {e}");
            None
        }
    }
}

#[test]
#[ignore = "needs artifacts/firmware/CLOUDHV.fd and a ~2.9 GiB verified Ubuntu ISO"]
fn cloudhv_boots_the_ubuntu_iso_to_its_bootloader() {
    if !kvm_available() {
        return;
    }
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    };
    let Some(iso) = ubuntu_iso() else {
        eprintln!("skipping: no Ubuntu ISO — run scripts/fetch-ubuntu-iso.sh (or set {ISO_ENV})");
        return;
    };
    let Some(target) = scratch_target() else {
        return;
    };
    eprintln!("booting {} with {}", firmware.display(), iso.display());

    // Two vCPUs and 2560 MiB, i.e. examples/ubuntu-uefi.toml. Below 2 GiB the
    // live-server initrd runs out of room before it reaches the installer, which
    // would make a memory choice look like a device bug.
    let machine = MachineConfig {
        memory_mib: 2560,
        vcpu_count: 2,
    };
    let hv = Hypervisor::open().expect("kvm");
    let mut vm = Vm::new(&hv, &machine).expect("vm");
    let mem_size = machine.memory_mib << 20;

    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).expect("mp table");
    machine_x86::acpi::write(vm.memory(), machine.vcpu_count).expect("acpi tables");

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).expect("serial");

    // Device order is the profile's, and it is load-bearing: PCI device numbers
    // are dense from 00:01.0 in attach order, so the target is /dev/vda at
    // 00:01.0 and the ISO is /dev/vdb at 00:02.0 — which is what
    // `BOOTED_FROM_ISO_ESP` pins.
    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();
    devices.push(Box::new(
        virtio_block::BlockDevice::open(&target, true).expect("target disk"),
    ));
    let installer = virtio_block::BlockDevice::open(&iso, false).expect("installer ISO");
    assert!(
        installer.is_read_only(),
        "the installer ISO must be attached read-only"
    );
    let iso_sectors = installer.capacity_sectors();
    devices.push(Box::new(installer));

    // The scanout is where the installer will actually appear; the firmware and
    // GRUB talk on ttyS0 either way. Present so the device set matches a real run
    // (five functions, five IOAPIC pins) rather than a reduced one.
    let display =
        display::DisplayHandle::detached(1280, 800).expect("detached display for the scanout");
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

    // Opt-in extra runtime past the hand-off, for looking at the installer.
    let linger = std::env::var(LINGER_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default();

    let started = Instant::now();
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).expect("vcpus");
    let handed_off = std::cell::Cell::new(None);
    let outcomes = threads.join_or_stop(
        || {
            let text = capture.text();
            if handed_off.get().is_none() && text.contains(HANDED_OFF_TO_KERNEL) {
                handed_off.set(Some(started.elapsed()));
            }
            if text.contains(ASSERT) || started.elapsed() >= DEADLINE + linger {
                return true;
            }
            // Stop as soon as the last thing this test can *see* has happened, so
            // a healthy run does not sit out the whole deadline — unless the
            // caller asked to linger, in which case keep going for that long.
            match handed_off.get() {
                Some(at) => started.elapsed() >= at + linger,
                None => false,
            }
        },
        Duration::from_millis(20),
    );
    let elapsed = started.elapsed();
    let log = capture.text();

    // The interesting lines, in order, so `--nocapture` shows the boot chain
    // rather than 400 KiB of DXE dispatch and ANSI cursor motion.
    println!("--- boot chain ({elapsed:?}) ---");
    for line in log.lines().filter(|line| {
        [
            "PciBus: Discovered",
            "VirtioBlkInit",
            "Found Mass Storage",
            "FSOpen",
            "BdsDxe: loading",
            STARTED_BOOT_OPTION,
            GRUB_BANNER,
            HANDED_OFF_TO_KERNEL,
            ASSERT,
        ]
        .iter()
        .any(|needle| line.contains(needle))
    }) {
        println!("{}", line.trim());
    }
    println!("--- ISO capacity: {iso_sectors} sectors ---");

    if let Some(path) = std::env::var_os(SHOT_ENV) {
        match display.screenshot(&path) {
            Ok(()) => println!("scanout written to {}", PathBuf::from(&path).display()),
            Err(e) => println!("could not write the scanout: {e}"),
        }
    }

    let asserts: Vec<&str> = log.lines().filter(|l| l.contains(ASSERT)).collect();
    assert!(
        asserts.is_empty(),
        "the firmware asserted — that is a missing machine feature, see ADR-0003: {asserts:?}"
    );

    // Criterion (a).
    assert!(
        log.contains(OPENED_BOOTX64),
        "the firmware never opened \\EFI\\BOOT\\BOOTX64.EFI on the ISO's ESP \
         (outcomes {outcomes:?}); full log:\n{log}"
    );
    assert!(
        log.contains(BOOTED_FROM_ISO_ESP),
        "the boot option BDS expanded was not partition 2 of the GPT on 00:02.0, \
         i.e. not the installer ISO's EFI System Partition"
    );
    assert!(
        log.contains(STARTED_BOOT_OPTION),
        "BDS opened BOOTX64.EFI but never transferred control to it"
    );

    // Criterion (b): the loader that BOOTX64.EFI chained to is GRUB, and it read
    // the ISO's menu rather than dropping to its rescue prompt.
    assert!(
        log.contains(GRUB_BANNER),
        "BOOTX64.EFI started but GRUB never printed its banner"
    );
    assert!(
        log.contains(GRUB_UBUNTU_ENTRY),
        "GRUB started but did not render the ISO's own menu entries"
    );

    // Criterion (c), the half ttyS0 can prove: GRUB loaded a kernel and left
    // firmware services behind. What the kernel then does is on the scanout.
    assert!(
        log.contains(HANDED_OFF_TO_KERNEL),
        "GRUB rendered its menu but never called ExitBootServices, so no kernel \
         was launched within {DEADLINE:?}"
    );
}
