//! The shared-memory window on WHP, end to end (EPIC 20, VEN-2001 phase 2).
//!
//! The Windows mirror of `tests/boot/tests/pci_shm.rs`, and the reason it
//! exists rather than being assumed from the KVM run: this is the one place in
//! the epic where the two hosts do genuinely different things. KVM moves a
//! window by re-registering a memory slot it reserved for the life of the VM;
//! WHP calls `WHvUnmapGpaRange` and `WHvMapGpaRange`, and maps the range
//! `Read | Write` with no execute, which KVM has no way to say. Everything
//! above `vmm_core::shm::GpaMapper` is the same code — `machine_x86::shm`, the
//! aperture arithmetic, the BAR-rebase sweep, the DSDT — so what this boot adds
//! is exactly the four WHP lines and the guest's verdict on them.
//!
//! The claim, in the guest's own words and in both directions:
//!
//! 1. the host stamps a marker into the window before the vCPUs start;
//! 2. a real Linux kernel finds a 64-bit prefetchable BAR 2 it was told nothing
//!    about, inside the host-bridge window the DSDT published;
//! 3. it `mmap`s `/sys/bus/pci/devices/*/resource2` and reads the marker back;
//! 4. it writes a reply one page in, and the host reads that out of the same
//!    pages after the guest has stopped.
//!
//! What this does **not** prove is Venus: nothing on Windows decodes a Vulkan
//! command stream either (ADR-0004). The renderer here is the portable
//! loopback, and the window is what Venus was missing.
//!
//! Self-skips when WHP is off or the artifacts are missing, like every other
//! test in this crate.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Instant;

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use machine_x86::shm::ShmSupport;
use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
use virtio_core::VirtioDevice;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};

mod whp_common;
use whp_common::{artifact, complete_line_with, dump_log, kernel, tail, whp_guard, Capture};

/// What the host writes at offset 0 before the guest starts, and what the guest
/// is told to expect. The same strings the KVM boot uses, so the two logs read
/// alike.
const HOST_MAGIC: &[u8] = b"HOST-WROTE-THIS-FIRST";
/// What the guest writes back, one page in. Matches `SHM_GUEST_REPLY` in the
/// test initramfs' `/init`.
const GUEST_REPLY: &[u8] = b"GUEST-SAW-THE-WINDOW";
const GUEST_REPLY_OFFSET: u64 = 4096;

/// This test's own machine shape rather than `whp_common::MACHINE`: the point
/// of the second size is that the aperture *moves with RAM*, and 2048 MiB is
/// the case with no high RAM at all, where it sits at exactly 4 GiB.
const MEMORY_MIB: u64 = 2048;

fn probe_field(capture: &Capture, key: &str) -> Option<String> {
    capture
        .probe("shmprobe")?
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn parse_u64(value: &str) -> u64 {
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).expect("hex"),
        None => value.parse().expect("decimal"),
    }
}

