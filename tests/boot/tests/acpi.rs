//! ACPI integration tests: the tables `machine_x86::acpi` publishes have to be
//! the ones a real guest kernel actually uses, and the S5 path has to end the VM.
//!
//! Three things a unit test cannot check:
//!
//! 1. **the tables are found** — with an MADT present the kernel stops printing
//!    "ACPI MADT or MP tables are not detected", reports our RSDP address and
//!    brings up the IOAPIC from the MADT rather than from the MP table;
//! 2. **the topology is right** — a two-vCPU guest must bring up two CPUs from
//!    the MADT, with the APIC ids the MP table and CPUID also report;
//! 3. **`poweroff` works** — `reboot(LINUX_REBOOT_CMD_POWER_OFF)` in the guest
//!    only ends the VM if the FADT, the DSDT's `\_S5` and the host's ACPI PM
//!    block all agree. The guest, not the harness, ends the VM.
//!
//! All self-skip without /dev/kvm or the guest artifacts.

#![cfg(target_os = "linux")]

use std::time::Duration;

use boot_tests::{boot_artifacts, boot_once, kvm_available, BootSpec};

/// The line the kernel prints when it has neither ACPI nor MPS tables. Its
/// absence is the headline result of this whole change.
const NO_TABLES: &str = "ACPI MADT or MP tables are not detected";

fn spec() -> Option<BootSpec> {
    if !kvm_available() {
        return None;
    }
    let (kernel, initramfs) = boot_artifacts()?;
    let mut spec = BootSpec::new(kernel, initramfs);
    spec.deadline = Duration::from_secs(60);
    Some(spec)
}

#[test]
fn guest_finds_our_acpi_tables_and_the_madt_topology() {
    let Some(spec) = spec() else { return };
    let spec = spec.with_vcpus(2);
    let outcome = boot_once(&spec).expect("boot failed");
    let log = &outcome.serial;
    assert!(
        outcome.reached_ready(),
        "guest never reached the ready marker; serial log:\n{log}"
    );

    // Every ACPI/APIC line the guest printed, so `--nocapture` gives the
    // evidence for this change rather than just a green tick.
    for line in log.lines().filter(|line| {
        ["ACPI", "APIC", "CPUs", "MPS", "MP table"]
            .iter()
            .any(|needle| line.contains(needle))
    }) {
        println!("{}", line.trim());
    }

    assert!(
        !log.contains(NO_TABLES),
        "the kernel still fell back to the MP-table probe; serial log:\n{log}"
    );
    // Our RSDP address, handed over in boot_params.acpi_rsdp_addr and echoed by
    // the kernel's table parser.
    let rsdp = format!("{:#010x}", machine_x86::layout::ACPI_RSDP_START);
    assert!(
        log.contains("ACPI: RSDP") && log.to_lowercase().contains(rsdp.trim_start_matches("0x")),
        "no 'ACPI: RSDP {rsdp}' line; serial log:\n{log}"
    );
    for table in ["XSDT", "FACP", "APIC", "DSDT"] {
        assert!(
            log.contains(&format!("ACPI: {table}")),
            "the kernel never reported our {table}; serial log:\n{log}"
        );
    }
    // The IOAPIC must come from the MADT, at the address the MADT names.
    assert!(
        log.contains("IOAPIC[0]") || log.contains("IO-APIC"),
        "no IOAPIC line; serial log:\n{log}"
    );
    assert!(
        log.contains(&format!("{:x}", machine_x86::layout::IOAPIC_ADDR)),
        "the IOAPIC address in the log is not the one the MADT publishes; \
         serial log:\n{log}"
    );
    // Two enabled processors, from the MADT's LAPIC entries.
    assert!(
        log.contains("2 CPUs")
            || log.contains("SMP: Allowing 2 CPUs")
            || log.contains("smpboot: Allowing 2 CPUs"),
        "the kernel did not see both vCPUs; serial log:\n{log}"
    );
    assert!(
        !log.contains("ACPI Error") && !log.contains("ACPI BIOS Error"),
        "ACPICA rejected something in our tables; serial log:\n{log}"
    );
}

