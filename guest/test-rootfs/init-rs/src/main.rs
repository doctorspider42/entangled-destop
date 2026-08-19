//! PID 1 of the test initramfs: prints the boot marker the integration
//! tests wait for, optionally runs a scripted probe, then reboots.
//!
//! Probes are requested on the kernel command line and each one prints exactly
//! one `VMHOST_TEST_OK <name> key=value …` or `VMHOST_TEST_FAIL <name> <why>`
//! line, which is all a host harness ever parses (never free-form kernel
//! output). Supported today:
//!
//!   `entangled.blkbench=<mib>`  sequential read of the first `<mib>` MiB of
//!                               `/dev/vda`, reporting bytes and milliseconds
//!                               (used to compare virtio-blk throughput with and
//!                               without the MVP-307 queue-notify offload).
//!   `entangled.pciscan=1`       reports what the kernel enumerated on the PCI
//!                               bus and which virtio drivers bound to it — the
//!                               guest-side evidence for the virtio-pci
//!                               transport (EPIC 19). There is no `lspci` in this
//!                               initramfs, so it reads sysfs directly.
//!   `entangled.poweroff=1`      power the machine off through ACPI instead of
//!                               rebooting: proves the FADT, the DSDT's `\_S5`
//!                               and the host's ACPI PM block agree. Opt-in,
//!                               because every other test wants the reboot path.

use std::ffi::CString;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

/// Read size for the block probe; large enough to keep the ring busy, small
/// enough that a 64 MiB image still produces many requests.
const CHUNK: usize = 64 * 1024;

/// Never read more than this, whatever the command line asks for: the probe
/// must not turn into an unbounded loop on a malformed cmdline.
const MAX_BENCH_MIB: u64 = 4096;

/// Cap on the PCI functions the scan reports, so a machine that grows a bus full
/// of devices cannot turn one probe line into an unbounded one.
const MAX_SCANNED_FUNCTIONS: usize = 16;

fn main() {
    // The marker must match linux_boot::GUEST_READY_MARKER. Printed before any
    // probe runs, so the host's time-to-ready measurement is pure boot time.
    println!("VMHOST_GUEST_READY");

    // /proc is where the command line lives, so it has to be mounted before we
    // can find out whether a probe was requested at all.
    mount("proc", "/proc", "proc");
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if param(&cmdline, "entangled.pciscan=").is_some() {
        // /sys before /dev: the scan wants sysfs, and it runs first so its
        // verdict is on the console even if the block probe then hangs.
        mount("sysfs", "/sys", "sysfs");
        pci_scan();
    }
    if let Some(mib) = param(&cmdline, "entangled.blkbench=").and_then(|v| v.parse::<u64>().ok()) {
        mount("devtmpfs", "/dev", "devtmpfs");
        blk_bench(mib.min(MAX_BENCH_MIB));
    }

    if param(&cmdline, "entangled.poweroff=").as_deref() == Some("1") {
        acpi_power_off();
    }

    // Restart, not power-off, by default: the reboot path works on every
    // machine we boot, ACPI or not. With `reboot=k` the kernel's restart chain
    // ends in a triple fault, which reaches the host as KVM_EXIT_SHUTDOWN and
    // terminates the VM cleanly. `entangled.poweroff=1` takes the ACPI route
    // instead.
    // SAFETY: plain syscalls; as PID 1 we hold CAP_SYS_BOOT. tcdrain flushes
    // the serial console before the reboot triple-faults the machine.
    unsafe {
        libc::tcdrain(1);
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_RESTART);
    }
    // Unreachable unless the kernel refuses; never return from PID 1 (that
    // would panic the kernel with a confusing message).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Asks the kernel to power the machine off through ACPI and reports what
