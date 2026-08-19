//! The virtio-pci acceptance boot (EPIC 19).
//!
//! Unit tests can prove that a register answers what the spec says it should.
//! They cannot prove that a real guest kernel, given nothing but a PCI bus, finds
//! a disk on it — which is the entire claim of this transport. That needs a real
//! kernel, and this is it.
//!
//! What makes the boot meaningful is what is *absent*: there is no
//! `virtio_mmio.device=` clause on the command line (the harness `debug_assert`s
//! that), so the guest has no idea any device exists until it walks the bus
//! itself. Everything the tests below check is therefore downstream of real
//! enumeration:
//!
//! 1. configuration mechanism #1 answers, so Linux believes in bus 0 at all;
//! 2. the host bridge and the virtio function are enumerated with the vendor,
//!    device and class the host published;
//! 3. `virtio-pci` binds to the function — i.e. the capability list walk found a
//!    common-configuration structure, and the modern driver claimed the device
//!    rather than "leaving for legacy driver";
//! 4. feature negotiation, queue programming and BAR-window dispatch all worked,
//!    because `/dev/vda` exists;
//! 5. **the queues and the interrupts actually work end to end**, because the
//!    guest reads megabytes off that disk *and counts the interrupts it took to do
//!    it*. The byte count alone would not prove this: a driver whose interrupts
//!    are lost still finishes a read eventually, because it notices used buffers
//!    the next time anything else wakes it. A climbing interrupt count is the
//!    evidence.
//!
//! # Two boots, one per interrupt mechanism
//!
//! Since MSI-X exists, a Linux guest offered it will never choose INTx — so there
//! are two acceptance boots rather than one, and the INTx one has to *ask* for an
//! INTx-only function ([`PciInterruptMode::IntxOnly`]).
//!
//! Both matter. MSI-X is what a real VM uses. INTx is what a driver uses before it
//! enables MSI-X, what it falls back to when `pci_alloc_irq_vectors` fails, what
//! an unbind returns to, and what a host without `KVM_CAP_SIGNAL_MSI` gets — so it
//! is not a legacy path, it is the floor.
//!
//! For MSI-X the interrupt count is not enough on its own: every other symptom of
//! a working device is identical on INTx, so the MSI-X boot also checks that the
//! kernel allocated message vectors (`msi_irqs/`), that `/proc/interrupts` names
//! `PCI-MSIX-…` as the controller, and that the lines are the per-source
//! `virtio0-config` / `virtio0-req.0` rather than one shared vector.
//!
//! Self-skips without `/dev/kvm` or the guest artifacts, like every other boot
//! test. `scripts/fetch-test-kernel.sh` will not do here — the *bootstrap* kernel
//! is the one with a config we control, so `guest/bootstrap-kernel/build.sh` is
//! required.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use boot_tests::{artifact, boot_once, kvm_available, make_raw_disk, test_initramfs, BootSpec};
use control_api::VirtioTransport;
use machine_x86::virtio_pci::PciInterruptMode;

/// Megabytes the guest reads off the PCI disk. Enough to be many requests rather
/// than one lucky one, small enough to keep the test quick.
const BENCH_MIB: u64 = 8;

/// Scratch image size, comfortably above [`BENCH_MIB`].
const DISK_MIB: u64 = 64;

fn deadline() -> Duration {
    let secs = std::env::var("ENTANGLED_BOOT_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60u64)
        .clamp(5, 600);
    Duration::from_secs(secs)
}

/// The bootstrap kernel, plus a note about why the fetched Debian kernel is not
/// a substitute.
fn bootstrap_kernel() -> Option<PathBuf> {
    match artifact("bootstrap/vmlinuz") {
        Some(kernel) => Some(kernel),
        None => {
            eprintln!(
                "skipping: artifacts/bootstrap/vmlinuz is missing — run \
                 guest/bootstrap-kernel/build.sh (this test needs CONFIG_VIRTIO_PCI, \
                 which only the bootstrap kernel's config guarantees)"
            );
            None
        }
    }
}

fn scratch_disk(name: &str) -> Option<PathBuf> {
    // The repository lives on a drvfs mount on this project's development host,
    // which cannot create sparse files; scratch images go somewhere native.
    let dir =
        std::env::var("ENTANGLED_SCRATCH_DIR").unwrap_or_else(|_| "/tmp/entangled-bench".into());
    let path = PathBuf::from(dir).join(name);
    match make_raw_disk(&path, DISK_MIB) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("skipping: no scratch disk ({error})");
            None
        }
    }
}