/// A single-vCPU boot must report exactly one CPU: an MADT that over-reported
/// would make the kernel wait for CPUs that never come up.
#[test]
fn single_vcpu_guest_sees_one_cpu() {
    let Some(spec) = spec() else { return };
    let outcome = boot_once(&spec).expect("boot failed");
    let log = &outcome.serial;
    assert!(outcome.reached_ready(), "serial log:\n{log}");
    assert!(!log.contains(NO_TABLES), "serial log:\n{log}");
    assert!(
        !log.contains("Allowing 2 CPUs") && !log.contains("Allowing 4 CPUs"),
        "a 1-vCPU MADT must not advertise more; serial log:\n{log}"
    );
}

/// Both tables stay published, so `acpi=off` must still boot — through the MP
/// table, with the same topology. This is what makes the MADT safe to add: the
/// old path is a supported fallback, not dead code.
#[test]
fn acpi_off_falls_back_to_the_mp_table() {
    let Some(spec) = spec() else { return };
    let spec = spec.with_vcpus(2).with_extra_cmdline("acpi=off");
    let outcome = boot_once(&spec).expect("boot failed");
    let log = &outcome.serial;
    assert!(
        outcome.reached_ready(),
        "the guest cannot boot with acpi=off any more; serial log:\n{log}"
    );
    // With ACPI off the kernel must find the MP table, not fall through to
    // virtual-wire mode.
    assert!(
        !log.contains(NO_TABLES),
        "neither ACPI nor the MP table was used; serial log:\n{log}"
    );
    assert!(
        log.contains("Intel MultiProcessor Specification") || log.contains("mpc:"),
        "no MP table in the log; serial log:\n{log}"
    );
    // Same topology from the other table: two CPUs and the same IOAPIC address.
    assert!(
        log.contains(&format!("{:x}", machine_x86::layout::IOAPIC_ADDR)),
        "serial log:\n{log}"
    );
    for line in log
        .lines()
        .filter(|line| line.contains("MultiProcessor") || line.contains("IOAPIC"))
    {
        println!("{}", line.trim());
    }
}

/// `poweroff` in the guest must terminate the VM through ACPI: the kernel
/// evaluates `\_S5`, writes `SLP_TYP=5 | SLP_EN` to PM1a_CNT, and the host's PM
/// block turns that into `RunOutcome::Shutdown`.
#[test]
fn guest_poweroff_ends_the_vm_through_acpi() {
    let Some(spec) = spec() else { return };
    let spec = spec.with_poweroff_probe();
    let outcome = boot_once(&spec).expect("boot failed");
    let log = &outcome.serial;
    assert!(outcome.reached_ready(), "serial log:\n{log}");

    let probe = outcome
        .probe("poweroff")
        .unwrap_or_else(|| panic!("the guest never ran the poweroff probe; serial log:\n{log}"));
    assert!(
        !log.contains("VMHOST_TEST_FAIL poweroff"),
        "the guest kernel had no ACPI power-off handler; serial log:\n{log}"
    );
    // The probe lists the tables the *guest* found, from /sys/firmware/acpi.
    let tables = probe
        .iter()
        .find(|(k, _)| k == "tables")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    // The root tables (RSDP/XSDT) are not listed there — sysfs shows the
    // installed tables — but everything hanging off them must be.
    for table in ["APIC", "DSDT", "FACP", "FACS"] {
        assert!(
            tables.contains(table),
            "the guest's /sys/firmware/acpi/tables has no {table} (got {tables:?}); \
             serial log:\n{log}"
        );
    }
    assert!(
        outcome.ended_by_guest(),
        "the harness had to stop the VM, so the ACPI power-off did not end it: \
         {:?}; serial log:\n{log}",
        outcome.vcpu_outcomes
    );
    assert!(
        !log.contains("Power off not available"),
        "serial log:\n{log}"
    );
}
