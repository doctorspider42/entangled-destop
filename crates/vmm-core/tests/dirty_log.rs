//! What each hypervisor's dirty-page log does and does not see
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! Both hosts can tell us which guest pages were written: KVM through
//! `KVM_MEM_LOG_DIRTY_PAGES` + `KVM_GET_DIRTY_LOG`, WHP through
//! `WHvMapGpaRangeFlagTrackDirtyPages` + `WHvQueryGpaRangeDirtyBitmap`. The
//! interesting question is not whether that works — it does, and
//! [`a_guest_write_is_reported`] shows it — but **what is missing from it**.
//!
//! [`a_host_write_is_invisible_to_the_dirty_log`] is the finding ADR-0006's
//! decision rests on: this process writes two pages of guest RAM with tracking
//! already armed, and **neither host reports either of them**. Both track
//! writes by protecting the guest's second-level page tables, and a `memcpy`
//! from the VMM into the allocation those tables point at faults nothing.
//!
//! Every virtio-blk read completion, every received packet, every used-ring
//! update and every boot image is exactly such a write. A snapshot that carried
//! "everything the dirty log reported since the base" would come back with a
//! guest whose page cache still held the block it had read a minute earlier —
//! silently, and with no path back to this file.
//!
//! The log errs the other way too, on one host: KVM on AMD reports a page the
//! guest only **executed** from (measured 2026-09-09; see
//! [`a_guest_write_is_reported`]). That direction is harmless — an incremental
//! that copied it would only copy too much — and the two together are the
//! honest summary: *the dirty log is neither a subset nor a superset of "what
//! changed".*
//!
//! Self-skipping without a hypervisor, like every other test here.
//!
//! ```bash
//! cargo test -p vmm-core --test dirty_log -- --nocapture
//! ```

#![cfg(any(target_os = "linux", windows))]

use std::sync::atomic::AtomicBool;

use vm_memory::{Bytes, GuestAddress};
use vmm_core::hv::{DirtyLog, DirtyPages, DIRTY_PAGE_SIZE};
use vmm_core::{ExitHandler, MachineConfig, RunOutcome};

/// Where the test guest's code is placed — by the **host**, which is the point.
const CODE_ADDR: u64 = 0x1000;
/// The two pages the guest writes with its own instructions.
const GUEST_WRITE_A: u64 = 0x8000;
const GUEST_WRITE_B: u64 = 0x9000;

const CONFIG: MachineConfig = MachineConfig {
    memory_mib: 16,
    vcpu_count: 1,
};

/// `mov byte [0x8000], 0x42 ; mov byte [0x9000], 0x43`, 16-bit real mode.
///
/// `C6 06 <disp16> <imm8>` is `mov byte ptr [disp16], imm8` with the default
/// `ds = 0`, so the two guest-physical addresses above are exactly what the
/// guest writes to.
const WRITES: [u8; 10] = [
    0xc6, 0x06, 0x00, 0x80, 0x42, // mov byte [0x8000], 0x42
    0xc6, 0x06, 0x00, 0x90, 0x43, // mov byte [0x9000], 0x43
];

/// How the test guest ends, which differs per host for the reason the two smoke
/// tests document: KVM's in-kernel irqchip swallows `hlt` (the vCPU would block
/// for ever), so the KVM guest triple-faults on `ud2` instead; WHP with local
/// APIC emulation off reports `hlt` as an exit.
#[cfg(target_os = "linux")]
const TERMINATOR: [u8; 2] = [0x0f, 0x0b]; // ud2
#[cfg(windows)]
const TERMINATOR: [u8; 1] = [0xf4]; // hlt

fn code() -> Vec<u8> {
    let mut code = WRITES.to_vec();
    code.extend_from_slice(&TERMINATOR);
    code
}

#[derive(Default)]
struct Recorder;

impl ExitHandler for Recorder {
    fn io_out(&mut self, _port: u16, _data: &[u8]) {}
    fn io_in(&mut self, _port: u16, data: &mut [u8]) {
        data.fill(0xff);
    }
    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(0);
    }
}

/// Whether any region's log has the page containing `gpa`.
fn dirty_at(log: &[DirtyPages], gpa: u64) -> bool {
    log.iter().any(|region| region.contains_dirty_gpa(gpa))
}

fn total_dirty(log: &[DirtyPages]) -> u64 {
    log.iter().map(DirtyPages::count).sum()
}

/// Serialises the tests. WHP allows one partition per host **process** to have
/// guest memory mapped and cargo runs tests in threads of one process (see
/// `whp_smoke.rs`); KVM has no such limit, but taking the same lock on both
/// hosts keeps this file free of a second `cfg`.
fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ------------------------------------------------------------------ per host

