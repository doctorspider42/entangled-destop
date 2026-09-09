//! UEFI boot with RAM above the high-RAM split, on WHP (EPIC 17 / ADR-0003).
//!
//! The Windows twin of `tests/boot/tests/uefi_highmem.rs`. Same claim, same
//! assertions, one extra thing it proves that the KVM side cannot: that
//! `WhpPartition`'s `WHvMapGpaRange` loop maps *both* guest-memory regions —
//! `[0, 0xc000_0000)` and `[4 GiB, …)` — and that the firmware then finds the
//! high half and puts its 64-bit PCI aperture past the end of it rather than on
//! top of it.
//!
//! The failure this exists for: EDK2 re-reads `hvm_start_info.rsdp_paddr` at the
//! *end* of DXE (`InstallCloudHvTables()`, after PCI enumeration). If anything
//! has overwritten the PVH hand-off block by then, the read produces a
//! non-canonical pointer and the firmware stops with
//! `X64 Exception Type - 0D(#GP)` inside `QemuFwCfgAcpiPlatform.dll`. So the
//! block is compared byte-for-byte after the run, not just inspected before it.
//!
//! Needs the "Windows Hypervisor Platform" optional feature and
//! `artifacts/firmware/CLOUDHV.fd`; self-skips without either.

#![cfg(windows)]

use std::time::{Duration, Instant};

use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::layout;
use machine_x86::serial::SerialConsole;
use vm_memory::{Bytes, GuestAddress, GuestMemory};
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::MachineConfig;

mod whp_common;
use whp_common::{artifact, dump_log, tail, whp_guard, Capture};

/// 4096 MiB — the size `examples/ubuntu-desktop-live.toml` asks for, and the
/// first one that makes guest memory two regions. WHP commits the range up
/// front, so this is the test's real footprint; it is the price of covering the
/// shape at all.
const MACHINE: MachineConfig = MachineConfig {
    memory_mib: 4096,
    vcpu_count: 2,
};

/// Every firmware MMIO access on WHP goes through the instruction emulator, in
/// a debug build. The KVM twin needs 2 s; this is slack, not budget.
const DEADLINE: Duration = Duration::from_secs(300);

const INSTALLING: &str = "root bridges have been connected, installing ACPI tables";
const INSTALL_FAILED: &str = "InstallAcpiTables:";
const BOOT_MANAGER: &str = "No bootable option or device was found";
const EXCEPTION: &str = "X64 Exception Type";

/// The three fields of `hvm_start_info` the firmware dereferences later, plus
/// the memory map they point at.
#[derive(Debug, PartialEq, Eq)]
struct Handoff {
    magic: u32,
    rsdp_paddr: u64,
    memmap_paddr: u64,
    memmap: Vec<u8>,
}

fn read_handoff<M: GuestMemory>(mem: &M) -> Handoff {
    let at = |off: u64| GuestAddress(layout::PVH_START_INFO_START + off);
    let mut memmap = vec![0u8; uefi_boot::pvh::MEMMAP_ENTRY_SIZE * 8];
    mem.read_slice(&mut memmap, GuestAddress(layout::PVH_MEMMAP_START))
        .expect("read the PVH memmap");
    Handoff {
        magic: mem.read_obj(at(0x00)).expect("read the PVH magic"),
        rsdp_paddr: mem.read_obj(at(0x20)).expect("read rsdp_paddr"),
        memmap_paddr: mem.read_obj(at(0x28)).expect("read memmap_paddr"),
        memmap,
    }
}

