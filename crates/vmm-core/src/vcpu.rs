//! vCPU creation, the KVM_RUN loop and controlled stop
//! (backlog MVP-104/107/108, lifecycle checkpoints per ADR-0005).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use vmm_sys_util::signal::{register_signal_handler, Killable};

use crate::hv::{
    ExitHandler, HvError, MpState, RunOutcome, VcpuCensus, VcpuRegisters, X86DescriptorTable,
    X86Registers, X86Segment, X86SpecialRegisters,
};
use crate::lifecycle::{Checkpoint, Lifecycle, ResettableVcpu, VcpuKick};
use crate::VmmError;

/// RT signal used to kick vCPU threads out of KVM_RUN.
fn kick_signal() -> i32 {
    libc::SIGRTMIN()
}

/// The MSRs this kernel is prepared to save and restore, read once per process.
///
/// A property of the host, not of a VM or a vCPU, and the ioctl lives on the
/// `Kvm` handle — which a running vCPU no longer has. Cached here so an SMP
/// guest does not read the same list once per CPU, and so `Vcpu::new` keeps its
/// signature (ADR-0006).
///
/// A host that refuses the ioctl leaves the list empty, which makes a snapshot
/// carry no MSRs at all. That is worth an error line rather than silence: it is
/// the difference between a guest that resumes and one that resumes and then
/// dies in its next `syscall`.
fn host_msr_index_list(kvm: &Kvm) -> Arc<[u32]> {
    static LIST: OnceLock<Arc<[u32]>> = OnceLock::new();
    Arc::clone(LIST.get_or_init(|| match kvm.get_msr_index_list() {
        Ok(list) => {
            let indices: Arc<[u32]> = list.as_slice().into();
            tracing::debug!(
                count = indices.len(),
                "this kernel reports its saveable MSRs"
            );
            indices
        }
        Err(error) => {
            tracing::error!(
                %error,
                "KVM_GET_MSR_INDEX_LIST failed; a snapshot of this VM would carry no MSRs"
            );
            Arc::from(Vec::new())
        }
    }))
}

/// One virtual CPU. All KVM vCPU ioctls must come from the owning thread.
pub struct Vcpu {
    pub index: u32,
    fd: VcpuFd,
    /// Every MSR *this kernel* is prepared to save and restore
    /// (`KVM_GET_MSR_INDEX_LIST`), captured once at creation because the list
    /// is a property of the host and the ioctl is on the `Kvm` handle, which a
    /// running vCPU no longer has (ADR-0006).
    ///
    /// Shared rather than copied per vCPU: it is ~100 words and identical for
    /// every one of them.
    msr_index_list: Arc<[u32]>,
}

impl Vcpu {
    pub(crate) fn new(vm: &VmFd, kvm: &Kvm, index: u32) -> Result<Self, VmmError> {
        let fd = vm.create_vcpu(u64::from(index))?;
        let mut cpuid = kvm.get_supported_cpuid(256)?;
        for entry in cpuid.as_mut_slice() {
            match entry.function {
                // Leaf 1: ECX bit 31 tells the guest it runs under a
                // hypervisor; EBX[31:24] is the *initial APIC ID*, which
                // KVM_GET_SUPPORTED_CPUID leaves at the value of whichever
                // host CPU serviced the ioctl. Left alone, every vCPU claims
                // the host's APIC ID: guest code that compares the CPUID
                // identity against the local APIC's own ID then decides it is
                // running on an unknown processor. EDK2's `GetBspNumber()`
                // does exactly that and asserts (UEFI-1802); Linux is more
                // forgiving but no more correct.
                1 if entry.index == 0 => {
                    entry.ecx |= 1 << 31;
                    entry.ebx = (entry.ebx & 0x00ff_ffff) | (index << 24);
                }
                // Leaves 0xB (extended topology), 0x1F (V2 extended topology)
                // and 0x8000_0026 (AMD extended CPU topology) report the
                // 32-bit x2APIC ID in EDX, with the same problem and the same
                // fix.
                0xb | 0x1f | 0x8000_0026 => entry.edx = index,
                // Leaf 0x8000_001E EAX is AMD's *extended APIC id*, and on an
                // AMD host it is the value Linux ends up trusting: with
                // TOPOEXT present, `parse_8000_001e()` overwrites the
                // initial APIC id it took from leaf 1 with this one. Left at
                // KVM's default every vCPU claimed id 0, and a 2-vCPU guest
                // logged `[Firmware Bug]: CPU 1: APIC ID mismatch. CPUID:
                // 0x0000 APIC: 0x0001` — the same class of bug as leaf 1, on
                // the leaf that wins. EBX/ECX (core id, node id) are left
                // alone: this machine has no topology to describe beyond
                // "n independent CPUs".
                0x8000_001e => entry.eax = index,
                _ => {}
            }
        }
        fd.set_cpuid2(&cpuid)?;
        Ok(Self {
            index,
            fd,
            msr_index_list: host_msr_index_list(kvm),
        })
    }