#[test]
fn a_guest_reads_and_writes_the_shared_memory_window_on_whp() {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return;
        }
    };
    let (Some((kernel, which)), Some(initramfs)) =
        (kernel(), artifact("tests/test-initramfs.cpio.gz"))
    else {
        eprintln!(
            "skipping: test artifacts missing — build artifacts/bootstrap/vmlinuz and run \
             scripts/build-test-initramfs.sh"
        );
        return;
    };
    eprintln!("booting the {which} kernel with a shared-memory window on pci");

    let machine = vmm_core::MachineConfig {
        memory_mib: MEMORY_MIB,
        vcpu_count: 1,
    };
    let mut partition = WhpPartition::with_options(&hv, &machine, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), machine.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), machine.vcpu_count).unwrap();

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), machine.vcpu_count)
        .expect("userspace irqchip");

    // A detached scanout: this boot is about the window, not about pixels.
    let display = display::DisplayHandle::detached(640, 480).expect("detached scanout");
    let devices: Vec<Box<dyn VirtioDevice>> = vec![Box::new(virtio_gpu::GpuDevice::with_renderer(
        display,
        Box::new(virtio_gpu::NullRenderer::with_venus()),
    ))];

    let mem = Arc::new(partition.memory().clone());
    // The one WHP-specific line in the whole window path.
    let allocate =
        |len: u64, host_mapped: bool| partition.create_shm_window(len, host_mapped);
    let pci = VirtioPciBus::attach_userspace_with_shm(
        mem,
        devices,
        &irqchip,
        PciInterruptMode::Msix,
        Some(ShmSupport {
            mem_bytes: machine.memory_mib << 20,
            allocate: &allocate,
        }),
    )
    .expect("attach virtio-pci with a shared-memory window");

    let window = pci
        .shm_window(0)
        .cloned()
        .expect("the machine backed the device's declared region");
    let backing = window
        .backing_for(virtio_gpu::VIRTIO_GPU_SHM_ID_HOST_VISIBLE)
        .expect("the region the device declared");
    assert_eq!(
        window.initial_base(),
        machine_x86::layout::pci_mmio64_base(machine.memory_mib << 20),
        "the host's initial assignment must be the aperture base"
    );
    assert_eq!(
        window.placed_at(),
        None,
        "nothing may be mapped before the guest enables memory decoding"
    );
    // Written before the vCPUs exist, so the guest cannot be reading a race.
    backing.write(0, HOST_MAGIC).expect("inside the window");

    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::with_virtio_pci(serial, pci).with_irqchip(Arc::clone(&irqchip));

    let expect = std::str::from_utf8(HOST_MAGIC).expect("ascii");
    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: format!(
            "console=ttyS0 earlyprintk=serial panic=1 reboot=k entangled.shmprobe={expect}"
        ),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, machine.memory_mib << 20)
        .expect("bzImage + initramfs load");

    let mut vcpus = partition.take_vcpus();
    {
        let vcpu = &mut vcpus[0];
        x86_boot::setup_long_mode_sregs(partition.memory(), vcpu).unwrap();
        x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr).unwrap();
    }
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    let start = Instant::now();
    let mut ready_at = None;
    let mut done = false;
    let mut panicked = false;
    while start.elapsed() < whp_common::BOOT_DEADLINE {
        let text = capture.text();
        if ready_at.is_none() && text.contains(GUEST_READY_MARKER) {
            ready_at = Some(start.elapsed());
        }
        if complete_line_with(&text, "VMHOST_TEST_OK shmprobe ")
            || complete_line_with(&text, "VMHOST_TEST_FAIL shmprobe ")
        {
            done = true;
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            panicked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = threads.stop();
    let text = capture.text();
    dump_log(&text);
    for line in text.lines().filter(|l| l.contains("shmprobe")) {
        eprintln!("{}", line.trim());
    }

    assert!(
        !panicked,
        "the guest panicked; serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        done,
        "no shmprobe verdict within {:?}; serial tail:\n{}",
        whp_common::BOOT_DEADLINE,
        tail(&text, 60)
    );
    assert!(
        !text.contains("VMHOST_TEST_FAIL"),
        "a guest probe failed; serial tail:\n{}",
        tail(&text, 40)
    );

    // (2) the guest's own enumeration.
    let base = parse_u64(&probe_field(&capture, "bar").expect("bar="));
    let size = parse_u64(&probe_field(&capture, "size").expect("size="));
    assert_eq!(
        base,
        machine_x86::layout::pci_mmio64_base(machine.memory_mib << 20),
        "the guest found BAR 2 somewhere other than the aperture base"
    );
    assert_eq!(size, virtio_gpu::NULL_HOST_VISIBLE_BYTES);
    assert_eq!(probe_field(&capture, "sixtyfour").as_deref(), Some("1"));
    assert_eq!(probe_field(&capture, "prefetch").as_deref(), Some("1"));

    // (3) the host's bytes, read by the guest.
    let magic = probe_field(&capture, "magic").unwrap_or_default();
    assert!(
        magic.starts_with(std::str::from_utf8(HOST_MAGIC).unwrap()),
        "the guest read {magic:?} out of the window, not the host's marker"
    );

    // (4) the guest's bytes, read by the host, after the vCPUs have stopped.
    let mut reply = vec![0u8; GUEST_REPLY.len()];
    backing
        .read(GUEST_REPLY_OFFSET, &mut reply)
        .expect("inside the window");
    assert_eq!(
        reply, GUEST_REPLY,
        "the host cannot see what the guest wrote through WHvMapGpaRange"
    );

    eprintln!(
        "WHP shared-memory window: time to ready {ready_at:?}, window at {base:#x}+{size}, \
         both directions verified"
    );
}