/// happened on the way in.
///
/// `reboot(LINUX_REBOOT_CMD_POWER_OFF)` only reaches the ACPI path if
/// `acpi_sleep_init()` registered a power-off handler, which needs a FADT *and*
/// a `\_S5` package in the DSDT. When it did not, the kernel prints
/// "Power off not available" (older kernels) or halts, and the syscall returns
/// here — so a returning syscall is a real failure, not a race.
fn acpi_power_off() {
    // Which ACPI tables the guest actually parsed, straight from the kernel —
    // stronger evidence than a dmesg line, and it names them.
    mount("sysfs", "/sys", "sysfs");
    let mut tables: Vec<String> = std::fs::read_dir("/sys/firmware/acpi/tables")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name != "data" && name != "dynamic")
                .collect()
        })
        .unwrap_or_default();
    tables.sort();
    // What the host harness greps for. Printed before the syscall, because
    // afterwards there is no userspace left to print anything.
    println!(
        "VMHOST_TEST_OK poweroff via=acpi-s5 tables={}",
        if tables.is_empty() {
            "none".to_string()
        } else {
            tables.join(",")
        }
    );
    // SAFETY: plain syscalls; as PID 1 we hold CAP_SYS_BOOT. tcdrain flushes the
    // serial console first, because an ACPI power-off stops the machine between
    // one instruction and the next.
    unsafe {
        libc::tcdrain(1);
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    // Only reachable if the kernel had no way to power off.
    println!("VMHOST_TEST_FAIL poweroff kernel-refused-power-off");
}

/// Value of a `key=` parameter on the kernel command line.
fn param(cmdline: &str, key: &str) -> Option<String> {
    cmdline
        .split_ascii_whitespace()
        .find_map(|word| word.strip_prefix(key))
        .map(|value| value.to_string())
}

/// Mounts one pseudo-filesystem, ignoring failure: an already-mounted or
/// unavailable filesystem is not worth aborting the boot marker over.
fn mount(source: &str, target: &str, fstype: &str) {
    let _ = std::fs::create_dir_all(target);
    let (Ok(source), Ok(target), Ok(fstype)) = (
        CString::new(source),
        CString::new(target),
        CString::new(fstype),
    ) else {
        return;
    };
    // SAFETY: three valid NUL-terminated strings that outlive the call and a
    // null data pointer, which is how pseudo-filesystems are mounted.
    unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        );
    }
}

/// Reports what the kernel found on the PCI bus, and how much of it bound to a
/// virtio driver.
///
/// The whole point of the virtio-pci transport is that the guest discovers its
/// devices instead of being told where they are, so the evidence has to come from
/// the guest's own enumeration. `/sys/bus/pci/devices/*` is that enumeration:
/// each entry's `vendor`, `device` and `class` are what the kernel read out of
/// configuration space, and a `driver` symlink means a driver claimed it.
///
/// One line, machine-readable, like every other probe:
/// `VMHOST_TEST_OK pciscan functions=2 virtio=1 bound=1 devices=8086:0d57/060000,1af4:1042/018000:virtio-pci`
fn pci_scan() {
    let root = Path::new("/sys/bus/pci/devices");
    let Ok(entries) = std::fs::read_dir(root) else {
        // No sysfs directory at all means the kernel has no PCI bus — either
        // CONFIG_PCI is off or configuration mechanism #1 did not answer.
        println!("VMHOST_TEST_FAIL pciscan no-pci-bus-in-sysfs");
        return;
    };

    // Sorted so the line is stable across boots: readdir order is not.
    let mut addresses: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    addresses.sort();

    let mut described = Vec::new();
    let mut functions = 0usize;
    let mut virtio = 0usize;
    let mut bound = 0usize;
    for address in addresses.iter().take(MAX_SCANNED_FUNCTIONS) {
        let dir = root.join(address);
        let field = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map(|v| v.trim().trim_start_matches("0x").to_string())
                .unwrap_or_default()
        };
        let (vendor, device, class) = (field("vendor"), field("device"), field("class"));
        functions += 1;
        // 0x1af4 is the virtio vendor; a modern device id is 0x1040 + type.
        let is_virtio = vendor == "1af4";
        if is_virtio {
            virtio += 1;
            // Which IRQ the kernel settled on for this function. Worth reporting
            // on its own: x86 without ACPI cannot *route* a PCI interrupt, so it
            // logs "probably buggy MP table" and keeps the line the host wrote
            // into the interrupt_line register. If that fell through to 0, INTx
            // is broken even though everything else looks fine.
            println!("entangled-pciscan: {address} irq={}", field("irq"));
        }
        // The driver symlink's target name is the driver that claimed it, which
        // for a modern virtio function must be `virtio-pci`.
        let driver = std::fs::read_link(dir.join("driver"))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if is_virtio && !driver.is_empty() {
            bound += 1;
        }
        described.push(match driver.is_empty() {
            true => format!("{vendor}:{device}/{class}"),
            false => format!("{vendor}:{device}/{class}:{driver}"),
        });
    }

    if functions == 0 {
        println!("VMHOST_TEST_FAIL pciscan bus-present-but-empty");
        return;
    }
    println!(
        "VMHOST_TEST_OK pciscan functions={functions} virtio={virtio} bound={bound} devices={}",
        described.join(",")
    );
}