    /// The host's MSR save/restore list, for the snapshot path.
    pub(crate) fn msr_index_list(&self) -> &[u32] {
        &self.msr_index_list
    }

    pub fn fd(&self) -> &VcpuFd {
        &self.fd
    }

    /// This processor's multiprocessing state, as the neutral [`MpState`].
    ///
    /// A vCPU ioctl, so only the owning thread may ask — which is why the
    /// census is filed by the run loop's own thread rather than collected by
    /// whoever joins it.
    pub(crate) fn mp_state(&self) -> Result<MpState, HvError> {
        self.fd
            .get_mp_state()
            .map(|state| crate::snapshot_kvm::mp_state_from_kvm(state.mp_state))
            .map_err(|e| HvError::Registers(format!("KVM_GET_MP_STATE: {e}")))
    }
}

// ---- hypervisor-neutral register access (WHP-1701, ADR-0002) --------------

fn seg_to_kvm(seg: &X86Segment) -> kvm_bindings::kvm_segment {
    kvm_bindings::kvm_segment {
        base: seg.base,
        limit: seg.limit,
        selector: seg.selector,
        type_: seg.type_,
        present: seg.present,
        dpl: seg.dpl,
        db: seg.db,
        s: seg.s,
        l: seg.l,
        g: seg.g,
        avl: seg.avl,
        unusable: seg.unusable,
        padding: 0,
    }
}

fn seg_from_kvm(seg: &kvm_bindings::kvm_segment) -> X86Segment {
    X86Segment {
        base: seg.base,
        limit: seg.limit,
        selector: seg.selector,
        type_: seg.type_,
        present: seg.present,
        dpl: seg.dpl,
        db: seg.db,
        s: seg.s,
        l: seg.l,
        g: seg.g,
        avl: seg.avl,
        unusable: seg.unusable,
    }
}

impl VcpuRegisters for Vcpu {
    fn get_registers(&self) -> Result<X86Registers, HvError> {
        let r = self
            .fd
            .get_regs()
            .map_err(|e| HvError::Registers(e.to_string()))?;
        Ok(X86Registers {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rsp: r.rsp,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rip: r.rip,
            rflags: r.rflags,
        })
    }

    fn set_registers(&self, regs: &X86Registers) -> Result<(), HvError> {
        let r = kvm_bindings::kvm_regs {
            rax: regs.rax,
            rbx: regs.rbx,
            rcx: regs.rcx,
            rdx: regs.rdx,
            rsi: regs.rsi,
            rdi: regs.rdi,
            rsp: regs.rsp,
            rbp: regs.rbp,
            r8: regs.r8,
            r9: regs.r9,
            r10: regs.r10,
            r11: regs.r11,
            r12: regs.r12,
            r13: regs.r13,
            r14: regs.r14,
            r15: regs.r15,
            rip: regs.rip,
            rflags: regs.rflags,
        };
        self.fd
            .set_regs(&r)
            .map_err(|e| HvError::Registers(e.to_string()))
    }

    fn get_special_registers(&self) -> Result<X86SpecialRegisters, HvError> {
        let s = self
            .fd
            .get_sregs()
            .map_err(|e| HvError::Registers(e.to_string()))?;
        Ok(X86SpecialRegisters {
            cs: seg_from_kvm(&s.cs),
            ds: seg_from_kvm(&s.ds),
            es: seg_from_kvm(&s.es),
            fs: seg_from_kvm(&s.fs),
            gs: seg_from_kvm(&s.gs),
            ss: seg_from_kvm(&s.ss),
            tr: seg_from_kvm(&s.tr),
            ldt: seg_from_kvm(&s.ldt),
            gdt: X86DescriptorTable {
                base: s.gdt.base,
                limit: s.gdt.limit,
            },
            idt: X86DescriptorTable {
                base: s.idt.base,
                limit: s.idt.limit,
            },
            cr0: s.cr0,
            cr2: s.cr2,
            cr3: s.cr3,
            cr4: s.cr4,
            cr8: s.cr8,
            efer: s.efer,
            apic_base: s.apic_base,
        })
    }