#[cfg(target_os = "linux")]
mod host {
    use super::*;
    use vmm_core::{Hypervisor, Vm};

    pub struct Machine {
        pub vm: Vm,
        pub log: std::sync::Arc<dyn DirtyLog>,
    }

    /// A VM with tracking already armed, or `None` with a note.
    pub fn machine_or_skip() -> Option<Machine> {
        let hv = match Hypervisor::open() {
            Ok(hv) => hv,
            Err(e) => {
                eprintln!("skipping KVM dirty-log test: {e}");
                return None;
            }
        };
        let vm = Vm::new(&hv, &CONFIG).expect("create a VM");
        let log = vm.dirty_log();
        log.set_dirty_logging(true).expect("arm dirty logging");
        Some(Machine { vm, log })
    }

    /// Runs the blob already in guest RAM to completion.
    pub fn run(machine: &mut Machine) {
        let mut vcpus = machine.vm.take_vcpus();
        let mut vcpu = vcpus.remove(0);
        let mut sregs = vcpu.fd().get_sregs().expect("sregs");
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        // IDT limit 0 so the final `ud2` escalates to a triple fault, which is
        // a deterministic `KVM_EXIT_SHUTDOWN` (see `smoke.rs`).
        sregs.idt.base = 0;
        sregs.idt.limit = 0;
        vcpu.fd().set_sregs(&sregs).expect("set sregs");
        let mut regs = vcpu.fd().get_regs().expect("regs");
        regs.rip = CODE_ADDR;
        regs.rflags = 2;
        vcpu.fd().set_regs(&regs).expect("set regs");

        let running = AtomicBool::new(true);
        let outcome = vcpu.run_loop(&mut Recorder, &running).expect("run");
        assert!(
            matches!(outcome, RunOutcome::Halted | RunOutcome::Shutdown),
            "the test guest did not terminate: {outcome:?}"
        );
    }
}

#[cfg(windows)]
mod host {
    use super::*;
    use vmm_core::hv::VcpuRegisters;
    use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};

    pub struct Machine {
        pub vm: WhpPartition,
        pub log: std::sync::Arc<dyn DirtyLog>,
    }

    pub fn machine_or_skip() -> Option<Machine> {
        let hv = match WhpHypervisor::open() {
            Ok(hv) => hv,
            Err(e) => {
                eprintln!("skipping WHP dirty-log test: {e}");
                eprintln!("(to run these tests: {WHP_ENABLE_HINT})");
                return None;
            }
        };
        // WHP decides tracking when the range is mapped, so it is an option on
        // the partition rather than a switch on the log.
        let options = WhpOptions {
            track_dirty: true,
            ..WhpOptions::default()
        };
        let vm = WhpPartition::with_options(&hv, &CONFIG, options).expect("create a partition");
        let log = vm.dirty_log();
        assert!(log.dirty_logging(), "the partition is not tracking writes");
        Some(Machine { vm, log })
    }

    pub fn run(machine: &mut Machine) {
        let mut vcpus = machine.vm.take_vcpus();
        let mut vcpu = vcpus.remove(0);
        let mut sregs = vcpu.get_special_registers().expect("sregs");
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_special_registers(&sregs).expect("set sregs");
        let mut regs = vcpu.get_registers().expect("regs");
        regs.rip = CODE_ADDR;
        regs.rflags = 2;
        vcpu.set_registers(&regs).expect("set regs");

        let running = AtomicBool::new(true);
        let outcome = vcpu.run_loop(&mut Recorder, &running).expect("run");
        assert!(
            matches!(outcome, RunOutcome::Halted | RunOutcome::Shutdown),
            "the test guest did not terminate: {outcome:?}"
        );
    }
}

// --------------------------------------------------------------- the finding

/// **The finding ADR-0006 rests on: a host write is invisible.**
///
/// No guest at all. Tracking is armed, this process writes two pages of guest
/// RAM through the same checked API every device uses, and the log is empty.
/// Both hosts, for the same reason: they protect the guest's second-level page
/// tables, and the VMM writes the host allocation those tables point at.
///
/// Turn this test around and it is the specification of an incremental
/// snapshot's failure: the pages virtio-blk DMAs a filesystem block into are
/// exactly these pages.
#[test]
fn a_host_write_is_invisible_to_the_dirty_log() {
    let _guard = guard();
    let Some(machine) = host::machine_or_skip() else {
        return;
    };
    assert!(
        !machine.log.tracking().host_writes,
        "a host whose tracking claims to see host writes needs a different \
         answer to ADR-0006's incremental question"
    );

    // Two writes, in two pages, after tracking is armed — so nothing about the
    // ordering can explain the result away. One of them is the shape a device
    // completion has (a buffer the guest is waiting on); the other is a boot
    // image.
    let mem = machine.vm.memory();
    mem.write_slice(&[0xa5u8; 512], GuestAddress(CODE_ADDR))
        .expect("host write 1");
    mem.write_slice(&[0x5au8; 4096], GuestAddress(GUEST_WRITE_A))
        .expect("host write 2");
    assert_eq!(mem.read_obj::<u8>(GuestAddress(CODE_ADDR)).unwrap(), 0xa5);

    let log = machine.log.fetch_dirty().expect("read the dirty log");
    assert_eq!(
        total_dirty(&log),
        0,
        "the hypervisor reported {} dirty pages after nothing but host writes: {:?}",
        total_dirty(&log),
        log.iter().map(DirtyPages::runs).collect::<Vec<_>>()
    );
    eprintln!(
        "[dirty] two host writes into guest RAM, {} pages reported",
        total_dirty(&log)
    );
}