/// Interrupts delivered to virtio devices so far, from `/proc/interrupts`.
///
/// Reported alongside the block probe because it answers a question the byte
/// count cannot: *how* the completions arrived. A device whose interrupts are
/// lost can still finish a read — the driver notices used buffers the next time
/// something else wakes it — so "the read succeeded" is not evidence that the
/// interrupt path works. A count that climbs with the request count is.
///
/// Returns `None` when `/proc/interrupts` has no virtio line at all, which is
/// itself the interesting answer: the driver never registered a handler.
fn virtio_interrupts() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/interrupts").ok()?;
    let mut total = 0u64;
    let mut found = false;
    for line in text.lines() {
        // "  5:        142   IO-APIC   5-edge      virtio0"
        let Some((counts, description)) = line.rsplit_once("  ") else {
            continue;
        };
        if !description.trim().starts_with("virtio") {
            continue;
        }
        found = true;
        // Everything between the "n:" label and the controller name is per-CPU
        // counts; sum whatever parses.
        for field in counts.split_ascii_whitespace() {
            if let Ok(count) = field.parse::<u64>() {
                total = total.saturating_add(count);
            }
        }
    }
    found.then_some(total)
}

/// Sequentially reads `mib` MiB from /dev/vda and reports the rate, plus how
/// many interrupts the device raised while doing it.
fn blk_bench(mib: u64) {
    let mut file = match std::fs::File::open("/dev/vda") {
        Ok(file) => file,
        Err(e) => {
            println!("VMHOST_TEST_FAIL blkbench cannot-open-/dev/vda:{e}");
            return;
        }
    };
    // `/proc` is already mounted by the caller (the command line came from it).
    let before = virtio_interrupts();
    let target = mib.saturating_mul(1 << 20);
    let mut buf = vec![0u8; CHUNK];
    let mut read_total: u64 = 0;
    let started = Instant::now();
    while read_total < target {
        match file.read(&mut buf) {
            Ok(0) => break, // end of device
            Ok(n) => read_total = read_total.saturating_add(n as u64),
            Err(e) => {
                println!("VMHOST_TEST_FAIL blkbench read-error:{e}");
                return;
            }
        }
    }
    let ms = started.elapsed().as_millis().max(1);
    let kib_per_s = read_total / 1024 * 1000 / ms as u64;
    // -1 distinguishes "no virtio interrupt line exists" from "zero interrupts
    // on the line that does", which are different failures.
    let irqs = match (before, virtio_interrupts()) {
        (Some(before), Some(after)) => (after.saturating_sub(before)) as i64,
        _ => -1,
    };
    println!(
        "VMHOST_TEST_OK blkbench bytes={read_total} ms={ms} kib_per_s={kib_per_s} irqs={irqs}"
    );
}
