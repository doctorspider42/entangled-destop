//! UEFI firmware on WHP (backlog EPIC 17 phase 4, ADR-0003): EDK2 CloudHv
//! boots through the PVH entry to the Boot Manager, and its variables persist
//! across two VMs sharing one NVRAM file.
//!
//! The Windows mirror of `tests/boot/tests/uefi_nvram.rs`, and deliberately the
//! *persistence* test rather than a plain firmware boot: it subsumes one. The
//! chain it exercises on WHP, in the order the firmware hits it:
//!
//! * PVH entry in 32-bit protected mode (`setup_pvh_sregs`/`setup_pvh_regs`,
//!   boot CPU only — an AP must stay in the reset state WHP created it in);
//! * the host bridge answer, the ACPI PM timer and the RTC as port I/O exits;
//! * the pflash CFI window as `MemoryAccess` exits through WHP's instruction
//!   emulator — the flash *probe* is the strictest MMIO client this machine
//!   has, because a single mis-emulated byte makes the firmware silently fall
//!   back to RAM variables;
//! * `MpInitLib`'s INIT-SIPI sweep against WHP's own AP handling;
//! * the variable writes landing in the file, and a second partition reading
//!   them back.
//!
//! It needs `artifacts/firmware/CLOUDHV.fd` built with the flash PCD override
//! (`bash guest/firmware/build-cloudhv.sh`; an `ENTANGLED_FW_PFLASH=0` build
//! correctly fails this test) and self-skips without it, like every whp_* test.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::layout;
use machine_x86::pflash::Pflash;
use machine_x86::serial::SerialConsole;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::MachineConfig;

mod whp_common;
use whp_common::{artifact, dump_log, tail, whp_guard, Capture};

/// A DEBUG firmware is chatty and every one of its MMIO accesses goes through
/// the instruction emulator; measured ~35 s to the Boot Manager in a debug
/// build, so this is slack, not budget.
const DEADLINE: Duration = Duration::from_secs(300);

/// Two vCPUs on purpose: the firmware's `MpInitLib` INIT-SIPI sweep is part of
/// what this test exercises on WHP.
const MACHINE: MachineConfig = MachineConfig {
    memory_mib: 2048,
    vcpu_count: 2,
};

const FLASH_DETECTED: &str = "QemuFlashDetected => FD behaves as FLASH, writable";
const FLASH_ADDRESS: &str = "QEMU Flash: Attempting flash detection at FFC000";
const WRITABLE_FVB: &str = "Installing QEMU flash FVB";
const EMU_FVB_DISABLED: &str = "Disabling EMU Variable FVB since flash variables appear to be";
const EMU_FVB_USED: &str = "EMU Variable FVB: Using pre-reserved block";
const BOOT_MANAGER: &str = "No bootable option or device was found";

struct Boot {
    log: String,
    programmed_bytes: u64,
    erased_blocks: u64,
    store_errors: u64,
}

/// Boots the firmware once on WHP with `nvram` attached as its variable store.
fn boot_with_nvram(hv: &WhpHypervisor, firmware: &Path, nvram: &Path) -> Boot {
    let mut partition = WhpPartition::with_options(hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).expect("mp table");
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).expect("acpi tables");

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");
    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let flash = Arc::new(Mutex::new(
        Pflash::open(nvram).expect("open the UEFI variable store"),
    ));
    let bus = MachineBus::new(serial)
        .with_firmware_platform()
        .with_pflash(Arc::clone(&flash))
        .with_irqchip(Arc::clone(&irqchip));

    let image = uefi_boot::FirmwareImage::read(firmware).expect("firmware image");
    assert!(
        matches!(image.kind(), uefi_boot::FirmwareKind::PvhElf { .. }),
        "CLOUDHV.fd must be a PVH ELF"
    );
    let boot = uefi_boot::load_pvh(partition.memory(), &image, MACHINE.memory_mib << 20)
        .expect("load firmware");

    let vcpus = partition.take_vcpus();
    {
        // Boot CPU only: the firmware's own INIT-SIPI sweep brings VP 1 up from
        // the reset state WHP models for it (see WhpPartition's SMP notes).
        let vcpu = &vcpus[0];
        x86_boot::setup_pvh_sregs(partition.memory(), vcpu).expect("pvh sregs");
        x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr).expect("pvh regs");
    }

    let started = Instant::now();
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).expect("vcpus");
    // BDS writes its boot options *before* it reports having nothing to boot, so
    // that line is a safe place to stop — the variable writes have happened.
    let _ = threads.join_or_stop(
        || {
            let text = capture.text();
            text.contains(BOOT_MANAGER) || text.contains("ASSERT") || started.elapsed() >= DEADLINE
        },
        Duration::from_millis(50),
    );

    let stats = flash.lock().expect("pflash lock").stats();
    drop(partition);
    drop(irqchip);
    Boot {
        log: capture.text(),
        programmed_bytes: stats.programmed_bytes,
        erased_blocks: stats.erased_blocks,
        store_errors: stats.store_errors,
    }
}

