//! Persistent UEFI variables (UEFI-1804, ADR-0003): the firmware must find a
//! writable flash device, keep its variables in it, and **find them again after
//! the VM has stopped**.
//!
//! This is the test that closes ADR-0003's one remaining open gap-map row. It
//! boots the real firmware twice against the same NVRAM file and asserts, in the
//! order the evidence appears:
//!
//! | Claim | Evidence |
//! |---|---|
//! | the CFI command set is right | `QemuFlashDetected => FD behaves as FLASH, writable` — anything else and the firmware silently uses RAM |
//! | the flash address is right | that line is preceded by `Attempting flash detection at FFC000xx`, i.e. inside `layout::PFLASH_BASE`'s first block (a stock build probes `4FFFD0`) |
//! | the emulated FVB replaces the RAM one | `Disabling EMU Variable FVB since flash variables appear to be supported` |
//! | the store's *format* is right | no `ASSERT [VariableRuntimeDxe]`; a blank store asserts on `VariableStore->Size` |
//! | variables really landed in the file | the file contains `BootOrder` and `Boot0000` as UTF-16 names, and the host counted the byte programs |
//! | they survive a stop | the second boot re-reads them: the same `Boot####` list, without re-creating it |
//!
//! It needs `artifacts/firmware/CLOUDHV.fd` **built with the flash PCD override**
//! (`bash guest/firmware/build-cloudhv.sh`; `ENTANGLED_FW_PFLASH=0` builds a
//! firmware this test correctly fails against) and `/dev/kvm`. It self-skips
//! without either. ~10 s.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::layout;
use machine_x86::pflash::Pflash;
use machine_x86::serial::SerialConsole;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

/// A DEBUG firmware reaches the Boot Manager in ~3 s; this is slack, not budget.
const DEADLINE: Duration = Duration::from_secs(90);

/// The verdict line. `FD behaves as RAM` / `as ROM` / `FLASH, write-protected`
/// are the three ways to fail it, and each names a different bug in the device.
const FLASH_DETECTED: &str = "QemuFlashDetected => FD behaves as FLASH, writable";
/// The probe address, minus its last two digits. The firmware scans block 0 for
/// the first byte that is not `0x00`, `0x50` or `0x70`, so *where* inside the
/// block it probes depends on the store's contents: `FFC00000` on erased flash,
/// `FFC00010` on a formatted store (the firmware volume header opens with a
/// 16-byte zero vector, which the scan walks over). Both are the right device;
/// what matters is the page.
const FLASH_ADDRESS: &str = "QEMU Flash: Attempting flash detection at FFC000";
const WRITABLE_FVB: &str = "Installing QEMU flash FVB";
const EMU_FVB_DISABLED: &str = "Disabling EMU Variable FVB since flash variables appear to be";
/// What the *stock* firmware does instead, kept here as the counter-evidence:
/// if this appears, the build has no flash PCD override.
const EMU_FVB_USED: &str = "EMU Variable FVB: Using pre-reserved block";
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

struct Boot {
    log: String,
    programmed_bytes: u64,
    erased_blocks: u64,
    store_errors: u64,
}

