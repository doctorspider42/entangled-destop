//! The shared-memory window acceptance boot (EPIC 20, VEN-2001 phase 2).
//!
//! Phase 1 could declare a virtio shared-memory region and validate mappings
//! inside it, but nothing allocated the window, so `RESOURCE_MAP_BLOB` could
//! not succeed anywhere and no guest could see a byte of it. This boot is the
//! evidence that it can now — and it is deliberately built so that **no part of
//! the claim rests on the host's own bookkeeping**:
//!
//! 1. the host writes a marker into the window's pages *before the vCPUs
//!    start*, through the device's `ShmBacking`;
//! 2. a real Linux kernel walks the PCI bus, finds a 64-bit prefetchable BAR 2
//!    it was told nothing about, and claims it inside the host-bridge window
//!    the DSDT published;
//! 3. the guest `mmap`s `/sys/bus/pci/devices/*/resource2` and reads the
//!    host's marker back — through a hypervisor mapping, with no exit and no
//!    host code in the path;
//! 4. the guest writes a reply one page in;
//! 5. after the guest has stopped, the host reads that reply out of the same
//!    host pages.
//!
//! Steps 1/3 and 4/5 are the two directions, and step 2 is the acceptance the
//! backlog asks for: *a guest kernel enumerating the shared-memory region with
//! the right size and address*.
//!
//! What this does **not** prove is Venus. Nothing on this host decodes a Vulkan
//! command stream (ADR-0004's 2026-08-21 probe: virglrenderer 0.9.1, no
//! `/dev/dri`, lavapipe), and the renderer here is the portable loopback. The
//! window is what Venus was missing; the renderer is what is missing next.
//!
//! Self-skips without `/dev/kvm` or the guest artifacts, like every other boot
//! test. The bootstrap kernel is required, not the fetched one: this needs
//! `CONFIG_VIRTIO_PCI` and a kernel that speaks ACPI, which only the config we
//! control guarantees.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use boot_tests::{
    artifact, boot_once, kvm_available, test_initramfs, BootSpec, GUEST_SHM_REPLY,
    GUEST_SHM_REPLY_OFFSET, HOST_SHM_MAGIC,
};
use control_api::VirtioTransport;

/// The guest this boots is 2048 MiB, which is *below* the 32-bit MMIO hole, so
/// its aperture — and therefore its window — sits at exactly 4 GiB. That is the
/// small-guest half of `layout::pci_mmio64_base`; the big-guest half (RAM
/// continuing past 4 GiB, so the aperture moves up with it) is covered by
/// [`the_window_follows_ram_on_a_big_guest`] below.
const SMALL_GUEST_MIB: u64 = 2048;

/// Big enough to have high RAM: 4096 MiB puts the top of RAM — and the
/// aperture — at 0x1_4000_0000, which is the number EDK2 was measured to
/// publish as `Pci64Base` for the same guest.
const BIG_GUEST_MIB: u64 = 4096;

fn deadline() -> Duration {
    let secs = std::env::var("ENTANGLED_BOOT_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60u64)
        .clamp(5, 600);
    Duration::from_secs(secs)
}

fn bootstrap_kernel() -> Option<PathBuf> {
    match artifact("bootstrap/vmlinuz") {
        Some(kernel) => Some(kernel),
        None => {
            eprintln!(
                "skipping: artifacts/bootstrap/vmlinuz is missing — run \
                 guest/bootstrap-kernel/build.sh (this test needs CONFIG_VIRTIO_PCI)"
            );
            None
        }
    }
}

/// Boots a guest with a virtio-gpu whose host-visible window is backed, and
/// asks it to prove it can see the window.
fn shm_boot(memory_mib: u64) -> Option<boot_tests::BootOutcome> {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm is not available");
        return None;
    }
    let kernel = bootstrap_kernel()?;
    let initramfs = test_initramfs().or_else(|| {
        eprintln!("skipping: run scripts/build-test-initramfs.sh");
        None
    })?;
    let expect = String::from_utf8(HOST_SHM_MAGIC.to_vec()).expect("ascii");
    let mut spec = BootSpec::new(kernel, initramfs);
    spec.memory_mib = memory_mib;
    spec.vcpus = 1;
    spec.transport = VirtioTransport::Pci;
    spec.shm_window = true;
    spec.extra_cmdline = format!("entangled.shmprobe={expect}");
    spec.await_marker = Some("shmprobe".into());
    spec.deadline = deadline();
    match boot_once(&spec) {
        Ok(outcome) => Some(outcome),
        Err(error) => panic!("boot failed: {error}"),
    }
}

