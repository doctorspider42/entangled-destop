---
name: whp-backend
description: Implementing the native Windows host backend of Entangled Desktop on WHP (Windows Hypervisor Platform) — partition and vCPU lifecycle, WHvMapGpaRange, register mapping, run-loop exit translation, the instruction emulator for MMIO, CPUID policy, and the userspace PIC/IOAPIC/PIT that WHP does not provide (backlog EPIC 17, crates vmm-core and machine-x86). Load before any work touching the `windows` crate, WHv* APIs, or `machine_x86::irqchip`.
---

# WHP backend

Scope: backlog EPIC 17 (WHP-1701…1705), ADR-0002. Two crates:

- `crates/vmm-core/src/whp/` — everything WHP-specific: `partition.rs`,
  `vcpu.rs`, `regs.rs`, `interrupt.rs`, `emulator.rs`, `cpuid.rs`. All behind
  `#[cfg(windows)]`; the `windows` crate is declared only under
  `[target.'cfg(windows)'.dependencies]`.
- `crates/machine-x86/src/irqchip/` — the userspace 8259/8254/IOAPIC. **Not**
  Windows-gated: see [Where the irqchip lives](#where-the-irqchip-lives-and-why).

Tests: `crates/vmm-core/tests/whp_smoke.rs` (8) and
`crates/vmm-core/tests/whp_boot.rs` (2).

## Status: phase 2 is done — Linux boots

`cargo test -p vmm-core --test whp_boot` boots the bootstrap kernel with the
marker initramfs to `VMHOST_GUEST_READY` on Windows, headless, in about 3.5 s in
a debug build, then tears the VM down. What the serial log says about the
machine, and why each line matters:

```text
..TIMER: vector=0x30 apic1=0 pin1=2 apic2=-1 pin2=-1   check_timer() passed on the first pin
tsc: PIT calibration matches PMTIMER. 2 loops          the 8254 and the ACPI PM timer agree
tsc: Detected 1894.399 MHz processor                   ...and with the host's real TSC
APIC: Switch to symmetric I/O mode setup               IOAPIC, not 8259 virtual wire
NR_IRQS: 4352, nr_irqs: 256, preallocated irqs: 16     the 8259 stub answered probe_8259A()
Booting paravirtualized kernel on bare hardware        no Hyper-V enlightenments (leaf 0x4000_0000)
serial8250: ttyS0 at I/O 0x3f8 (irq = 4 ...) 16550A    interrupt-driven console on IOAPIC pin 4
VMHOST_GUEST_READY
ACPI: PM: Preparing to enter system sleep state S5     ACPI poweroff latched
```

Also working from phase 1: partition lifecycle, guest memory mapping, vCPU
creation, register access through `vmm_core::hv::VcpuRegisters`, the
`WHvRunVirtualProcessor` loop with port-I/O emulation, stop via
`WHvCancelRunVirtualProcessor`, 100 leak-free create/destroy cycles.

Not done — see [Phase 3](#phase-3-what-is-still-missing).

## Build and test

```powershell
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\entangled-target-whp"
cargo test -p vmm-core -p machine-x86 -p linux-boot
cargo clippy -p vmm-core -p machine-x86 -p linux-boot --all-targets -- -D warnings
# the whole serial log of a boot, for diagnosing anything timer- or APIC-shaped
$env:ENTANGLED_WHP_BOOT_LOG = "$env:TEMP\whpboot.log"
cargo test -p vmm-core --test whp_boot -- --nocapture
```

Host `x86_64-pc-windows-gnu` works; no MSVC toolchain needed. Linux must stay
green in the same change — run the WSL regression too (see CLAUDE.md).

The boot test needs `artifacts/bootstrap/vmlinuz` (or `artifacts/tests/vmlinuz`
as a fallback) and `artifacts/tests/test-initramfs.cpio.gz`. Those are built by
Linux-side scripts; copying them in from another checkout is fine.

**The whole workspace builds natively now**, including the `virtio-*` crates and
`display`, thanks to two vendored patches in `third_party/` (see their
VENDORED.md). What is still Linux-only above `vmm-core` is device *wiring* —
`machine_x86::irqfd`, `notify`, `virtio`, `virtio_pci` — because irqfds and
ioeventfds are KVM concepts.

WHP needs the **"Windows Hypervisor Platform"** optional feature (admin +
reboot; `dism /Online /Enable-Feature /FeatureName:HypervisorPlatform`). It
coexists with WSL2 — both ride Hyper-V. Without it every test self-skips with
that hint; `WhpHypervisor::probe()` reports it instead of failing, the way
`Hypervisor::probe()` does for `/dev/kvm`.

## Existing pieces — build on these, don't duplicate

- `vmm_core::hv` holds the neutral types: `X86Registers`,
  `X86SpecialRegisters`, `X86Segment`, `MachineConfig`, `RunOutcome`,
  `ExitHandler`, `VcpuRegisters`, and — new in phase 2 — `InterruptDelivery`
  with `InterruptRequest`/`InterruptKind`/`DestinationMode`/`TriggerMode`.
  Machine code goes through these and must never see a `WHV_*` type, exactly as
  it never sees a `kvm_bindings` one.
- `vmm_core::create_guest_memory` / `GuestMem` are **portable**. Do not write a
  second guest-memory type.
- `whp::regs` owns every name/value table and the bit packing. Add registers
  there, keeping `*_NAMES` index-for-index in sync with
  `*_values`/`*_from_values`; the unit tests pin the order.
- `whp::partition::whp_err` turns an `HRESULT` into
  `VmmError::Whp { call, message }`. Never let a WHP failure surface as a bare
  hex code.
- `whp::partition::set_property` is the `unsafe fn` for fixed-size partition
  properties. Variable-length ones (the CPUID exit list) call
  `WHvSetPartitionProperty` directly, with the array's whole size.
- `machine_x86::irqfd::IrqFdLine` and `machine_x86::irqchip::ioapic::IoApicLine`
  are the two implementations of `virtio_core::interrupt::IrqLine`. Every
  consumer — the serial console, both virtio transports — takes an
  `Arc<dyn IrqLine>` and must not be able to tell them apart.

## Type mappings: `hv` ↔ WHV

| `hv` type | WHP |
|---|---|
| `X86Registers` (18 fields) | `GP_NAMES` + the `Reg64` union arm |
| `X86Segment` | `WHV_X64_SEGMENT_REGISTER` (`Segment` arm) — attributes packed by hand |
| `X86DescriptorTable` | `WHV_X64_TABLE_REGISTER` (`Table` arm), `Pad` zeroed |
| `X86SpecialRegisters` cr0/cr2/cr3/cr4/cr8/efer | `Reg64` arm |
| `X86SpecialRegisters::apic_base` | `WHvX64RegisterApicBase`, **separate best-effort call** |
| `MachineConfig::vcpu_count` | `WHvPartitionPropertyCodeProcessorCount` (before `WHvSetupPartition`) |
| `MachineConfig::memory_mib` | `WHvMapGpaRange` of one RWX region at GPA 0 |
| `InterruptRequest` | `WHV_INTERRUPT_CONTROL` + `WHvRequestInterrupt` |
| `WhpOptions::local_apic` | `WHvPartitionPropertyCodeLocalApicEmulationMode` = `XApic` |
| `WhpOptions::cpuid_policy` | `ExtendedVmExits.X64CpuidExit` + `CpuidExitList` |

### Segment attributes

`WHV_X64_SEGMENT_REGISTER::Attributes` is a 16-bit bitfield the `windows` crate
exposes only as an opaque `_bitfield`, so `whp::regs` packs it. LSB first, per
WinHvPlatformDefs.h — the same encoding VMX uses for segment access rights:

| Bits | Header field | `X86Segment` |
|---|---|---|
| 0–3 | `SegmentType` | `type_` |
| 4 | `NonSystemSegment` | `s` |
| 5–6 | `DescriptorPrivilegeLevel` | `dpl` |
| 7 | `Present` | `present` |
| 8–11 | `Reserved` | — (write 0) |
| 12 | `Available` | `avl` |
| 13 | `Long` | `l` |
| 14 | `Default` | `db` |
| 15 | `Granularity` | `g` |

WHP has no counterpart to KVM's `unusable`: an unusable segment is `Present=0`.
`seg_to_whp` clears `Present` when `unusable` is set; `seg_from_whp` reports
`unusable: 0` always. `machine-x86` only ever writes `unusable: 0`.

### `WHV_INTERRUPT_CONTROL`

Also an opaque `_bitfield`, also packed by hand (`whp::interrupt`), also pinned
by unit test. From WinHvPlatformDefs.h, LSB first: `Type:8`,
`DestinationMode:4`, `TriggerMode:4`, `Reserved:48`, then `Destination: u32` and
`Vector: u32` as real fields. Fixed / physical / edge is all zeroes, which is
what most of this machine's lines are.

## Exit translation

| `WHV_RUN_VP_EXIT_REASON` | Backend behaviour |
|---|---|
| `X64Halt` | with a local APIC: wait on `HaltGate` and re-enter. Without one: `RunOutcome::Halted` |
| `X64IoPortAccess` | `ExitHandler::io_out`/`io_in`, then RIP += instruction length, and RAX write-back for `IN`. String/`REP` forms go to `WHvEmulatorTryIoEmulation` |
| `MemoryAccess` | `WHvEmulatorTryMmioEmulation`; an *execute* fault is reported as an error instead (a bad jump target, not MMIO) |
| `X64Cpuid` | `CpuidPolicy::apply` on top of `DefaultResultRax..Rdx`, then write RAX–RDX and advance RIP |
| `UnrecoverableException`, `InvalidVpRegisterValue` | `RunOutcome::Shutdown` — the triple-fault equivalent |
| `Canceled`, `None` | re-check the stop flag, then re-enter or `RunOutcome::Stopped` |
| anything else | `VmmError::Vcpu` naming the reason and RIP |

Two things KVM does for us that **WHP does not**:

1. **RIP is never advanced.** Every emulated instruction must add
   `VpContext.InstructionLength` (the low nibble of `VpContext._bitfield`) to
   `VpContext.Rip` and write RIP back. The one exception is the instruction
   emulator, which advances RIP itself through the register-write callback.
2. **Nothing is decoded.** The port-I/O exit does carry port, direction and
   access size; the MMIO exit carries only the GPA, the access type and raw
   instruction bytes.

### `hlt` is an idle loop, not an ending

The single most consequential difference. KVM with an in-kernel irqchip absorbs
`hlt` in the kernel — the vCPU blocks inside `KVM_RUN` and userspace never sees
it. WHP always reports `WHvRunVpExitReasonX64Halt`, and a Linux guest executes
`hlt` on *every* trip through `default_idle()`. Returning on the first one stops
the guest milliseconds into boot; re-entering immediately spins a host core at
100%.

`whp::interrupt::HaltGate` is the answer: an epoch counter plus a condition
variable, bumped by every `WHvRequestInterrupt`. The run loop snapshots the epoch
**before** entering the guest and waits on it after a halt, which is what closes
the race where an injection lands between the `hlt` executing and the exit being
observed. The wait is a wake-up *hint*, not the interrupt: the vector is already
in WHP's APIC by then, so a spurious wake costs one round trip and a missed wake
costs at most `HALT_POLL` (10 ms).

`VcpuCanceller::cancel` notifies the gate as well as cancelling, because a halted
vCPU is not inside `WHvRunVirtualProcessor` at all — cancelling only arms its
next entry.

WHP **does** advance RIP past the `hlt` before reporting the exit. Verified by
the boot working at all: with RIP left on the `hlt`, `default_idle()` would
re-halt after every interrupt and never reach its `need_resched` check, and the
guest would appear to hang at first idle.

## Where the irqchip lives, and why

`machine-x86::irqchip`, portable and tested on both hosts — **not**
`vmm-core::whp`. KVM provides all three chips in the kernel
(`KVM_CREATE_IRQCHIP`, `KVM_CREATE_PIT2`), WHP provides only each vCPU's local
APIC, so the *gap* is host-specific; the *fix* is not. A redirection table, a
counter driven by a clock and an 8259 register file are the machine's devices,
the same category as the 16550 beside them.

Exactly one step is genuinely WHP: handing a decoded interrupt message to a local
APIC. That is `vmm_core::hv::InterruptDelivery`, one method, implemented by
`whp::WhpInterruptDelivery` and deliberately **not** implemented by KVM.

```text
                                   machine-x86 (portable)          vmm-core::whp
  8254 ch0 --IRQ 0--> IoApicLine(pin 2) \
  16550    --IRQ 4--> IoApicLine(pin 4)  >-- IoApic --> InterruptDelivery --> WHvRequestInterrupt
  virtio n --IRQ 5+n-> IoApicLine(5+n)  /   (RTE decode)                          |
                                                                             HaltGate::notify
```

Wiring one up:

```rust
let partition = WhpPartition::with_options(&hv, &cfg, WhpOptions::for_guest())?;
let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), cfg.vcpu_count)?;
let serial = SerialConsole::with_trigger(irqchip.serial_line(), out);
let bus = MachineBus::new(serial).with_irqchip(Arc::clone(&irqchip));
```

On KVM the same bus is built *without* `with_irqchip`, and the 8259/8254 ports
and the IOAPIC page stay with the in-kernel chips. Claiming them on Linux would
fight KVM.

### IOAPIC design notes

24 redirection entries at `layout::IOAPIC_ADDR` (0xFEC00000), GSI base 0 — the
topology `mptable` and `acpi` already publish, so a guest cannot see two
different machines. `IOREGSEL` at +0x00, `IOWIN` at +0x10, 32-bit accesses only.
Two deliberate deviations from hardware, both load-bearing:

- **No remote IRR / EOI tracking.** A real IOAPIC latches a level-triggered
  interrupt until the local APIC broadcasts the EOI, which WHP can report
  (`WHvRunVpExitReasonX64ApicEoi`). Every line this machine owns is an edge on an
  ISA-style pin, so nothing needs re-assertion and turning the EOI exit on would
  only cost exits. Same call the KVM irqfd path already made.
- **A masked pin latches one pending edge**, delivered on unmask. Hardware loses
  it. The asymmetry is deliberate: Linux masks and unmasks IRQ 0 repeatedly in
  `check_timer()`, and a UART THRE edge lost inside a mask window stalls the
  console for good. One bit per pin, so a stuck device cannot grow host state.

The guest-controlled `IOREGSEL` becomes a table index in exactly one function
(`redirection_index`), which range-checks it. Delivery modes SMI/INIT/ExtINT are
dropped with a `debug!`; ExtINT is the one a guest really programs (pin 0, for
virtual-wire mode) and dropping it is correct here because nothing is wired to
the 8259's INTR.

`InterruptDelivery::request` is never called under the IOAPIC lock — it can
re-enter host code. The unmask path collects the released pin, drops the lock,
then delivers.

### PIT design notes

Counters are **computed from elapsed host time**, not stepped. That is what makes
the guest's TSC calibration land on the host's real frequency, and it is why the
log line `tsc: PIT calibration matches PMTIMER` is the single best signal that the
8254 is right.

Two consumers, in this order:

1. `quick_pit_calibrate()` — channel 2, mode 0, latch `0xffff`, gate opened
   through port `0x61` bit 0, MSB watched with *unlatched* lo/hi reads of `0x42`.
   Fallback `pit_calibrate_tsc()` polls channel 2's OUT at `0x61` bit 5.
2. `pit_timer_init()` programs channel 0 as a rate generator at `HZ` and
   registers it as `global_clock_event`; then `setup_IO_APIC()` →
   `check_timer()` → `timer_irq_works()` needs jiffies to advance by more than
   four inside ten jiffies' worth of milliseconds. Failure looks like
   `..MP-BIOS bug: 8254 timer not connected to IO-APIC` and ends in
   `IO-APIC + timer doesn't work!`. The same interrupt then calibrates the local
   APIC timer.

Only channel 0 has an output pin wired anywhere (IRQ 0 → `TIMER_PIN` = 2).
`Pit::tick` delivers the edges that came due and reports when the next one is;
`PitTimer` is the host thread that calls it and joins on drop.

`MAX_CATCHUP_EDGES` is 16, sized against the *host's* sleep granularity rather
than a round number: Rust's `thread::sleep` on Windows 11 rides a
high-resolution waitable timer and overshoots a 1 ms request by ~0.5 ms
(measured), but a loaded host can miss a ~15 ms quantum, which is 15 owed edges
at the x86_64 defconfig's `HZ=1000`. Below that the guest's jiffies drift
permanently behind, which is exactly what `timer_irq_works()` reports as a broken
8254.

The clock is injectable (`Clock::Fake`, `#[cfg(test)]`) so counter and edge
arithmetic is asserted deterministically instead of by sleeping. One test does
use the real clock, to prove the conversion is right.

### PIC design notes

An 8259A pair with the full ICW sequence, IMR, the OCW3 read select and the ELCR
pair at `0x4d0`/`0x4d1` — and **no delivery path at all**. It exists for
`probe_8259A()`, which writes `0xfb` to `0x21` and requires it to read back. An
unclaimed ISA port floats high, so with no PIC Linux installs `null_legacy_pic`,
`nr_legacy_irqs()` drops to 0, `check_timer()` and the PIT clockevent disappear
and the WHP guest quietly becomes a *different machine* from the KVM one. Watch
for `preallocated irqs: 16` in the log; `Using NULL legacy PIC` is the failure.

## The instruction emulator

`WHvEmulatorCreateEmulator` (winhvemulation.dll) with five callbacks, created
lazily on the first exit that needs decoding so a port-I/O-only guest never loads
the DLL:

| Callback | Maps to |
|---|---|
| `WHvEmulatorMemoryCallback` | `ExitHandler::mmio_read` / `mmio_write` |
| `WHvEmulatorIoPortCallback` | `ExitHandler::io_in` / `io_out` (string I/O path) |
| `WHvEmulatorGetVirtualProcessorRegisters` | `WHvGetVirtualProcessorRegisters` |
| `WHvEmulatorSetVirtualProcessorRegisters` | `WHvSetVirtualProcessorRegisters` — **this is how RIP gets advanced** |
| `WHvEmulatorTranslateGvaPage` | `WHvTranslateGva` |

`WHV_EMULATOR_CALLBACKS::Size` must be `size_of` the struct; WHP versions it that
way and a mismatch is an opaque `E_INVALIDARG`.

Direction in both access-info structs: `0` = guest read (fill `Data`), `1` =
guest write (consume it).

Rules that are easy to get wrong:

- **The context pointer** is the `&mut EmulatorContext` passed to
  `TryMmioEmulation`; WHP calls back synchronously on the same thread inside the
  one call, which is why the `&mut` inside it is never aliased.
- **No unwinding.** The callbacks are `extern "system"`, so a panic crossing one
  aborts the process. Every path returns an `HRESULT`; a failed handler surfaces
  as a failed emulation, which the run loop turns into a typed error naming the
  GPA.
- **Alignment applies to WHP's buffer too.** The register-value buffer the
  emulator hands the register callbacks has no 16-byte guarantee of its own, so
  both callbacks stage through an over-aligned buffer. `Vec<Aligned16<T>>` is the
  unbounded fallback: `Vec` allocates at `align_of::<T>()`, and
  `Aligned16<WHV_REGISTER_VALUE>` is the same 16 bytes, so the backing store has
  `[WHV_REGISTER_VALUE; n]`'s layout, over-aligned.
- **`WHV_EMULATOR_STATUS` bit 0 is `EmulationSuccessful`**; the other nine name
  which part gave up. Report the reason, not the raw word.

## CPUID policy

`whp::cpuid`. The API choice is the finding: **`CpuidResultList` is wrong for
editing a leaf.** It takes complete results, and no WHP call reports what WHP
would otherwise have returned, so using it means inventing every bit of a leaf —
including the feature bits WHP masks for its own reasons.
`WHvPartitionPropertyCodeCpuidExitList` plus
`WHV_EXTENDED_VM_EXITS.X64CpuidExit` (bit 0) gives an exit whose context carries
`DefaultResultRax..Rdx`, which makes the policy a *diff*, the same shape as the
KVM path's edit of `KVM_GET_SUPPORTED_CPUID`.

Six leaves in `CPUID_EXIT_LEAVES`; keep the list short, every entry costs an exit
each time the guest reads it. Five mirror `vmm_core::Vcpu::new` exactly:

| Leaf | Change |
|---|---|
| `0x1` | `ECX[31] = 1` (hypervisor present); `EBX[31:24] = vp_index` (initial APIC id) |
| `0xb`, `0x1f`, `0x8000_0026` | `EDX = vp_index` (x2APIC id) |
| `0x8000_001e` | `EAX = vp_index` (AMD extended APIC id — the one Linux trusts with TOPOEXT) |

The sixth is the one place the hosts legitimately differ: **`0x4000_0000` is
zeroed.** A WHP partition runs on Hyper-V, and a guest that sees `"Microsoft Hv"`
starts using synthetic MSRs an exo-partition does not implement. With an empty
hypervisor interface and the hypervisor-present bit still set,
`detect_hypervisor_vendor()` finds no match and the guest takes the architectural
paths — hence `Booting paravirtualized kernel on bare hardware` in the log, and
no kvmclock (there is none to have).

MSR exits are deliberately **off**: WHP's own MSR policy is better than one we
would write, and unknown MSRs already `#GP` to the guest, which Linux's safe
accessors expect. The boot log's two `unchecked MSR access error` lines
(`0xc0010015`, `0xc001001f` — AMD-specific) are that working as intended.

## Traps that cost real debugging time

- **16-byte alignment is mandatory.** `WHV_UINT128` is `DECLSPEC_ALIGN(16)` in
  the header, but the generated binding drops it, leaving
  `WHV_REGISTER_VALUE` at alignment 8. A bare `[WHV_REGISTER_VALUE; N]` local
  lands on an 8-mod-16 address roughly half the time and WHP faults with
  `STATUS_ACCESS_VIOLATION` *inside* the call — no error, no backtrace, just a
  dead process. Always use `regs::Aligned16`; `WhpVcpu::batch_count` rejects a
  misaligned buffer with a typed error rather than trusting the caller. Applies
  to buffers WHP hands *us* as well (see the emulator section).
- **One mapped partition per process.** A second concurrent partition is
  created fine, but its first `WHvMapGpaRange` fails with `0xC0370008`
  ("another partition with the same name already exists"). Sequential
  create/destroy is unaffected. Consequence: on Windows a multi-VM `entangled`
  needs one process per VM, and concurrent WHP tests must serialise
  (`whp_guard()` in both test files).
- **`WHvEmulatorTryMmioEmulation` refuses 16-bit real mode**, failing with
  `internal emulation failure` (status `0x2`) without calling back at all. The
  MMIO smoke test is therefore a *long-mode* guest, built with the same
  `machine_x86::boot::setup_long_mode_sregs` the real boot uses.
- **`apic_base` is not readable until APIC emulation is on**, and asking for it
  inside a batched call fails the whole batch. Hence the separate call.
- **Ordering is strict.** Partition properties (processor count, local APIC
  emulation mode, extended VM exits, CPUID exit list) only before
  `WHvSetupPartition`; GPA ranges and virtual processors only after. A guest's
  shape is therefore fixed at creation, which is why `WhpOptions` is a
  constructor argument and not a setter.
- **Teardown order matters.** `WHvDeletePartition` must run before the guest
  RAM is freed, and every VP must be deleted before its partition.
  `Partition` owning `GuestMem` plus `Arc<Partition>` in each `WhpVcpu` makes
  both orderings structural rather than a convention to remember.
- **The `windows` crate constants are `WHV_*(i32)` newtypes**, so matching on
  them needs `#[allow(non_upper_case_globals)]` (CI runs `-D warnings`).

## Phase 3: what is still missing

In dependency order:

1. **virtio on Windows.** The device crates and both transports already build
   natively; what does not is the *attach* path. `machine_x86::virtio` and
   `virtio_pci` take a `VmFd`, create an `EventFd` per device for the irqfd and
   (by default) one ioeventfd per queue. On WHP the interrupt line is already
   solved — `irqchip.ioapic().line(layout::VIRTIO_MMIO_FIRST_IRQ as u8 + slot)`
   is an `Arc<dyn IrqLine>` the transport takes unchanged. The queue-*kick* side
   is the open question: there is no ioeventfd, so either every kick runs inline
   on the vCPU thread (`QueueNotifyMode::Synchronous`, which already exists and
   is correct, just slower) or it goes through `WHvSetVirtualProcessorNotification`
   /doorbell (`WHV_DOORBELL_MATCH_DATA` is in the binding). Start synchronous;
   measure before adding doorbells. Concretely: split the KVM-specific halves of
   `virtio.rs`/`virtio_pci.rs`/`notify.rs` out of the address decoding and
   cmdline generation, which are already portable, and un-gate the two virtio
   fields in `bus.rs`.
2. **SMP.** One vCPU today. An AP is started by INIT/SIPI, which needs
   `ExtendedVmExits.X64ApicInitSipiExitTrap` and a handler that puts the target
   VP at the SIPI vector. The CPUID policy is already per-vCPU and the MP
   table/MADT already describe *n* CPUs, so this is the WHP half only.
3. **Networking (WHP-1704).** No TAP on Windows, and wintun/tap-windows6 are
   GPL (blocked by `cargo deny`). Plan unchanged: user-mode NAT on `smoltcp`
   (0BSD), which also buys rootless networking on Linux.
4. **The GUI/CLI run path.** `apps/entangled`'s `run` and `control-api`'s
   lifecycle still assume KVM. The seam is ready; what is needed is a backend
   choice at VM construction and an `entangled doctor` arm reporting
   `WhpCapabilities` (which already exists and is unused). Remember the
   one-mapped-partition-per-process limit: the manager must spawn one process per
   VM on Windows.
5. **UEFI on WHP.** `uefi-boot` is portable and `setup_pvh_sregs` goes through
   the same seam, so this may already work; nobody has tried. Reset-vector ROM
   placement needs a second `WHvMapGpaRange` below 4 GiB.
6. **CI (WHP-1705).** A `windows-latest` matrix job can build, clippy and run
   the non-WHP tests; GitHub's runners have no nested virtualisation, so the WHP
   tests will self-skip there — which is exactly why they self-skip rather than
   fail.

## Hard rules that apply here

- Every `unsafe` block gets a `// SAFETY:` comment
  (`undocumented_unsafe_blocks` is `deny`). WHP FFI is the densest `unsafe` in
  the tree — say *why* the pointer is valid and which union arm is live, not
  that the call is an FFI call.
- No `unwrap`/`expect` on a runtime path; typed `VmmError` variants only.
- Guest-controlled values (exit contexts, register contents, `IOREGSEL`) never
  index host memory directly — go through `vm-memory`'s checked APIs or a
  range-checking helper.
- Keep the Linux API untouched. WHP additions are new types under `whp::`, and
  new machine devices under `machine_x86::`, never changes to `Vcpu`/`Vm`
  signatures. `WhpPartition::new` keeps the phase-1 behaviour so phase-1 tests
  keep passing unchanged.
