//! UEFI + ACPI boot test (EPIC 18 / ADR-0003 phase 2): the EDK2 CloudHv
//! firmware must find our ACPI tables through `hvm_start_info.rsdp_paddr` and
//! install them.
//!
//! Before ACPI, `AcpiPlatformDxe` parked with
//! `AcpiPlatformEntryPoint: waiting for root bridges to be connected` and then
//! failed `InstallAcpiTables` with `Not Found`, because `InstallCloudHvTables()`
//! dereferences `rsdp_paddr` and gives up on a zero (or unsigned) RSDP. With the
//! tables published it walks our XSDT, installs the FADT and MADT, then installs
//! the DSDT from the FADT's `X_DSDT`.
//!
//! Boots the firmware directly rather than through `entangled run`, for the same
//! reason the other tests in this crate build their own VM: the assertion is
//! about the serial log, and the test must self-skip when the firmware artifact
//! is missing (`bash guest/firmware/build-cloudhv.sh`, ~2.5 min).

#![cfg(target_os = "linux")]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

/// The firmware is chatty; 90 s is far more than the ~3 s a DEBUG build needs to
/// reach the Boot Manager, and keeps the test from being flaky under load.
const DEADLINE: Duration = Duration::from_secs(90);

/// What the firmware prints once it has walked our XSDT. Its `Not Found` sibling
/// is the phase-2 gap this test closes.
const INSTALLING: &str = "root bridges have been connected, installing ACPI tables";
const INSTALL_FAILED: &str = "InstallAcpiTables:";

/// The end of a healthy phase-1/2 boot.
const BOOT_MANAGER: &str = "No bootable option or device was found";

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
fn cloudhv_firmware_installs_our_acpi_tables() {
    if !kvm_available() {
        return;
    }
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    };

    // Two vCPUs, so "the CPU count is right" is a claim with content.
    let machine = MachineConfig {
        memory_mib: 2048,
        vcpu_count: 2,
    };
    let hv = Hypervisor::open().unwrap();
    let mut vm = Vm::new(&hv, &machine).unwrap();
    let mem_size = machine.memory_mib << 20;

    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).unwrap();
    let rsdp = machine_x86::acpi::write(vm.memory(), machine.vcpu_count).unwrap();
    assert_eq!(rsdp, machine_x86::layout::ACPI_RSDP_START);

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).unwrap();
    // The firmware-facing platform devices (host bridge on 0xcf8, RTC) plus the
    // ACPI PM block, which the base bus always carries.
    let bus = MachineBus::new(serial).with_firmware_platform();

    let image = uefi_boot::FirmwareConfig {
        firmware: firmware.clone(),
    }
    .open()
    .unwrap();
    let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size).unwrap();

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
            text.contains(BOOT_MANAGER) || started.elapsed() >= DEADLINE
        },
        Duration::from_millis(20),
    );
    let log = capture.text();

    // Interesting lines first, so a failure (and `--nocapture`) shows the
    // evidence rather than 300 KiB of DXE dispatch.
    for line in log.lines().filter(|line| {
        [
            "Acpi",
            "ACPI",
            "MpInitLib",
            "processors",
            "Cloud Hypervisor",
            BOOT_MANAGER,
        ]
        .iter()
        .any(|needle| line.contains(needle))
    }) {
        println!("{}", line.trim());
    }

    assert!(
        log.contains(INSTALLING),
        "AcpiPlatformDxe never got as far as installing tables; outcomes {outcomes:?}"
    );
    // `OnRootBridgesConnected` only prints this line on failure, with the status
    // appended ("Not Found" when the RSDP is missing or unusable).
    assert!(
        !log.contains(INSTALL_FAILED),
        "InstallAcpiTables failed: {:?}",
        log.lines()
            .filter(|line| line.contains(INSTALL_FAILED))
            .collect::<Vec<_>>()
    );
    assert!(
        !log.contains("ASSERT"),
        "the firmware asserted: {:?}",
        log.lines()
            .filter(|line| line.contains("ASSERT"))
            .collect::<Vec<_>>()
    );
    // The firmware's own CPU count, from MpInitLib's INIT-SIPI sweep. It must
    // agree with the MADT we published rather than with CloudHvX64.dsc's
    // PcdCpuMaxLogicalProcessorNumber of 254.
    assert!(
        log.contains(&format!(
            "MpInitLib: Find {} processors in system",
            machine.vcpu_count
        )),
        "the firmware did not find exactly {} processors",
        machine.vcpu_count
    );
    assert!(
        log.contains(BOOT_MANAGER),
        "the firmware did not reach the Boot Manager within {DEADLINE:?}"
    );
}
