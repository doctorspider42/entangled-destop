//! vCPU creation, the KVM_RUN loop and controlled stop
//! (backlog MVP-104/107/108).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use vmm_sys_util::signal::{register_signal_handler, Killable};

use crate::hv::{
    HvError, VcpuRegisters, X86DescriptorTable, X86Registers, X86Segment, X86SpecialRegisters,
};
use crate::VmmError;

/// RT signal used to kick vCPU threads out of KVM_RUN.
fn kick_signal() -> i32 {
    libc::SIGRTMIN()
}

/// How a vCPU's run loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// KVM_EXIT_HLT reached userspace. Note: with the in-kernel irqchip the
    /// kernel emulates HLT internally (the vCPU blocks waiting for an
    /// interrupt), so this exit only surfaces on machines without it. Test
    /// guests signal completion via triple fault ([`RunOutcome::Shutdown`])
    /// instead.
    Halted,
    /// Guest requested shutdown (triple fault / KVM_EXIT_SHUTDOWN).
    Shutdown,
    /// The host asked the loop to stop.
    Stopped,
}

/// Where VM exits are dispatched. The device bus implements this; tests use
/// small recording handlers.
pub trait ExitHandler: Send {
    fn io_out(&mut self, port: u16, data: &[u8]);
    fn io_in(&mut self, port: u16, data: &mut [u8]);
    fn mmio_write(&mut self, addr: u64, data: &[u8]);
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]);
}

/// One virtual CPU. All KVM vCPU ioctls must come from the owning thread.
pub struct Vcpu {
    pub index: u32,
    fd: VcpuFd,
}

impl Vcpu {
    pub(crate) fn new(vm: &VmFd, kvm: &Kvm, index: u32) -> Result<Self, VmmError> {
        let fd = vm.create_vcpu(u64::from(index))?;
        let mut cpuid = kvm.get_supported_cpuid(256)?;
        for entry in cpuid.as_mut_slice() {
            // Leaf 1 ECX bit 31: tell the guest it runs under a hypervisor.
            if entry.function == 1 && entry.index == 0 {
                entry.ecx |= 1 << 31;
            }
        }
        fd.set_cpuid2(&cpuid)?;
        Ok(Self { index, fd })
    }

    pub fn fd(&self) -> &VcpuFd {
        &self.fd
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
        let fail = |message: String| VmmError::Vcpu {
            index: self.index as usize,
            message,
        };
        while running.load(Ordering::Acquire) {
            match self.fd.run() {
                Ok(VcpuExit::Hlt) => return Ok(RunOutcome::Halted),
                Ok(VcpuExit::Shutdown) => return Ok(RunOutcome::Shutdown),
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
        }
        Ok(RunOutcome::Stopped)
    }
}

/// Running vCPU threads with a shared stop flag (MVP-108).
pub struct VcpuThreads {
    running: Arc<AtomicBool>,
    handles: Vec<JoinHandle<Result<RunOutcome, VmmError>>>,
}

/// Spawns one thread per vCPU. `make_handler` builds the exit handler for
/// each vCPU index (usually a clone of the device bus).
pub fn spawn_vcpus(
    vcpus: Vec<Vcpu>,
    mut make_handler: impl FnMut(u32) -> Box<dyn ExitHandler>,
) -> Result<VcpuThreads, VmmError> {
    ensure_kick_signal_handler()?;
    let running = Arc::new(AtomicBool::new(true));
    let mut handles = Vec::with_capacity(vcpus.len());
    for mut vcpu in vcpus {
        let mut handler = make_handler(vcpu.index);
        let flag = Arc::clone(&running);
        let handle = std::thread::Builder::new()
            .name(format!("vcpu{}", vcpu.index))
            .spawn(move || vcpu.run_loop(handler.as_mut(), &flag))
            .map_err(|e| VmmError::Vcpu {
                index: 0,
                message: format!("failed to spawn vCPU thread: {e}"),
            })?;
        handles.push(handle);
    }
    Ok(VcpuThreads { running, handles })
}

impl VcpuThreads {
    /// Requests all vCPUs to stop, kicks them out of KVM_RUN and joins the
    /// threads. Kicks repeatedly to close the race between the flag check
    /// and (re-)entering KVM_RUN.
    pub fn stop(self) -> Vec<Result<RunOutcome, VmmError>> {
        self.running.store(false, Ordering::Release);
        self.handles
            .into_iter()
            .map(|handle| {
                while !handle.is_finished() {
                    let _ = handle.kill(kick_signal());
                    std::thread::yield_now();
                }
                join_outcome(handle)
            })
            .collect()
    }

    /// Waits for the guests to end on their own (halt/shutdown).
    pub fn join(self) -> Vec<Result<RunOutcome, VmmError>> {
        self.handles.into_iter().map(join_outcome).collect()
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
            if should_stop() {
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