/// Pulls `key=value` out of the probe line, panicking with the whole serial log
/// when it is missing — a probe that did not run is a failure, not a skip.
fn field(outcome: &boot_tests::BootOutcome, key: &str) -> String {
    let fields = outcome.probe("shmprobe").unwrap_or_else(|| {
        panic!(
            "the guest never reported a shared-memory window; serial log:\n{}",
            outcome.serial
        )
    });
    fields
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("shmprobe line has no {key}=; log:\n{}", outcome.serial))
}

fn parse_u64(value: &str) -> u64 {
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).expect("hex"),
        None => value.parse().expect("decimal"),
    }
}

/// The whole claim, on a guest small enough to have no high RAM.
#[test]
fn a_guest_enumerates_and_reads_the_shared_memory_window() {
    let Some(outcome) = shm_boot(SMALL_GUEST_MIB) else {
        return;
    };
    assert!(
        outcome.reached_ready(),
        "guest never reached the ready marker; log:\n{}",
        outcome.serial
    );
    // Printed, not only asserted: this line *is* the evidence, and a reader
    // running the test wants to see the guest's own words for it.
    for line in outcome
        .serial
        .lines()
        .filter(|l| l.contains("shmprobe") || l.contains("BAR 2"))
    {
        println!("{}", line.trim());
    }

    // (2) The guest's own enumeration: address, size, and the two BAR
    // attributes that decide whether Linux claims it at all.
    let base = parse_u64(&field(&outcome, "bar"));
    let size = parse_u64(&field(&outcome, "size"));
    let expected_base = machine_x86::layout::pci_mmio64_base(SMALL_GUEST_MIB << 20);
    assert_eq!(
        base, expected_base,
        "the guest found BAR 2 at {base:#x}, not at the aperture base {expected_base:#x}; \
         log:\n{}",
        outcome.serial
    );
    assert_eq!(
        size,
        virtio_gpu::NULL_HOST_VISIBLE_BYTES,
        "the window the guest sees is not the one the renderer declared"
    );
    assert_eq!(
        field(&outcome, "sixtyfour"),
        "1",
        "the BAR must be 64-bit, or it could not be above 4 GiB at all"
    );
    assert_eq!(
        field(&outcome, "prefetch"),
        "1",
        "the BAR must be prefetchable, or EDK2 would place it in the 32-bit \
         aperture and Linux would refuse the DSDT window"
    );

    // (3) The host wrote it before the vCPUs started; the guest read it back.
    let magic = field(&outcome, "magic");
    assert!(
        magic.starts_with(std::str::from_utf8(HOST_SHM_MAGIC).unwrap()),
        "the guest read {magic:?} out of the window, not the host's marker"
    );

    // (5) …and the other direction, checked after the guest has stopped.
    let window = outcome
        .shm
        .as_ref()
        .expect("the harness backed a window for this boot");
    let backing = window
        .backing_for(virtio_gpu::VIRTIO_GPU_SHM_ID_HOST_VISIBLE)
        .expect("the region the device declared");
    let mut reply = vec![0u8; GUEST_SHM_REPLY.len()];
    backing
        .read(GUEST_SHM_REPLY_OFFSET, &mut reply)
        .expect("inside the window");
    assert_eq!(
        reply, GUEST_SHM_REPLY,
        "the host cannot see what the guest wrote into the window"
    );

    // The window is host memory, and a stopped guest must not still be able to
    // reach it: every vCPU is gone, so the machine has unmapped nothing yet —
    // but the mapping dies with this `Arc`, which is the contract
    // `SharedWindow::drop` keeps.
    assert!(
        window.placed_at().is_some(),
        "the window should still be mapped while the harness holds it"
    );
}

/// The trap this placement exists to avoid: a guest whose RAM continues past
/// 4 GiB must get its window *above* that RAM, not on top of it.
///
/// Same boot, one number changed. It is the number that used to be wrong
/// everywhere in this project until a sibling agent paid for it, and the
/// aperture is derived from it rather than guessed.
#[test]
fn the_window_follows_ram_on_a_big_guest() {
    let Some(outcome) = shm_boot(BIG_GUEST_MIB) else {
        return;
    };
    let base = parse_u64(&field(&outcome, "bar"));
    let expected = machine_x86::layout::pci_mmio64_base(BIG_GUEST_MIB << 20);
    assert_eq!(expected, 0x1_4000_0000, "this is EDK2's measured Pci64Base");
    assert_eq!(
        base, expected,
        "a 4096 MiB guest's window must start past its high RAM; log:\n{}",
        outcome.serial
    );
    assert!(
        base >= vmm_core::HIGH_RAM_START + (BIG_GUEST_MIB << 20) - vmm_core::LOW_RAM_END,
        "the window overlaps the guest's own high RAM"
    );
    let magic = field(&outcome, "magic");
    assert!(
        magic.starts_with(std::str::from_utf8(HOST_SHM_MAGIC).unwrap()),
        "the guest read {magic:?} out of the window"
    );
}