/// UEFI variable names are UTF-16LE in the store; this is how a human greps for
/// them, and it is enough to prove *which* variables are in the file.
fn variable_names(nvram: &Path) -> Vec<String> {
    let bytes = std::fs::read(nvram).expect("read the NVRAM file");
    let varstore = &bytes[..layout::PFLASH_VARSTORE_SIZE as usize];
    let mut names = Vec::new();
    let mut current = String::new();
    for pair in varstore.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        match char::from_u32(u32::from(unit)) {
            Some(c) if c.is_ascii_graphic() => current.push(c),
            _ => {
                if current.len() >= 4 {
                    names.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
    }
    names
}

fn scratch_nvram(name: &str) -> Option<PathBuf> {
    let dir = match std::env::var_os("ENTANGLED_SCRATCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::temp_dir().join("entangled-tests"),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("skipping: cannot create {}: {e}", dir.display());
        return None;
    }
    let path = dir.join(format!("{name}-{}.nvram", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Some(path)
}

fn interesting(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| {
            [
                "QEMU Flash",
                "QemuFlashDetected",
                "QEMU flash FVB",
                "EMU Variable FVB",
                "MpInitLib",
                "Boot000",
                "ASSERT",
                BOOT_MANAGER,
            ]
            .iter()
            .any(|needle| line.contains(needle))
        })
        .map(str::trim)
        .collect()
}

#[test]
fn uefi_variables_survive_a_vm_restart_on_whp() {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return;
        }
    };
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    };
    let Some(nvram) = scratch_nvram("whp-nvram-persistence") else {
        return;
    };

    // ---- first boot: a fresh store ----
    let first = boot_with_nvram(&hv, &firmware, &nvram);
    dump_log(&first.log);
    eprintln!("--- first boot ---");
    for line in interesting(&first.log) {
        eprintln!("{line}");
    }
    eprintln!(
        "programmed {} bytes, erased {} blocks",
        first.programmed_bytes, first.erased_blocks
    );

    assert!(
        first.log.contains(FLASH_ADDRESS),
        "the firmware probed for flash somewhere other than {:#x} — the build's \
         PcdOvmfFdBaseAddress and machine_x86::layout::PFLASH_BASE disagree; tail:\n{}",
        layout::PFLASH_BASE,
        tail(&first.log, 40)
    );
    assert!(
        first.log.contains(FLASH_DETECTED),
        "the firmware did not accept the flash device. On WHP every probe access \
         is an emulated MMIO exit, so a wrong width or a lost write shows up here \
         first. Verdict lines:\n{}",
        interesting(&first.log).join("\n")
    );
    assert!(
        first.log.contains(WRITABLE_FVB),
        "flash was detected but the writable FVB was not installed"
    );
    assert!(
        first.log.contains(EMU_FVB_DISABLED) && !first.log.contains(EMU_FVB_USED),
        "the RAM-backed variable store is still in charge, so nothing would persist"
    );
    assert!(
        !first.log.contains("ASSERT"),
        "the firmware asserted: {:?}",
        first
            .log
            .lines()
            .filter(|l| l.contains("ASSERT"))
            .collect::<Vec<_>>()
    );
    assert!(
        first.log.contains("MpInitLib: Find 2 processors in system"),
        "the firmware's INIT-SIPI sweep did not find both CPUs; tail:\n{}",
        tail(&first.log, 40)
    );
    assert_eq!(first.store_errors, 0, "the host failed to persist a write");
    assert!(
        first.programmed_bytes > 0,
        "the firmware never wrote a byte, so this test proves nothing"
    );

    // The variables are in the *file*, not in a partition that is now gone.
    let names = variable_names(&nvram);
    for expected in ["BootOrder", "Boot0000", "PlatformLang"] {
        assert!(
            names.iter().any(|n| n == expected),
            "no {expected} variable in {}; found {names:?}",
            nvram.display()
        );
    }
    assert_eq!(
        std::fs::metadata(&nvram).unwrap().len(),
        layout::PFLASH_NVRAM_SIZE,
        "the guest must not be able to change the store's size"
    );

    // ---- second boot: the same file, a new partition ----
    let second = boot_with_nvram(&hv, &firmware, &nvram);
    eprintln!("--- second boot ---");
    for line in interesting(&second.log) {
        eprintln!("{line}");
    }
    eprintln!(
        "programmed {} bytes, erased {} blocks",
        second.programmed_bytes, second.erased_blocks
    );

    assert!(
        second.log.contains(FLASH_DETECTED),
        "the second boot rejected the store the first one wrote; tail:\n{}",
        tail(&second.log, 40)
    );
    assert!(
        second.log.contains(EMU_FVB_DISABLED),
        "the second boot fell back to RAM variables"
    );
    assert!(
        !second.log.contains("ASSERT"),
        "the second boot asserted: {:?}",
        second
            .log
            .lines()
            .filter(|l| l.contains("ASSERT"))
            .collect::<Vec<_>>()
    );
    // Reuse, not re-creation: the store already holds the boot options, so the
    // second boot programs less and erases nothing.
    assert_eq!(second.store_errors, 0);
    assert_eq!(
        second.erased_blocks, 0,
        "a second boot against a good store has nothing to erase"
    );
    assert!(
        second.programmed_bytes < first.programmed_bytes,
        "the second boot programmed {} bytes, the first {} — it should be reusing \
         the store, not rebuilding it",
        second.programmed_bytes,
        first.programmed_bytes
    );

    let _ = std::fs::remove_file(&nvram);
}