    fn set_special_registers(&self, sregs: &X86SpecialRegisters) -> Result<(), HvError> {
        // Read-modify-write: kvm_sregs carries the pending-interrupt bitmap,
        // which must survive untouched.
        let mut s = self
            .fd
            .get_sregs()
            .map_err(|e| HvError::Registers(e.to_string()))?;
        s.cs = seg_to_kvm(&sregs.cs);
        s.ds = seg_to_kvm(&sregs.ds);
        s.es = seg_to_kvm(&sregs.es);
        s.fs = seg_to_kvm(&sregs.fs);
        s.gs = seg_to_kvm(&sregs.gs);
        s.ss = seg_to_kvm(&sregs.ss);
        s.tr = seg_to_kvm(&sregs.tr);
        s.ldt = seg_to_kvm(&sregs.ldt);
        s.gdt.base = sregs.gdt.base;
        s.gdt.limit = sregs.gdt.limit;
        s.idt.base = sregs.idt.base;
        s.idt.limit = sregs.idt.limit;
        s.cr0 = sregs.cr0;
        s.cr2 = sregs.cr2;
        s.cr3 = sregs.cr3;
        s.cr4 = sregs.cr4;
        s.cr8 = sregs.cr8;
        s.efer = sregs.efer;
        s.apic_base = sregs.apic_base;
        self.fd
            .set_sregs(&s)
            .map_err(|e| HvError::Registers(e.to_string()))
    }
}

impl Vcpu {
    /// Runs the vCPU until the guest halts, shuts down, `running` turns
    /// false, or an unrecoverable error occurs. Every error carries the vCPU
    /// index and a readable message — a vCPU fault must never appear as an
    /// anonymous crash (EPIC 1 acceptance).
    pub fn run_loop(
        &mut self,
        handler: &mut dyn ExitHandler,
        running: &AtomicBool,
    ) -> Result<RunOutcome, VmmError> {
        self.run_loop_with(handler, running, None)
    }

    /// [`Self::run_loop`] with a [`Lifecycle`] attached (ADR-0005).
    ///
    /// The only difference is the checkpoint at the top of the loop and what a
    /// guest reset means: with a lifecycle that can restart the machine, a
    /// triple fault or a write to a reset register becomes a *reboot* instead
    /// of the end of the VM.
    pub fn run_loop_with(
        &mut self,
        handler: &mut dyn ExitHandler,
        running: &AtomicBool,
        lifecycle: Option<&Lifecycle>,
    ) -> Result<RunOutcome, VmmError> {
        let index = self.index;
        let fail = move |message: String| VmmError::Vcpu {
            index: index as usize,
            message,
        };
        while running.load(Ordering::Acquire) {
            if let Some(lifecycle) = lifecycle {
                if lifecycle.attention() {
                    // Retire KVM's pending userspace-I/O completion *before*
                    // parking: a reset overwrites the registers this completion
                    // would land in, and re-entering afterwards would apply a
                    // stale result to a freshly booted guest.
                    self.flush_pending_exit(handler)?;
                    if lifecycle.checkpoint(index, self) == Checkpoint::Stopped {
                        return Ok(RunOutcome::Stopped);
                    }
                }
            }
            match self.fd.run() {
                Ok(VcpuExit::Hlt) => return Ok(RunOutcome::Halted),
                Ok(VcpuExit::Shutdown) => {
                    // A triple fault. Which of the three things it means is
                    // decided by what the machine latched and by *which* CPU
                    // faulted:
                    //
                    // * an ACPI S5 write outstanding — the guest is powering off,
                    //   and this is how it gets there;
                    // * the boot CPU with nothing latched — the last rung of
                    //   Linux's reboot ladder (`reboot=t`, or everything else
                    //   having been tried), so: reboot;
                    // * an application processor — *not* a reason to restart the
                    //   machine. EDK2's `MpInitLib` wakes each AP with INIT/SIPI
                    //   and carries on with however many answer ("Find 1
                    //   processors in system"), so a machine whose AP faults
                    //   during startup is one the firmware expects to keep
                    //   running. Rebooting on it would turn a rare race into a
                    //   reboot loop; ending the VM — which is what this backend
                    //   used to do — throws away a boot that was going to
                    //   succeed.
                    //
                    // In every case the vCPU must not be re-entered: KVM answers
                    // the next `KVM_RUN` on a shut-down vCPU with
                    // `KVM_EXIT_INTERNAL_ERROR`.
                    if handler.shutdown_requested() {
                        return Ok(RunOutcome::Shutdown);
                    }
                    let site = crate::hv::fault_site(self);
                    let can_reset = lifecycle.is_some_and(Lifecycle::can_reset);
                    match (index, can_reset) {
                        (_, false) => {
                            tracing::warn!(
                                vcpu = index,
                                %site,
                                "triple fault with no power-off latched and no lifecycle to                                  restart the machine: ending the VM"
                            );
                            return Ok(RunOutcome::Shutdown);
                        }
                        (0, true) => {
                            tracing::info!(
                                vcpu = index,
                                %site,
                                "triple fault on the boot CPU with no power-off latched:                                  treating it as a reboot"
                            );
                            let Some(lifecycle) = lifecycle else {
                                return Ok(RunOutcome::Shutdown);
                            };
                            if !lifecycle.request_guest_reset() {
                                return Ok(RunOutcome::Shutdown);
                            }
                            match park_for_reset(index, self, lifecycle, running, handler)? {
                                Parked::Reset => continue,
                                Parked::Stop => return Ok(RunOutcome::Stopped),
                                Parked::GaveUp => return Ok(RunOutcome::Shutdown),
                            }
                        }
                        (_, true) => {
                            tracing::warn!(
                                vcpu = index,
                                %site,
                                "triple fault on an application processor: parking it and                                  leaving the machine running"
                            );
                            let Some(lifecycle) = lifecycle else {
                                return Ok(RunOutcome::Shutdown);
                            };
                            // No deadline: the AP waits for a machine reset for
                            // as long as the VM lives, and the VM is not waiting
                            // for the AP.
                            match park_indefinitely(index, self, lifecycle, running, handler)? {
                                Parked::Reset => continue,
                                _ => return Ok(RunOutcome::Stopped),
                            }
                        }
                    }
                }
                Ok(VcpuExit::IoOut(port, data)) => handler.io_out(port, data),
                Ok(VcpuExit::IoIn(port, data)) => handler.io_in(port, data),
                Ok(VcpuExit::MmioWrite(addr, data)) => handler.mmio_write(addr, data),
                Ok(VcpuExit::MmioRead(addr, data)) => handler.mmio_read(addr, data),
                Ok(VcpuExit::FailEntry(reason, cpu)) => {
                    return Err(fail(format!(
                        "KVM_EXIT_FAIL_ENTRY: hardware entry failure reason {reason:#x} on cpu {cpu}"
                    )));
                }
                Ok(VcpuExit::InternalError) => {
                    return Err(fail("KVM_EXIT_INTERNAL_ERROR".into()));
                }
                Ok(exit) => return Err(fail(format!("unhandled VM exit: {exit:?}"))),
                // Kicked by the stop signal (or spurious wakeup): re-check
                // the running flag and continue.
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => continue,
                Err(e) => return Err(fail(format!("KVM_RUN failed: {e}"))),
            }
            // A device may have latched a power-off request while handling that
            // exit (the ACPI PM block does, on an S5 write). The guest is
            // spinning in `CpuDeadLoop()`/`hlt` by now and will never exit
            // again on its own, so this is the only place the request can be
            // noticed.
            if handler.shutdown_requested() {
                return Ok(RunOutcome::Shutdown);
            }
            // The same shape for a *reset* request (0xCF9, the keyboard
            // controller pulse, the ACPI reset register): latch it and let the
            // supervisor drive the restart, which parks this vCPU at the
            // checkpoint above.
            if handler.reset_requested() {
                match lifecycle {
                    Some(lifecycle) if lifecycle.can_reset() => {
                        if !lifecycle.request_guest_reset() {
                            return Ok(RunOutcome::Shutdown);
                        }
                        // The guest is in its own dead loop by now, waiting for
                        // a reset that will never come from inside. Waiting for
                        // the supervisor beats spinning through it.
                        match park_for_reset(index, self, lifecycle, running, handler)? {
                            Parked::Reset => continue,
                            Parked::Stop => return Ok(RunOutcome::Stopped),
                            Parked::GaveUp => return Ok(RunOutcome::Shutdown),
                        }
                    }
                    // No lifecycle to restart the machine: honour the request as
                    // an ending rather than letting the guest spin forever in
                    // the dead loop it entered after asking.
                    _ => return Ok(RunOutcome::Shutdown),
                }
            }
        }
        Ok(RunOutcome::Stopped)
    }

