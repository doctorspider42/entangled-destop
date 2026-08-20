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
