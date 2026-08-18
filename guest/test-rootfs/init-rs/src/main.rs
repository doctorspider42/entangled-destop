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

use std::ffi::CString;
use std::io::Read;
use std::time::Instant;

/// Read size for the block probe; large enough to keep the ring busy, small
/// enough that a 64 MiB image still produces many requests.
const CHUNK: usize = 64 * 1024;

/// Never read more than this, whatever the command line asks for: the probe
/// must not turn into an unbounded loop on a malformed cmdline.
const MAX_BENCH_MIB: u64 = 4096;

fn main() {
    // The marker must match linux_boot::GUEST_READY_MARKER. Printed before any
    // probe runs, so the host's time-to-ready measurement is pure boot time.
    println!("VMHOST_GUEST_READY");

    // /proc is where the command line lives, so it has to be mounted before we
    // can find out whether a probe was requested at all.
    mount("proc", "/proc", "proc");
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if let Some(mib) = param(&cmdline, "entangled.blkbench=").and_then(|v| v.parse::<u64>().ok()) {
        mount("devtmpfs", "/dev", "devtmpfs");
        blk_bench(mib.min(MAX_BENCH_MIB));
    }

    // Restart, not power-off: the MVP machine has no ACPI, so power-off just
    // halts the vCPU forever. With `reboot=k` the kernel's restart chain ends
    // in a triple fault, which reaches the host as KVM_EXIT_SHUTDOWN and
    // terminates the VM cleanly.
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

/// Sequentially reads `mib` MiB from /dev/vda and reports the rate.
fn blk_bench(mib: u64) {
    let mut file = match std::fs::File::open("/dev/vda") {
        Ok(file) => file,
        Err(e) => {
            println!("VMHOST_TEST_FAIL blkbench cannot-open-/dev/vda:{e}");
            return;
        }
    };
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
    println!("VMHOST_TEST_OK blkbench bytes={read_total} ms={ms} kib_per_s={kib_per_s}");
}