/// Boots the firmware once with `nvram` attached as its variable store.
fn boot_with_nvram(firmware: &Path, nvram: &Path) -> Boot {
    let machine = MachineConfig {
        memory_mib: 2048,
        vcpu_count: 1,
    };
    let hv = Hypervisor::open().expect("kvm");
    let mut vm = Vm::new(&hv, &machine).expect("vm");
    let mem_size = machine.memory_mib << 20;

    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).expect("mp table");
    machine_x86::acpi::write(vm.memory(), machine.vcpu_count).expect("acpi tables");

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).expect("serial");
    let flash = Arc::new(Mutex::new(
        Pflash::open(nvram).expect("open the UEFI variable store"),
    ));
    let bus = MachineBus::new(serial)
        .with_firmware_platform()
        .with_pflash(Arc::clone(&flash));

    let image = uefi_boot::FirmwareConfig {
        firmware: firmware.to_path_buf(),
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

    let started = Instant::now();
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).expect("vcpus");
    // BDS writes its boot options *before* it reports having nothing to boot, so
    // that line is a safe place to stop — the variable writes have happened.
    let _ = threads.join_or_stop(
        || {
            let text = capture.text();
            text.contains(BOOT_MANAGER) || text.contains("ASSERT") || started.elapsed() >= DEADLINE
        },
        Duration::from_millis(20),
    );

    let stats = flash.lock().expect("pflash lock").stats();
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
        None => PathBuf::from(std::env::var_os("HOME")?).join("entangled-vms"),
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
fn uefi_variables_survive_a_vm_restart() {
    if !kvm_available() {
        return;
    }
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    };
    let Some(nvram) = scratch_nvram("nvram-persistence") else {
        return;
    };

    // ---- first boot: a fresh store ----
    let first = boot_with_nvram(&firmware, &nvram);
    println!("--- first boot ---");
    for line in interesting(&first.log) {
        println!("{line}");
    }
    println!(
        "programmed {} bytes, erased {} blocks",
        first.programmed_bytes, first.erased_blocks
    );

    assert!(
        first.log.contains(FLASH_ADDRESS),
        "the firmware probed for flash somewhere other than {:#x} — the build's \
         PcdOvmfFdBaseAddress and machine_x86::layout::PFLASH_BASE disagree",
        layout::PFLASH_BASE
    );
    assert!(
        first.log.contains(FLASH_DETECTED),
        "the firmware did not accept the flash device. Its verdict lines are the \
         diagnosis: \"behaves as RAM\" means a write read back unchanged (clear-status \
         must return to read-array), \"behaves as ROM\" means the status register did \
         not answer, \"write-protected\" means the program set status bit 4. Log:\n{}",
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
    assert_eq!(first.store_errors, 0, "the host failed to persist a write");
    assert!(
        first.programmed_bytes > 0,
        "the firmware never wrote a byte, so this test proves nothing"
    );

    // The variables are in the *file*, not in a process that is now gone.
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

    // ---- second boot: the same file, a new VM ----
    let second = boot_with_nvram(&firmware, &nvram);
    println!("--- second boot ---");
    for line in interesting(&second.log) {
        println!("{line}");
    }
    println!(
        "programmed {} bytes, erased {} blocks",
        second.programmed_bytes, second.erased_blocks
    );

    assert!(
        second.log.contains(FLASH_DETECTED),
        "flash gone on the second boot"
    );
    assert!(
        !second.log.contains("ASSERT"),
        "the firmware asserted while reading an existing store — the format written \
         on the first boot is not one it can read back: {:?}",
        second
            .log
            .lines()
            .filter(|l| l.contains("ASSERT"))
            .collect::<Vec<_>>()
    );
    assert_eq!(second.store_errors, 0);

    // The load-bearing assertion: the second boot *reused* the store instead of
    // formatting it. A store the firmware rejected would be erased again (132
    // blocks, the whole 0x84000) and re-populated from scratch.
    assert_eq!(
        second.erased_blocks, 0,
        "the second boot erased {} blocks, i.e. it threw the existing variable \
         store away rather than reading it",
        second.erased_blocks
    );
    assert!(
        second.programmed_bytes < first.programmed_bytes,
        "the second boot wrote as much as the first ({} vs {} bytes), which is what \
         re-creating every boot option from nothing looks like",
        second.programmed_bytes,
        first.programmed_bytes
    );
    // And the same options are there, read out of the file the first VM wrote.
    for expected in ["Boot0000", "Boot0001", "Boot0002"] {
        assert!(
            second.log.contains(expected),
            "BDS did not list {expected} on the second boot"
        );
    }

    let _ = std::fs::remove_file(&nvram);
}