    /// Completes whatever KVM is still holding from the last exit, so the vCPU
    /// can be parked (and its registers rewritten) without a stale result
    /// landing on the other side.
    ///
    /// KVM keeps the completion of a userspace I/O or MMIO access in the
    /// `kvm_run` mapping between the exit and the next `KVM_RUN`, and applies it
    /// at the top of that call — *before* it honours `immediate_exit`. So one
    /// run with `immediate_exit` set retires it and comes straight back. A
    /// multi-fragment MMIO access can produce one more exit while doing so,
    /// which is why this dispatches rather than just discarding, and why the
    /// loop is bounded: the guest is untrusted and must not be able to keep a
    /// pause waiting.
    fn flush_pending_exit(&mut self, handler: &mut dyn ExitHandler) -> Result<(), VmmError> {
        const MAX_FRAGMENTS: usize = 8;
        let index = self.index;
        let fail = move |message: String| VmmError::Vcpu {
            index: index as usize,
            message,
        };
        self.fd.set_kvm_immediate_exit(1);
        let mut result = Ok(());
        for _ in 0..MAX_FRAGMENTS {
            match self.fd.run() {
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => break,
                Ok(VcpuExit::IoOut(port, data)) => handler.io_out(port, data),
                Ok(VcpuExit::IoIn(port, data)) => handler.io_in(port, data),
                Ok(VcpuExit::MmioWrite(addr, data)) => handler.mmio_write(addr, data),
                Ok(VcpuExit::MmioRead(addr, data)) => handler.mmio_read(addr, data),
                // Anything else (a halt, a shutdown) is not a pending
                // completion; leave it for the run loop to see again.
                Ok(_) => break,
                Err(e) => {
                    result = Err(fail(format!("KVM_RUN failed while quiescing: {e}")));
                    break;
                }
            }
        }
        self.fd.set_kvm_immediate_exit(0);
        result
    }
}

