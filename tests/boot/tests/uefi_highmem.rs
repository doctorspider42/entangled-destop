//! UEFI boot with RAM above the high-RAM split (EPIC 18 / ADR-0003).
//!
//! Every other UEFI boot test builds a small guest — 2048 MiB — which is one
//! single guest-memory region and one E820 RAM entry above 1 MiB. The profiles
//! people actually run do not: `examples/ubuntu-desktop-live.toml` asks for
//! 4096 MiB, which splits into `[0, 0xc000_0000)` plus `[4 GiB, 5 GiB)` and
//! makes the firmware size a 64-bit PCI aperture above the top of RAM. Nothing
//! covered that shape, which is why the failure below could sit in the tree.
//!
//! What it asserts, beyond "the firmware still boots":
//!
//! * the firmware *saw* the high half — `PlatformAddHobCB: HighMemory` and a
//!   64-bit aperture based one page past the end of RAM, not on top of it;
//! * `AcpiPlatformDxe` installed our tables, with no `#GP` on the way;
//! * **the PVH hand-off block is byte-for-byte intact when the run ends.**
//!   That last one is the interesting assertion. EDK2 does not copy
//!   `hvm_start_info` at entry: the reset vector stashes the `%ebx` pointer and
//!   `InstallCloudHvTables()` dereferences it again at the *end* of DXE, after
//!   PCI enumeration, to reach `rsdp_paddr`. A boot where something has
//!   scribbled those pages in between ends as
//!   `X64 Exception Type - 0D(#GP)` inside `QemuFwCfgAcpiPlatform.dll` — a
//!   non-canonical `rsdp_paddr` faults as `#GP`, not as a page fault, which is
//!   why the symptom looks nothing like memory corruption. Checking the block
//!   directly turns that into a named failure instead of a hex dump.
//!
//! Self-skips without `/dev/kvm` or `artifacts/firmware/CLOUDHV.fd`
//! (`bash guest/firmware/build-cloudhv.sh`, ~2.5 min), like every test here.
//! The Windows twin is `crates/vmm-core/tests/whp_highmem.rs`.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::layout;
use machine_x86::serial::SerialConsole;
use vm_memory::{Bytes, GuestAddress, GuestMemory};
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

/// 4096 MiB: the size `examples/ubuntu-desktop-live.toml` uses, and the
/// smallest round number above `vmm_core::LOW_RAM_END` (3072 MiB) that gives
/// the high region a realistic 1 GiB rather than a sliver. Guest RAM is
/// demand-committed, so the test's real footprint is what the firmware touches.
const MACHINE: MachineConfig = MachineConfig {
    memory_mib: 4096,
    vcpu_count: 2,
};

const DEADLINE: Duration = Duration::from_secs(120);

const INSTALLING: &str = "root bridges have been connected, installing ACPI tables";
const INSTALL_FAILED: &str = "InstallAcpiTables:";
const BOOT_MANAGER: &str = "No bootable option or device was found";
/// The banner EDK2's exception handler prints. The reported regression's
/// signature: `0D(#GP - General Protection)` in `AcpiPlatformDxe`.
const EXCEPTION: &str = "X64 Exception Type";

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

#[test]
fn cloudhv_firmware_boots_a_guest_with_ram_above_the_split() {
    if !kvm_available() {
        return;
    }
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

    let hv = Hypervisor::open().unwrap();
    let mut vm = Vm::new(&hv, &MACHINE).unwrap();
    assert_eq!(
        vm.memory().num_regions(),
        2,
        "a guest above the split must be two regions"
    );

    machine_x86::mptable::write(vm.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(vm.memory(), MACHINE.vcpu_count).unwrap();

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).unwrap();
    let bus = MachineBus::new(serial).with_firmware_platform();

    let image = uefi_boot::FirmwareConfig {
        firmware: firmware.clone(),
    }
    .open()
    .unwrap();
    let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size).unwrap();

    // What the firmware must still be able to read at the end of DXE.
    let handoff = read_handoff(vm.memory());
    assert_eq!(handoff.magic, uefi_boot::pvh::XEN_HVM_START_MAGIC_VALUE);
    assert_eq!(handoff.rsdp_paddr, layout::ACPI_RSDP_START);

    let vcpus = vm.take_vcpus();
    for vcpu in &vcpus {
        x86_boot::setup_pvh_sregs(vm.memory(), vcpu).unwrap();
        if vcpu.index == 0 {
            x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr).unwrap();
        }
    }

    let started = Instant::now();
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();
    let outcomes = threads.join_or_stop(
        || {
            let text = capture.text();
            text.contains(BOOT_MANAGER) || text.contains(EXCEPTION) || started.elapsed() >= DEADLINE
        },
        Duration::from_millis(20),
    );
    let log = capture.text();

    for line in log.lines().filter(|line| {
        [
            "HighMemory",
            "FirstNonAddress",
            "Pci64Base",
            "Acpi",
            "ACPI",
            EXCEPTION,
            "Find image based on IP",
            BOOT_MANAGER,
        ]
        .iter()
        .any(|needle| line.contains(needle))
    }) {
        println!("{}", line.trim());
    }

    // The one that names the regression: the hand-off block the firmware
    // re-reads after PCI enumeration must be exactly what we wrote.
    let after = read_handoff(vm.memory());
    assert_eq!(
        after,
        handoff,
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
        "the firmware never saw the high-RAM region; outcomes {outcomes:?}"
    );
    assert!(
        log.contains(&format!(
            "Pci64Base=0x{:x}",
            vmm_core::HIGH_RAM_START + (mem_size - vmm_core::LOW_RAM_END)
        )),
        "the 64-bit PCI aperture must start past the end of RAM, not on top of it"
    );
    assert!(
        log.contains(INSTALLING),
        "AcpiPlatformDxe never got as far as installing tables"
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
    // used to be left unasserted as "the load flake"; the flake was a torn
    // read of our ACPI PM timer (machine_x86::acpi::pm), not something a test
    // has to tolerate.
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
        "the firmware did not reach the Boot Manager within {DEADLINE:?}"
    );
}

/// The three fields of `hvm_start_info` the firmware dereferences later.
#[derive(Debug, PartialEq, Eq)]
struct Handoff {
    magic: u32,
    rsdp_paddr: u64,
    memmap_paddr: u64,
    memmap: Vec<u8>,
}

fn read_handoff<M: vm_memory::GuestMemory>(mem: &M) -> Handoff {
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