/// **A guest write is reported**, which is the other half: the mechanism works,
/// it just answers a different question.
///
/// The blob at [`CODE_ADDR`] is put there by this process; the bytes at
/// [`GUEST_WRITE_A`] and [`GUEST_WRITE_B`] by the guest's own `mov`
/// instructions.
///
/// The code page is **not** asserted either way, and that is a measurement
/// rather than a hedge: WHP leaves it clean, and KVM on this AMD host reports
/// it after the guest merely fetched instructions from it. Over-reporting is
/// the harmless direction, so the test records which host did what instead of
/// pinning one host's answer onto both.
#[test]
fn a_guest_write_is_reported() {
    let _guard = guard();
    let Some(mut machine) = host::machine_or_skip() else {
        return;
    };
    machine
        .vm
        .memory()
        .write_slice(&code(), GuestAddress(CODE_ADDR))
        .expect("load the test guest");

    host::run(&mut machine);

    let log = machine.log.fetch_dirty().expect("read the dirty log");
    assert_eq!(
        machine
            .vm
            .memory()
            .read_obj::<u8>(GuestAddress(GUEST_WRITE_A))
            .unwrap(),
        0x42,
        "the test guest never ran its first store"
    );
    assert!(
        dirty_at(&log, GUEST_WRITE_A) && dirty_at(&log, GUEST_WRITE_B),
        "the guest wrote {GUEST_WRITE_A:#x} and {GUEST_WRITE_B:#x} and the log has \
         {} dirty pages: {:?}",
        total_dirty(&log),
        log.iter().map(DirtyPages::runs).collect::<Vec<_>>()
    );
    eprintln!(
        "[dirty] {} pages after the guest wrote two; the code page {CODE_ADDR:#x} the \
         host wrote and the guest executed is {}",
        total_dirty(&log),
        if dirty_at(&log, CODE_ADDR) {
            "reported (this host over-reports execution)"
        } else {
            "absent"
        }
    );
}

/// Reading the log clears it, on both hosts — so a caller that reads twice sees
/// the second window, not the first plus the second.
///
/// [`vmm_core::hv::DirtyTracking::read_clears`] claims this; a claim about a
/// hypervisor's behaviour that nothing checks is a claim that quietly stops
/// being true.
#[test]
fn reading_the_log_clears_it() {
    let _guard = guard();
    let Some(mut machine) = host::machine_or_skip() else {
        return;
    };
    machine
        .vm
        .memory()
        .write_slice(&code(), GuestAddress(CODE_ADDR))
        .expect("load the test guest");
    host::run(&mut machine);

    let first = machine.log.fetch_dirty().expect("first read");
    assert!(total_dirty(&first) > 0, "the guest wrote nothing at all");
    let second = machine.log.fetch_dirty().expect("second read");

    assert_eq!(
        machine.log.tracking().read_clears,
        total_dirty(&second) < total_dirty(&first),
        "read_clears says {} but the second read reported {} of {} pages",
        machine.log.tracking().read_clears,
        total_dirty(&second),
        total_dirty(&first)
    );
    assert!(
        !dirty_at(&second, GUEST_WRITE_A),
        "the page the guest wrote is still dirty after the log was read"
    );
}

/// The log covers every RAM region and reports the whole guest, page by page.
#[test]
fn the_log_covers_every_region_of_guest_ram() {
    let _guard = guard();
    let Some(machine) = host::machine_or_skip() else {
        return;
    };
    let log = machine.log.fetch_dirty().expect("read the dirty log");
    let covered: u64 = log.iter().map(DirtyPages::len).sum();
    assert_eq!(
        covered,
        CONFIG.memory_mib << 20,
        "the log covers {covered} bytes of a {} byte guest",
        CONFIG.memory_mib << 20
    );
    for region in &log {
        assert_eq!(region.pages(), region.len() / DIRTY_PAGE_SIZE);
    }
}