// ---- parking a vCPU that must not re-enter the guest (ADR-0005) -----------

/// How a park ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parked {
    /// The machine was reset; this vCPU is at its boot state and may run.
    Reset,
    /// The VM is stopping.
    Stop,
    /// Nobody served the reset within the deadline.
    GaveUp,
}

/// How long a vCPU waits for the supervisor to serve a reset before giving up
/// and ending the VM.
///
/// Long enough that a busy host is not mistaken for a missing supervisor, short
/// enough that a VM whose supervisor died does not hang for ever. The guest is
/// untrusted: a reset it asked for must not be able to wedge the host.
const RESET_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Waits for the machine reset this vCPU has just asked for, bounded by
/// [`RESET_WAIT`].
fn park_for_reset(
    index: u32,
    vcpu: &mut Vcpu,
    lifecycle: &Lifecycle,
    running: &AtomicBool,
    handler: &mut dyn ExitHandler,
) -> Result<Parked, VmmError> {
    park(index, vcpu, lifecycle, running, handler, Some(RESET_WAIT))
}

/// The same, without a deadline: for an application processor that faulted and
/// is simply waiting for the machine to be reset around it, which may be never.
fn park_indefinitely(
    index: u32,
    vcpu: &mut Vcpu,
    lifecycle: &Lifecycle,
    running: &AtomicBool,
    handler: &mut dyn ExitHandler,
) -> Result<Parked, VmmError> {
    park(index, vcpu, lifecycle, running, handler, None)
}

fn park(
    index: u32,
    vcpu: &mut Vcpu,
    lifecycle: &Lifecycle,
    running: &AtomicBool,
    handler: &mut dyn ExitHandler,
    deadline: Option<std::time::Duration>,
) -> Result<Parked, VmmError> {
    let started = std::time::Instant::now();
    loop {
        if !running.load(Ordering::Acquire) {
            return Ok(Parked::Stop);
        }
        if lifecycle.attention() {
            vcpu.flush_pending_exit(handler)?;
            return Ok(match lifecycle.checkpoint(index, vcpu) {
                Checkpoint::Continue => Parked::Reset,
                Checkpoint::Stopped => Parked::Stop,
            });
        }
        if deadline.is_some_and(|limit| started.elapsed() >= limit) {
            tracing::error!(
                vcpu = index,
                waited = ?started.elapsed(),
                "no supervisor served the guest's reset request; ending the VM"
            );
            return Ok(Parked::GaveUp);
        }
        lifecycle.wait_for_attention(std::time::Duration::from_millis(20));
    }
}

// ---- architectural reset (ADR-0005) ---------------------------------------

/// The local APIC's power-on register page (SDM vol. 3, "Local APIC State
/// After Power-Up or Reset"), as KVM's `kvm_lapic_state` wants it.
///
/// Written wholesale rather than patched, because the interesting fields are
/// exactly the ones a running kernel changed: the LVT entries it pointed at its
/// own vectors, the spurious-interrupt vector register it enabled, and any
/// IRR/ISR bit left in flight. Re-entering a fresh boot with an armed APIC
/// timer is a triple fault a few hundred instructions later, before the new
/// kernel has an IDT.
fn reset_lapic_state(apic_id: u32) -> kvm_bindings::kvm_lapic_state {
    /// The register page is 1 KiB of 16-byte-spaced 32-bit registers.
    fn put(state: &mut kvm_bindings::kvm_lapic_state, offset: usize, value: u32) {
        for (i, byte) in value.to_le_bytes().iter().enumerate() {
            // `regs` is `[c_char; 1024]`; every offset used here is a named
            // architectural register well inside it.
            if let Some(slot) = state.regs.get_mut(offset + i) {
                *slot = *byte as std::os::raw::c_char;
            }
        }
    }
    // Reset values from the SDM: version 0x14 with 5 LVT entries, DFR all ones
    // (flat model), SVR with the APIC software-disabled and vector 0xff, every
    // LVT masked. Everything not named here is zero, which is the reset value.
    const LAPIC_ID: usize = 0x20;
    const LAPIC_VERSION: usize = 0x30;
    const LAPIC_DFR: usize = 0xe0;
    const LAPIC_SVR: usize = 0xf0;
    const LVT_FIRST: usize = 0x320;
    const LVT_LAST: usize = 0x370;
    const LVT_MASKED: u32 = 1 << 16;

    let mut state = kvm_bindings::kvm_lapic_state::default();
    put(&mut state, LAPIC_ID, apic_id << 24);
    put(&mut state, LAPIC_VERSION, 0x0005_0014);
    put(&mut state, LAPIC_DFR, 0xffff_ffff);
    put(&mut state, LAPIC_SVR, 0x0000_00ff);
    let mut lvt = LVT_FIRST;
    while lvt <= LVT_LAST {
        put(&mut state, lvt, LVT_MASKED);
        lvt += 0x10;
    }
    state
}

