---
name: kvm-machine
description: Implementing the KVM machine layer of Entangled Desktop — VM/vCPU creation, memory registration, register/CPUID setup, the KVM_RUN loop and VM exits (backlog EPIC 1, crates vmm-core and machine-x86). Load before any work touching kvm-ioctls/kvm-bindings.
---

# KVM machine layer

Scope: backlog EPIC 1 (MVP-101…110). Code lives in `crates/vmm-core` (hypervisor
handle, guest memory, vCPU threads, lifecycle) and `crates/machine-x86`
(x86-specific layout, E820, CPUID, GDT, IRQ chip policy).

## Existing pieces — build on these, don't duplicate

- `vmm_core::Hypervisor` opens `/dev/kvm`, validates API version 12 and the
  required capability set (`REQUIRED_CAPS` in `hypervisor.rs`). Add new
  required caps there, not ad hoc at call sites.
- `vmm_core::VmState` is the lifecycle state machine; vCPU threads report
  through it. Any abnormal exit → `Crashed` with a readable report, never a
  bare panic (acceptance criterion MVP-107/108).
- `machine_x86::layout` holds all guest physical addresses (zero page,
  cmdline, high RAM, MMIO hole, virtio-mmio slots). Never hardcode addresses
  elsewhere.
- `machine_x86::e820_map()` builds the memory map; extend it when high RAM
  (>3 GiB) support lands.

## Implementation order that works

1. VM creation: `kvm.create_vm()`, then `KVM_SET_USER_MEMORY_REGION` per RAM
   region from `e820_map` (only Ram entries), backed by `vm-memory`'s
   `GuestMemoryMmap`.
2. In-kernel IRQ chip **before** vCPU creation: `create_irq_chip()`, then
   `create_pit2(kvm_pit_config { flags: KVM_PIT_SPEAKER_DUMMY, .. })`.
3. vCPU: `create_vcpu(i)`; set CPUID from `kvm.get_supported_cpuid()` filtered
   (set hypervisor bit, cap leaf ranges); sregs for 64-bit boot per the Linux
   boot protocol (GDT with flat 4G code/data descriptors, CR0.PE|PG, CR4.PAE,
   EFER.LME|LMA, identity-mapped page tables in low memory); regs with
   `rip = kernel entry`, `rsi = zero page address`, rflags = 2.
4. `KVM_RUN` loop per vCPU thread. Match exits explicitly:
   `Io`, `MmioRead`/`MmioWrite`, `Hlt`, `Shutdown`, `FailEntry`,
   `InternalError`. Unknown exit → log + `Crashed`, never `unreachable!`.