#[test]
fn cloudhv_firmware_boots_a_guest_with_ram_above_the_split_on_whp() {
    let _serialised = whp_guard();
    let Some(hv) = WhpHypervisor::open().ok() else {
        eprintln!("skipping: {WHP_ENABLE_HINT}");
        return;
    };
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    };

    let mem_size = MACHINE.memory_mib << 20;
    assert!(
        mem_size > vmm_core::LOW_RAM_END,
        "this test is pointless below the split"
    );

    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    assert_eq!(
        partition.memory().num_regions(),
        2,
        "a guest above the split must be two mapped regions"
    );
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).expect("mp table");
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).expect("acpi tables");

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");
    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::new(serial)
        .with_firmware_platform()
        .with_irqchip(std::sync::Arc::clone(&irqchip));

    let image = uefi_boot::FirmwareImage::read(&firmware).expect("firmware image");
    let boot = uefi_boot::load_pvh(partition.memory(), &image, mem_size).expect("load firmware");

    let before = read_handoff(partition.memory());
    assert_eq!(before.magic, uefi_boot::pvh::XEN_HVM_START_MAGIC_VALUE);
    assert_eq!(before.rsdp_paddr, layout::ACPI_RSDP_START);

    let vcpus = partition.take_vcpus();
    {
        // Boot CPU only: an AP must stay in the reset state WHP created it in,
        // and the firmware's own INIT-SIPI sweep brings it up.
        let vcpu = &vcpus[0];
        x86_boot::setup_pvh_sregs(partition.memory(), vcpu).expect("pvh sregs");
        x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr).expect("pvh regs");
    }

    let started = Instant::now();
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).expect("vcpus");
    let _ = threads.join_or_stop(
        || {
            let text = capture.text();
            text.contains(BOOT_MANAGER)
                || text.contains(EXCEPTION)
                || text.contains("ASSERT")
                || started.elapsed() >= DEADLINE
        },
        Duration::from_millis(50),
    );
    let log = capture.text();
    dump_log(&log);

    let after = read_handoff(partition.memory());
    drop(partition);
    drop(irqchip);

    assert_eq!(
        after,
        before,
        "the PVH hand-off block at {:#x} was overwritten during the boot — \
         `InstallCloudHvTables()` reads `rsdp_paddr` out of it at the end of DXE, \
         so this is a `#GP` in AcpiPlatformDxe waiting to happen",
        layout::PVH_HANDOFF_START
    );
    assert!(
        !log.contains(EXCEPTION),
        "the firmware took a CPU exception: {:?}",
        log.lines()
            .filter(|l| l.contains(EXCEPTION) || l.contains("RIP  -"))
            .collect::<Vec<_>>()
    );
    assert!(
        log.contains("PlatformAddHobCB: HighMemory [0x100000000,"),
        "the firmware never saw the high-RAM region:\n{}",
        tail(&log, 40)
    );
    assert!(
        log.contains(&format!(
            "Pci64Base=0x{:x}",
            vmm_core::HIGH_RAM_START + (mem_size - vmm_core::LOW_RAM_END)
        )),
        "the 64-bit PCI aperture must start past the end of RAM, not on top of it:\n{}",
        tail(&log, 40)
    );
    assert!(
        log.contains(INSTALLING),
        "AcpiPlatformDxe never got as far as installing tables:\n{}",
        tail(&log, 40)
    );
    assert!(
        !log.contains(INSTALL_FAILED),
        "InstallAcpiTables failed: {:?}",
        log.lines()
            .filter(|l| l.contains(INSTALL_FAILED))
            .collect::<Vec<_>>()
    );
    assert!(
        !log.contains("ASSERT"),
        "the firmware asserted: {:?}",
        log.lines()
            .filter(|l| l.contains("ASSERT"))
            .collect::<Vec<_>>()
    );
    // The firmware's own CPU count, from `MpInitLib`'s INIT-SIPI sweep. This
    // used to be left unasserted as "the load flake"; the flake was a torn read
    // of our ACPI PM timer (machine_x86::acpi::pm), not something a test has to
    // tolerate.
    assert!(
        log.contains(&format!(
            "MpInitLib: Find {} processors in system",
            MACHINE.vcpu_count
        )),
        "the firmware did not find exactly {} processors: {:?}",
        MACHINE.vcpu_count,
        log.lines()
            .filter(|l| l.contains("MpInitLib"))
            .collect::<Vec<_>>()
    );
    assert!(
        log.contains(BOOT_MANAGER),
        "the firmware did not reach the Boot Manager within {DEADLINE:?}:\n{}",
        tail(&log, 40)
    );
}