/// The x86 power-on segment/control-register state, hypervisor-neutral.
///
/// `apic_base` is forced back to the architectural default *in xAPIC mode*: a
/// guest that had switched the local APIC's base, or enabled x2APIC, must not
/// hand that on to the next boot, which expects to find the APIC where the
/// firmware left it.
pub(crate) fn architectural_reset_sregs(is_boot_cpu: bool) -> X86SpecialRegisters {
    /// Present, system=1 (code/data), accessed. Type 11 = code, execute/read.
    fn code_segment(base: u64, selector: u16) -> X86Segment {
        X86Segment {
            base,
            limit: 0xffff,
            selector,
            type_: 0b1011,
            present: 1,
            s: 1,
            ..X86Segment::default()
        }
    }
    /// Type 3 = data, read/write, accessed.
    fn data_segment() -> X86Segment {
        X86Segment {
            base: 0,
            limit: 0xffff,
            selector: 0,
            type_: 0b0011,
            present: 1,
            s: 1,
            ..X86Segment::default()
        }
    }
    /// A system descriptor (`s = 0`): the TSS and the LDT at reset.
    fn system_segment(type_: u8) -> X86Segment {
        X86Segment {
            base: 0,
            limit: 0xffff,
            selector: 0,
            type_,
            present: 1,
            s: 0,
            ..X86Segment::default()
        }
    }

    /// `CR0` after reset: CD | NW | ET. No PE, no PG — real mode.
    const CR0_RESET: u64 = 0x6000_0010;
    /// `IA32_APIC_BASE`: the architectural window, enabled, xAPIC mode.
    const APIC_BASE_ENABLE: u64 = 1 << 11;
    const APIC_BASE_BSP: u64 = 1 << 8;
    const APIC_DEFAULT_BASE: u64 = 0xfee0_0000;

    let data = data_segment();
    X86SpecialRegisters {
        // The reset vector: CS.base 0xffff_0000 with IP 0xfff0 puts the first
        // fetch at 0xffff_fff0, which is where a firmware ROM lives.
        cs: code_segment(0xffff_0000, 0xf000),
        ds: data,
        es: data,
        fs: data,
        gs: data,
        ss: data,
        // Type 11 = 32-bit busy TSS, type 2 = LDT.
        tr: system_segment(0b1011),
        ldt: system_segment(0b0010),
        gdt: X86DescriptorTable {
            base: 0,
            limit: 0xffff,
        },
        idt: X86DescriptorTable {
            base: 0,
            limit: 0xffff,
        },
        cr0: CR0_RESET,
        cr2: 0,
        cr3: 0,
        cr4: 0,
        cr8: 0,
        efer: 0,
        apic_base: APIC_DEFAULT_BASE
            | APIC_BASE_ENABLE
            | if is_boot_cpu { APIC_BASE_BSP } else { 0 },
    }
}

/// The general-purpose registers after reset: everything zero except the
/// reserved `RFLAGS` bit, `RDX` (family/model/stepping, as every x86 leaves it)
/// and `RIP` at the reset vector offset.
pub(crate) fn architectural_reset_regs() -> X86Registers {
    X86Registers {
        rdx: 0x600,
        rflags: 2,
        rip: 0xfff0,
        ..X86Registers::default()
    }
}