/// Boots with a virtio-blk disk on PCI and asks the guest both questions at
/// once: what did you enumerate, and can you read the disk?
fn pci_boot(name: &str) -> Option<boot_tests::BootOutcome> {
    pci_boot_with(name, PciInterruptMode::default())
}

fn pci_boot_with(name: &str, interrupts: PciInterruptMode) -> Option<boot_tests::BootOutcome> {
    let kernel = bootstrap_kernel()?;
    let initramfs = test_initramfs().or_else(|| {
        eprintln!("skipping: run scripts/build-test-initramfs.sh");
        None
    })?;
    let disk = scratch_disk(name)?;

    let mut spec = BootSpec::new(kernel, initramfs)
        .with_transport(VirtioTransport::Pci)
        .with_pci_interrupts(interrupts)
        .with_disk(disk)
        .with_blk_bench(BENCH_MIB);
    // Both probes run; the block one is the last to print, so waiting for it
    // waits for both.
    spec.extra_cmdline = format!("{} entangled.pciscan=1", spec.extra_cmdline);
    spec.deadline = deadline();

    match boot_once(&spec) {
        Ok(outcome) => Some(outcome),
        Err(error) => panic!("pci boot failed: {error}"),
    }
}

/// What every pci boot must show, whichever interrupt mechanism it used: the bus
/// was enumerated, the modern driver bound, and the disk was read in full.
///
/// Returns the number of interrupts the guest counted while reading, which is the
/// part each caller then asserts *about* — the count alone cannot say which
/// mechanism carried them.
fn assert_bus_enumerated_and_disk_read(outcome: &boot_tests::BootOutcome) -> u64 {
    // Print it unconditionally: this log is the evidence, whether or not the
    // assertions below are happy with it.
    println!("--- guest serial ---\n{}\n--- end ---", outcome.serial);

    assert!(
        outcome.reached_ready(),
        "guest never reached the ready marker"
    );

    // ---- (1)(2) the bus was enumerated ---------------------------------------
    let scan = outcome
        .probe("pciscan")
        .expect("guest must report its PCI enumeration");
    let field = |key: &str| {
        scan.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let devices = field("devices");
    assert_eq!(
        field("functions"),
        "2",
        "expected the host bridge and one virtio function; got {devices}"
    );
    assert_eq!(field("virtio"), "1", "one virtio function: {devices}");

    // The host bridge, with the class Linux's pci_sanity_check accepts a bus on.
    assert!(
        devices.contains("8086:0d57/060000"),
        "host bridge missing or misdescribed: {devices}"
    );
    // The virtio-blk function: vendor 0x1af4, modern device id 0x1040 + 2, and
    // the mass-storage class that makes lspci name it something true.
    assert!(
        devices.contains("1af4:1042/018000"),
        "virtio-blk function missing or misdescribed: {devices}"
    );

    // ---- (3) the modern driver bound ----------------------------------------
    assert_eq!(
        field("bound"),
        "1",
        "virtio-pci did not claim the function: {devices}"
    );
    assert!(
        devices.contains(":virtio-pci"),
        "the function is bound, but not by virtio-pci: {devices}"
    );

    // ---- (4) the device actually works --------------------------------------
    let bytes = outcome
        .probe_value("blkbench", "bytes")
        .expect("guest must report reading /dev/vda — no /dev/vda means the probe failed");
    assert_eq!(
        bytes,
        BENCH_MIB << 20,
        "guest read {bytes} bytes of the {BENCH_MIB} MiB it was asked for"
    );

    // Nothing on the command line told the guest where to look.
    assert!(
        !outcome.serial.contains("virtio_mmio.device"),
        "the kernel command line announced mmio slots; this was not a pci boot"
    );

    // ---- (5) interrupts were delivered at all -------------------------------
    //
    // The read completing is not evidence: a driver finds used buffers whenever
    // anything else wakes it, so a device whose interrupts are all lost still
    // finishes eventually. Only a climbing count separates "the interrupt path
    // works" from "the interrupt path is silently dead".
    let irqs = outcome
        .probe_value("blkbench", "irqs")
        .expect("guest must report its virtio interrupt count");
    assert!(
        irqs > 0,
        "no interrupts reached the guest: the read completed by luck \
         (irqs={irqs}; -1 means /proc/interrupts had no virtio line at all, i.e. \
         the driver never registered a handler)"
    );
    irqs
}

/// One field of the guest's `blkbench` line, as a string.
fn blkbench_field(outcome: &boot_tests::BootOutcome, key: &str) -> String {
    outcome
        .probe("blkbench")
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// THE acceptance test: a guest that was told nothing finds its disk on the PCI
/// bus, reads from it, and takes its completion interrupts **as MSI-X messages**.
///
/// The last part is the one MSI-X adds, and it needs its own evidence, because
/// every other symptom of a working device is identical on INTx. Three
/// independent things have to agree:
///
/// * `/sys/bus/pci/devices/0000:00:01.0/msi_irqs` exists with one entry per
///   vector, which the kernel creates only once it has enabled MSI or MSI-X on
///   the function;
/// * the controller column of `/proc/interrupts` reads `PCI-MSIX-0000:00:01.0`
///   rather than `IO-APIC`;
/// * the lines are named `virtio0-config` and `virtio0-req.0`, i.e. the driver
///   really took *per-source* vectors rather than one shared one.
///
/// And the interrupt count still has to climb, so this is not merely a device
/// that negotiated MSI-X and then relied on luck.
#[test]
fn a_guest_enumerates_the_pci_bus_and_takes_its_interrupts_over_msix() {
    if !kvm_available() {
        return;
    }
    let Some(outcome) = pci_boot("pci-acceptance-msix.raw") else {
        return;
    };
    let irqs = assert_bus_enumerated_and_disk_read(&outcome);

    // Two vectors: one for the request queue, one for configuration changes.
    let vectors = outcome
        .probe_value("pciscan", "msix")
        .expect("guest must report how many message vectors it allocated");
    assert_eq!(
        vectors, 2,
        "expected one vector per queue plus one for config changes; the guest \
         allocated {vectors} (0 means the kernel never enabled MSI-X — either the \
         capability was not found or pci_alloc_irq_vectors failed)"
    );

    let mode = blkbench_field(&outcome, "irqmode");
    assert_eq!(
        mode, "msix",
        "the guest's interrupts were delivered by {mode:?}, not MSI-X; the \
         controller column of /proc/interrupts is echoed in the serial log above"
    );

    // Per-source vectors, not one shared one. `virtio0-req.0` is virtio-blk's
    // request queue and `virtio0-config` the config-change source; both names come
    // from the driver, so seeing them means the driver, not just the device, is in
    // per-queue MSI-X mode.
    let names = blkbench_field(&outcome, "irqnames");
    for expected in ["virtio0-config", "virtio0-req.0"] {
        assert!(
            names.contains(expected),
            "expected an MSI-X line named {expected}; the guest reported {names:?}"
        );
    }

    // The INTx pin must have carried nothing: under MSI-X the transport never
    // raises the line, so no virtio line may sit on the IOAPIC.
    assert!(
        !names.is_empty() && !outcome.serial.contains("IO-APIC   5-edge      virtio"),
        "a virtio interrupt line is still on the IOAPIC pin; serial log above"
    );

    let ms = outcome.probe_value("blkbench", "ms").unwrap_or(0);
    println!(
        "read {} bytes over virtio-pci with MSI-X in {ms} ms, {irqs} messages on \
         {vectors} vectors ({names}); boot to ready in {:?}",
        BENCH_MIB << 20,
        outcome.time_to_ready
    );
}

/// The INTx acceptance boot, unchanged in substance from before MSI-X existed and
/// kept for exactly that reason.
///
/// A Linux guest offered MSI-X will never choose INTx, so without
/// [`PciInterruptMode::IntxOnly`] this path would simply stop being tested — and
/// it is not dead code: it is what a driver uses before it enables MSI-X, what it
/// falls back to if `pci_alloc_irq_vectors` fails, what an unbind returns to, and
/// what a host without `KVM_CAP_SIGNAL_MSI` gets.
///
/// The wart it documents is also still real. The guest logs
///
/// ```text
/// virtio-pci 0000:00:01.0: can't find IRQ for PCI INT A; probably buggy MP table
/// ```
///
/// because the MP table publishes ISA interrupt sources and the DSDT has no
/// `_PRT`, so `pcibios_lookup_irq` finds no routing entry and Linux keeps the line
/// the host wrote into `interrupt_line`. That line is asserted below via the
/// guest's own `/sys/.../irq`, because "it happens to work" and "it is guaranteed
/// to work" are different things and only one of them survives a kernel upgrade.
#[test]
fn an_intx_only_function_still_serves_its_disk_on_the_line_the_host_published() {
    if !kvm_available() {
        return;
    }
    let Some(outcome) = pci_boot_with("pci-acceptance-intx.raw", PciInterruptMode::IntxOnly) else {
        return;
    };
    let irqs = assert_bus_enumerated_and_disk_read(&outcome);

    // No MSI-X capability was published, so the kernel cannot have enabled it.
    assert_eq!(
        outcome.probe_value("pciscan", "msix"),
        Some(0),
        "an INTx-only function must expose no message vectors at all"
    );
    let mode = blkbench_field(&outcome, "irqmode");
    assert_eq!(mode, "intx", "expected an IO-APIC line, got {mode:?}");

    // …and on the line the host published, not some fallback.
    let expected_irq = machine_x86::layout::PCI_FIRST_IRQ;
    assert!(
        outcome.serial.contains(&format!(
            "entangled-pciscan: 0000:00:01.0 irq={expected_irq}"
        )),
        "the guest did not settle on GSI {expected_irq}, the line the host wrote \
         into interrupt_line; serial log above"
    );

    let ms = outcome.probe_value("blkbench", "ms").unwrap_or(0);
    println!(
        "read {} bytes over virtio-pci with INTx in {ms} ms with {irqs} interrupts \
         on GSI {expected_irq}; boot to ready in {:?}",
        BENCH_MIB << 20,
        outcome.time_to_ready
    );
}

/// The same disk, the same guest, on both transports — so a difference in
/// behaviour is attributable to the transport and nothing else.
///
/// `#[ignore]`d because it boots twice; the acceptance test above is the one CI
/// needs.
#[test]
#[ignore = "boots twice: run when changing either transport"]
fn both_transports_serve_the_same_disk() {
    if !kvm_available() {
        return;
    }
    let Some(pci) = pci_boot("pci-compare.raw") else {
        return;
    };
    let Some(kernel) = bootstrap_kernel() else {
        return;
    };
    let Some(initramfs) = test_initramfs() else {
        return;
    };
    let Some(disk) = scratch_disk("mmio-compare.raw") else {
        return;
    };
    let mut mmio = BootSpec::new(kernel, initramfs)
        .with_transport(VirtioTransport::Mmio)
        .with_disk(disk)
        .with_blk_bench(BENCH_MIB);
    mmio.deadline = deadline();
    let mmio = boot_once(&mmio).expect("mmio boot");

    for (name, outcome) in [("pci", &pci), ("mmio", &mmio)] {
        assert!(outcome.reached_ready(), "{name}: no ready marker");
        assert_eq!(
            outcome.probe_value("blkbench", "bytes"),
            Some(BENCH_MIB << 20),
            "{name}: guest did not read the whole disk range"
        );
        assert!(
            outcome.probe_value("blkbench", "irqs").unwrap_or(0) > 0,
            "{name}: the read completed without any device interrupts"
        );
        println!(
            "{name}: ready in {:?}, read {BENCH_MIB} MiB in {} ms with {} interrupts",
            outcome.time_to_ready,
            outcome.probe_value("blkbench", "ms").unwrap_or(0),
            outcome.probe_value("blkbench", "irqs").unwrap_or(0)
        );
    }
    // Only the mmio boot has cmdline clauses; only the pci boot enumerates.
    assert!(pci.probe("pciscan").is_some());
    assert!(
        mmio.serial.contains("virtio_mmio.device"),
        "the mmio boot must still announce its slots"
    );
}