5. Stop path (MVP-108): set a shared `AtomicBool`, kick vCPUs with
   `vcpu.set_kvm_immediate_exit(1)` + signal (`vmm-sys-util`'s `Killable`),
   join all threads, transition state. 100 create/destroy cycles must not
   leak (test exists as acceptance criterion — keep it green).

## Safety rules

- Every `unsafe` block needs a `// SAFETY:` comment (`undocumented_unsafe_blocks`
  is deny at workspace level).
- Guest memory access only through `vm-memory` checked APIs
  (`read_obj`/`write_obj`/`get_slice`) — no raw pointer arithmetic on guest RAM.
- KVM ioctls only from the thread that owns the vCPU fd (KVM requirement);
  keep vCPU fds thread-confined by construction.

## Testing

- Unit tests that need `/dev/kvm` must self-skip when it is absent (pattern in
  `hypervisor.rs` tests) so CI without nested virt stays green.
- The EPIC 1 smoke test (MVP-109): a hand-written real-mode blob that writes a
  known value to an I/O port, asserted from the exit handler. Put it in
  `crates/vmm-core/tests/`; the blob bytes belong in the test file with the
  disassembly as a comment.
- Local full-KVM runs: WSL Ubuntu has `/dev/kvm`
  (`wsl -d Ubuntu -e bash -lc "cd /mnt/d/entangled-desktop && cargo test -p vmm-core"`).

## Pitfalls

- `KVM_SET_USER_MEMORY_REGION` slots must not overlap; slot indices are
  per-VM and never reused after deletion in our design — allocate montonically.
- Forgetting `KVM_CAP_IMMEDIATE_EXIT` handling makes clean shutdown racy;
  it is already in `REQUIRED_CAPS`.
- The PIT/IRQ chip must exist before vCPUs, or `KVM_CREATE_PIT2` fails.
- Do not use `KVM_GET_SUPPORTED_CPUID` results unfiltered — mask out features
  we cannot honor (x2apic is fine to keep; PMU, nested virt leaves are not).

## Host memory that is not guest RAM (EPIC 20, VEN-2001)

`Vm::create_shm_window(len)` allocates a virtio shared-memory window and
reserves the memory slot it will live in. Notes that cost time if you rediscover
them:

* **Slot numbers are a counter now** (`Vm::next_slot`), shared by guest RAM,
  firmware ROMs and windows. They used to be derived twice by two formulas, and
  a ROM mapped after a window would silently have reused the window's number —
  which KVM implements as "replace that mapping", not as an error.
* **A window's slot is reserved for the life of the VM**, even while the window
  is unmapped. Deleting is `KVM_SET_USER_MEMORY_REGION` with `memory_size = 0`;
  moving is the same call on the same slot, which replaces it.
* **There is no execute permission to give or withhold.** `KVM_MEM_READONLY`
  exists, execute-disable does not. WHP maps the same window `Read | Write`;
  neither is a security difference that matters, but do not write documentation
  claiming KVM enforces it.
* The machine layer never sees any of this: it asks
  `vmm_core::shm::GpaMapper`, which is the ADR-0002 seam for exactly this
  (`machine_x86::shm`).

## Resetting a vCPU in place (ADR-0005)

KVM has no "reset this vCPU" ioctl, so `ResettableVcpu::reset_arch_state` writes
the state out by hand, in this order and for these reasons:

1. **`KVM_SET_VCPU_EVENTS`** with the exception/interrupt/NMI fields cleared. An
   injected interrupt left over from the guest that just died would be delivered
   into the new boot's first instructions.
2. **`KVM_SET_LAPIC`** with a power-on register page (id, version 0x14, DFR all
   ones, SVR 0xFF, every LVT masked). This is the one most easily skipped and
   most expensive to skip: a rebooted guest with the previous kernel's APIC timer
   still armed takes an interrupt a few hundred instructions in, before it has an
   IDT, and triple-faults.
3. **sregs then regs** to the architectural reset values. `apic_base` is forced
   back to `0xfee0_0000 | EN` (xAPIC): a guest that moved the APIC or enabled
   x2APIC must not hand that to the next boot. Note that
   `machine_x86::boot::setup_long_mode_sregs` *ORs* into `cr0`/`cr4`/`efer`, so a
   stale `CR4.LA57` would otherwise survive into a 4-level page table.
4. **`KVM_SET_MP_STATE`**: `RUNNABLE` for the boot CPU, `UNINITIALIZED` for every
   application processor — exactly where `KVM_CREATE_VCPU` left it. The new
   kernel's INIT/SIPI sweep then brings the AP up the way the first boot did, and
   KVM performs the real INIT reset itself.

Two traps around it:

- **Flush the pending userspace-I/O completion before rewriting registers.** KVM
  keeps it in the `kvm_run` mapping between an exit and the next `KVM_RUN`, and
  applies it at the top of that call *ahead of* the `immediate_exit` check — so
  one run with `set_kvm_immediate_exit(1)` retires it and returns `EINTR`. A
  multi-fragment MMIO access can produce one more exit while doing so, which is
  why the flush dispatches to the handler and is bounded.
- **Never re-enter a vCPU that reported `KVM_EXIT_SHUTDOWN`.** The next
  `KVM_RUN` answers `KVM_EXIT_INTERNAL_ERROR`. It has to park until the reset
  arrives (or, for an application processor, indefinitely — see the reset matrix
  in ADR-0005 for why an AP's triple fault must not restart the machine).


## Snapshotting a vCPU, and the chips that are not in userspace (ADR-0006)

`Vcpu::snapshot`/`restore` (`crates/vmm-core/src/snapshot_kvm.rs`) fill the
neutral `X86CpuState`. The rules that matter:

- **Ask the kernel for the MSR list; never write one down.**
  `KVM_GET_MSR_INDEX_LIST` is the host saying what *it* is prepared to save, and
  it is ~100 registers on a 6.x kernel. Read in batches: `KVM_GET_MSRS` returns
  how many entries it filled and **stops at the first one it cannot answer**, so
  a single unsupported register would otherwise cost every register behind it.
  Drop the offender (`rest[read]`) and carry on. The same shape on the way back,
  with `KVM_SET_MSRS`.
- **Skip the x2APIC window (`0x800..=0x8ff`).** It is the same local APIC the
  `KVM_GET_LAPIC` page carries, and restoring both has them fight.
- **Restore order is not arbitrary:** `mp_state` → `KVM_SET_LAPIC` → sregs →
  regs → `KVM_SET_XCRS` → `KVM_SET_XSAVE` → MSRs → debug registers →
  `KVM_SET_VCPU_EVENTS`. An AP must be back in its wait before anything touches
  it; `IA32_TSC_DEADLINE` is meaningless before the APIC timer exists; `XCR0`
  decides which components the XSAVE area may carry; pending events go last
  because everything above can clear them.
- **`KVM_SET_XSAVE` is `unsafe` in kvm-ioctls** because it can read past the
  4 KiB `kvm_xsave` when the host task has dynamically enabled an XSTATE feature
  through `arch_prctl`. This process never calls `arch_prctl`, and the snapshot's
  own XSTATE header is checked for dynamic components before the call — a
  snapshot claiming AMX is refused rather than truncated.

**The trap that cost a whole restore.** On this host the 8259 pair, the IOAPIC
and the 8254 are **in the kernel**, so `machine_x86::state` has never seen them
and a snapshot built only from the device list is missing them entirely. A VM
restored with a power-on IOAPIC has every pin masked: the first attempt came
back, kept drawing frames (MSI-X goes straight to the local APIC and bypasses
the IOAPIC) and never printed another line. `vmm_core::hv::HostIrqChip` — three
`KVM_GET_IRQCHIP` chips and `KVM_GET_PIT2` — is the seam that carries them, and
`Vm::irqchip()` hands it out. Restore them **after** the devices and with the
IOAPIC last, the mirror of the reset order.

## Tracking guest writes, and what the log misses (ADR-0006)

`Vm::dirty_log()` hands out a `vmm_core::hv::DirtyLog`: `KVM_MEM_LOG_DIRTY_PAGES`
toggled by **re-registering each RAM slot** (a `KVM_SET_USER_MEMORY_REGION` on an
existing slot number replaces it — that is the kernel's own spelling of a flag
change) and `KVM_GET_DIRTY_LOG` per slot to read it. The slots are kept as
`RamSlot` copies of exactly what was registered, because a re-registration whose
host address had drifted would silently repoint guest RAM at somebody else's
memory.

Three things to know before building on it:

- **The log does not see this process's writes.** KVM write-protects the *guest's*
  second-level page tables; a `memcpy` from the VMM into the anonymous mapping
  those tables point at faults nothing. Boot images, virtio-blk read completions,
  received packets, used-ring updates — none of them move a bit. That is why
  ADR-0006 does not build an incremental snapshot on this, and
  `crates/vmm-core/tests/dirty_log.rs` is the test that says so on both hosts.
- **On AMD it over-reports.** Measured on this machine: a page the guest only
  *executed* from comes back dirty. Harmless for a copier, fatal for anyone who
  reads the log as "what the guest wrote" and reasons from the count.
- **Reading clears.** No `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT` here, so
  `KVM_GET_DIRTY_LOG` re-protects as it reads and a second read reports the next
  window, not the sum. `DirtyTracking::read_clears` states it and the test checks
  it, because a claim about a hypervisor that nothing checks quietly stops being
  true.

Arming it costs the guest a write-protection fault on its first write to every
page — below the noise of a boot when measured (ADR-0006) — plus the cost that a
boot cannot show: a memslot with dirty logging on cannot use huge pages, so the
guest runs with 4 KiB second-level entries for as long as it is armed. Off by
default; `ENTANGLED_TRACK_DIRTY=1` turns it on.