impl ResettableVcpu for Vcpu {
    /// KVM has no "reset this vCPU" ioctl, so the state is written out by hand.
    ///
    /// Three things matter, in this order:
    ///
    /// 1. **Pending events go first.** An injected interrupt or a pending
    ///    exception left over from the guest that just died would be delivered
    ///    into the new boot's first instructions.
    /// 2. **The local APIC**, written as a whole power-on register page rather
    ///    than patched. The interesting fields are exactly the ones a running
    ///    kernel changed: the LVT entries it pointed at its own vectors, the
    ///    spurious-interrupt register it enabled, and any IRR/ISR bit left in
    ///    flight. Re-entering a fresh boot with an armed APIC timer is a triple
    ///    fault a few hundred instructions later, before the new kernel has an
    ///    IDT.
    /// 3. **Registers, then `mp_state`.** An application processor goes back to
    ///    `KVM_MP_STATE_UNINITIALIZED` — exactly where `KVM_CREATE_VCPU` left it
    ///    — so the new kernel's INIT/SIPI sweep brings it up the same way the
    ///    first boot did, and KVM performs the real INIT reset itself.
    fn reset_arch_state(&mut self, is_boot_cpu: bool) -> Result<(), HvError> {
        let err = |what: &str, e: kvm_ioctls::Error| {
            HvError::Registers(format!("vCPU {}: {what} failed: {e}", self.index))
        };

        let mut events = self
            .fd
            .get_vcpu_events()
            .map_err(|e| err("KVM_GET_VCPU_EVENTS", e))?;
        events.exception.injected = 0;
        events.exception.pending = 0;
        events.exception.has_error_code = 0;
        events.exception.error_code = 0;
        events.interrupt.injected = 0;
        events.interrupt.shadow = 0;
        events.nmi.injected = 0;
        events.nmi.pending = 0;
        events.nmi.masked = 0;
        events.sipi_vector = 0;
        self.fd
            .set_vcpu_events(&events)
            .map_err(|e| err("KVM_SET_VCPU_EVENTS", e))?;

        self.fd
            .set_lapic(&reset_lapic_state(self.index))
            .map_err(|e| err("KVM_SET_LAPIC", e))?;

        self.set_special_registers(&architectural_reset_sregs(is_boot_cpu))?;
        self.set_registers(&architectural_reset_regs())?;

        let mp_state = kvm_bindings::kvm_mp_state {
            mp_state: if is_boot_cpu {
                kvm_bindings::KVM_MP_STATE_RUNNABLE
            } else {
                kvm_bindings::KVM_MP_STATE_UNINITIALIZED
            },
        };
        self.fd
            .set_mp_state(mp_state)
            .map_err(|e| err("KVM_SET_MP_STATE", e))?;
        Ok(())
    }

    /// See `crate::snapshot_kvm`, which owns the MSR list, the ordering rules
    /// and the blob formats.
    fn save_cpu_state(&self) -> Result<crate::hv::X86CpuState, HvError> {
        self.snapshot()
    }

    fn load_cpu_state(&mut self, state: &crate::hv::X86CpuState) -> Result<(), HvError> {
        self.restore(state)
    }
}

/// Running vCPU threads with a shared stop flag (MVP-108).
pub struct VcpuThreads {
    running: Arc<AtomicBool>,
    handles: Vec<JoinHandle<Result<RunOutcome, VmmError>>>,
    /// Released on stop, so a *paused* VM can still be torn down (ADR-0005).
    lifecycle: Option<Arc<Lifecycle>>,
    /// Which processors the guest actually brought up; reported once the run
    /// is over, because a guest running on fewer CPUs than it was given says
    /// nothing about it itself.
    census: Arc<VcpuCensus>,
}

/// Kicks one vCPU thread out of `KVM_RUN` with the RT signal, from any thread.
///
/// The thread publishes its own `pthread_t` on entry rather than the spawner
/// handing one out: `vmm_sys_util`'s `Killable` lives on the `JoinHandle`, which
/// belongs to [`VcpuThreads`], and the lifecycle needs a kick handle that
/// outlives any borrow of it.
struct SignalKicker {
    thread: Arc<AtomicU64>,
}

impl VcpuKick for SignalKicker {
    fn kick(&self) {
        let raw = self.thread.load(Ordering::Acquire);
        if raw == 0 {
            // The thread has not published itself yet; the requester re-kicks.
            return;
        }
        // SAFETY: `raw` is a `pthread_t` this process created, published by the
        // thread itself and never reused (the value is only ever written once,
        // before the thread enters its run loop). `pthread_kill` on a thread
        // that has since exited is the one race here, and glibc handles it by
        // returning ESRCH rather than faulting — which is why the result is
        // discarded rather than reported.
        unsafe {
            libc::pthread_kill(raw as libc::pthread_t, kick_signal());
        }
    }
}

/// Spawns one thread per vCPU. `make_handler` builds the exit handler for
/// each vCPU index (usually a clone of the device bus).
pub fn spawn_vcpus(
    vcpus: Vec<Vcpu>,
    make_handler: impl FnMut(u32) -> Box<dyn ExitHandler>,
) -> Result<VcpuThreads, VmmError> {
    spawn_vcpus_with(vcpus, make_handler, None)
}

/// [`spawn_vcpus`] with a [`Lifecycle`] attached (ADR-0005): the run loops gain
/// a checkpoint, the lifecycle gains a kick handle per vCPU, and stopping the
/// VM releases anything parked at the checkpoint.
pub fn spawn_vcpus_with(
    vcpus: Vec<Vcpu>,
    mut make_handler: impl FnMut(u32) -> Box<dyn ExitHandler>,
    lifecycle: Option<Arc<Lifecycle>>,
) -> Result<VcpuThreads, VmmError> {
    ensure_kick_signal_handler()?;
    let running = Arc::new(AtomicBool::new(true));
    let census = Arc::new(VcpuCensus::new(vcpus.len() as u32));
    let mut handles = Vec::with_capacity(vcpus.len());
    for mut vcpu in vcpus {
        let mut handler = make_handler(vcpu.index);
        let flag = Arc::clone(&running);
        let lifecycle = lifecycle.clone();
        let census = Arc::clone(&census);
        let published = Arc::new(AtomicU64::new(0));
        if let Some(lifecycle) = &lifecycle {
            lifecycle.register_kicker(Arc::new(SignalKicker {
                thread: Arc::clone(&published),
            }));
        }
        let handle = std::thread::Builder::new()
            .name(format!("vcpu{}", vcpu.index))
            .spawn(move || {
                // SAFETY: `pthread_self` takes no arguments and cannot fail.
                published.store(unsafe { libc::pthread_self() } as u64, Ordering::Release);
                let index = vcpu.index;
                let outcome = vcpu.run_loop_with(handler.as_mut(), &flag, lifecycle.as_deref());
                census.file(index, vcpu.mp_state());
                if let Some(lifecycle) = &lifecycle {
                    lifecycle.vcpu_finished(index);
                }
                outcome
            })
            .map_err(|e| VmmError::Vcpu {
                index: 0,
                message: format!("failed to spawn vCPU thread: {e}"),
            })?;
        handles.push(handle);
    }
    Ok(VcpuThreads {
        running,
        handles,
        lifecycle,
        census,
    })
}

impl VcpuThreads {
    /// Requests all vCPUs to stop, kicks them out of KVM_RUN and joins the
    /// threads. Kicks repeatedly to close the race between the flag check
    /// and (re-)entering KVM_RUN.
    pub fn stop(self) -> Vec<Result<RunOutcome, VmmError>> {
        self.running.store(false, Ordering::Release);
        if let Some(lifecycle) = &self.lifecycle {
            // A vCPU parked at a lifecycle checkpoint is not inside KVM_RUN and
            // no amount of kicking would move it; releasing the hold is what
            // lets a paused VM be shut down.
            lifecycle.shutdown();
        }
        let census = Arc::clone(&self.census);
        let outcomes: Vec<_> = self
            .handles
            .into_iter()
            .map(|handle| {
                while !handle.is_finished() {
                    let _ = handle.kill(kick_signal());
                    std::thread::yield_now();
                }
                join_outcome(handle)
            })
            .collect();
        census.report();
        outcomes
    }

    /// Waits for the guests to end on their own (halt/shutdown).
    pub fn join(self) -> Vec<Result<RunOutcome, VmmError>> {
        let census = Arc::clone(&self.census);
        let outcomes: Vec<_> = self.handles.into_iter().map(join_outcome).collect();
        census.report();
        outcomes
    }

    /// Waits until every vCPU ends on its own **or** `should_stop` returns
    /// true (e.g. SIGINT was received, or the window was closed), polling at
    /// `poll` intervals. Either way all threads are joined before returning
    /// (backlog MVP-1204/1208 groundwork).
    pub fn join_or_stop(
        self,
        should_stop: impl Fn() -> bool,
        poll: std::time::Duration,
    ) -> Vec<Result<RunOutcome, VmmError>> {
        loop {
            if self.handles.iter().all(|h| h.is_finished()) {
                return self.join();
            }
            // One finished vCPU means the VM is over: no run loop returns while
            // its guest is healthy, and a multi-CPU guest that triple-faults on
            // the BSP leaves its APs parked inside the hypervisor forever —
            // waiting for *all* of them would hang the supervisor on a machine
            // that is already dead (measured with `reboot=k` on 2 vCPUs).
            if should_stop() || self.handles.iter().any(|h| h.is_finished()) {
                return self.stop();
            }
            std::thread::sleep(poll);
        }
    }
}

fn join_outcome(handle: JoinHandle<Result<RunOutcome, VmmError>>) -> Result<RunOutcome, VmmError> {
    handle.join().unwrap_or_else(|_| {
        Err(VmmError::Vcpu {
            index: 0,
            message: "vCPU thread panicked".into(),
        })
    })
}

extern "C" fn kick_noop(
    _signum: libc::c_int,
    _info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
}

/// Registers the (no-op) handler for the kick signal exactly once. Delivery
/// of the signal interrupts KVM_RUN with EINTR, which the run loop treats as
/// "re-check the stop flag".
fn ensure_kick_signal_handler() -> Result<(), VmmError> {
    static INIT: OnceLock<Result<(), i32>> = OnceLock::new();
    INIT.get_or_init(|| register_signal_handler(kick_signal(), kick_noop).map_err(|e| e.errno()))
        .map_err(|code| VmmError::Vcpu {
            index: 0,
            message: format!("failed to register vCPU kick signal handler (errno {code})"),
        })
}
