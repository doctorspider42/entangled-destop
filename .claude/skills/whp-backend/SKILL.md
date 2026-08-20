---
name: whp-backend
description: Implementing the native Windows host backend of Entangled Desktop on WHP (Windows Hypervisor Platform) — partition and vCPU lifecycle, WHvMapGpaRange, register mapping, run-loop exit translation, the instruction emulator for MMIO, CPUID policy, and the userspace PIC/IOAPIC/PIT that WHP does not provide (backlog EPIC 17, crates vmm-core and machine-x86). Load before any work touching the `windows` crate, WHv* APIs, or `machine_x86::irqchip`.
---

# WHP backend

Scope: backlog EPIC 17 (WHP-1701…1705), ADR-0002. Three places:

- `crates/vmm-core/src/whp/` — everything WHP-specific: `partition.rs`,
  `vcpu.rs`, `regs.rs`, `interrupt.rs`, `emulator.rs`, `cpuid.rs`. All behind
  `#[cfg(windows)]`; the `windows` crate is declared only under
  `[target.'cfg(windows)'.dependencies]`.
- `crates/machine-x86/src/irqchip/` — the userspace 8259/8254/IOAPIC — and
  `machine-x86/src/msi.rs` — the portable MSI decode + `UserspaceMsiSink`.
  **Not** Windows-gated: see [Where the irqchip lives](#where-the-irqchip-lives-and-why).
- `apps/entangled/src/run_vm.rs` — the product wiring: one shared run body, one
  per-OS `host::start()` doing machine assembly.

Tests: `crates/vmm-core/tests/` — `whp_smoke.rs` (8), `whp_boot.rs` (2),
`whp_virtio_blk.rs` (1), `whp_smp.rs` (1), `whp_virtio_pci.rs` (1),
`whp_usernet.rs` (1), `whp_uefi.rs` (1), with the shared plumbing in
`whp_common/`. Every one self-skips when WHP is off or the guest artifacts are
missing. The portable halves are unit-tested in `machine-x86` and `virtio-net` and
run on both hosts.

## Status: phase 4 is done — `entangled run` is native on Windows

On Windows, natively, in a debug build (2026-08-19):

| What | Evidence |
|---|---|
| `entangled run examples\windows-whp.toml` | window created (wgpu on Vulkan), boot to the marker in ~4 s, guest ends via ACPI S5, `VM finished state=Stopped`, exit 0; `--headless` likewise |
| virtio-pci + MSI-X | 8 MiB off `/dev/vda` in 30 ms = **273066 KiB/s**, `irqmode=msix`, pciscan `virtio=1 bound=1 msix=2` (`--test whp_virtio_pci`) |
| user-mode networking, in-guest | static-configured eth0 through the NAT to a host TCP listener and the echo back; `tcp_flows=1` on the host side (`--test whp_usernet`, the guest half is `entangled.netprobe=`) |
| UEFI + NVRAM | CloudHv boots to the Boot Manager twice against one NVRAM file: first boot programs 3423 bytes, second reuses them (1538 programmed, 0 erased) — the same numbers as the KVM run (`--test whp_uefi`) |

## Status: phase 5 is done — `entangled install` is native on Windows

`entangled install ubuntu --size 20G --auto --headless` on this Windows host,
debug build, no WSL involved (2026-08-20):

| What | Evidence |
|---|---|
| the whole install | **4 min 27 s** wall clock, `installed: GPT with an ESP on /dev/vda1 (953 MiB) and root on /dev/vda2 (ext4 UUID 153f909e-…)`, exit 0 |
| GRUB typed at over ttyS0 | the four commands echoed in the transcript, `autoinstall` on the kernel command line, subiquity ran every section unattended |
| the ending | `reboot: Power down` → `guest requested ACPI S5 (soft off) via="PM1a_CNT"` → `VM finished state=Stopped`; the transcript is `<vm>-install.log` (173 010 bytes) |
| NVRAM | `UEFI variable store written by the firmware programmed_bytes=5328 erased_blocks=0 refused=0 store_errors=0` |
| the installed disk | `entangled run <vm>.toml` boots it: EDK2 → the `Boot####` entry grub-install wrote → GRUB's menu on ttyS0 → systemd → login prompt |

All of that is now the unattended
`cargo test -p entangled --test ubuntu_install -- --ignored`, green on this host
(install 189 s, install + boot-what-was-installed 332 s) rather than a procedure
someone follows. It could not pass here before, for a reason unrelated to the
port — it read the serial transcript with `read_to_string`, which fails on the
0x00..0xFF range an installed Ubuntu writes while setting up its console font,
and matched a marker systemd splits with a colour escape. If you are writing a
boot-to-marker test, read the serial-transcript rules in the vm-testing skill
first; they cost two full acceptance runs to learn.

Nothing in the installer is Windows-specific. What made it Linux-only was a
`#[cfg]`, a `$HOME`-only cache lookup and a TAP default; details and the
reasoning are in ADR-0002's phase-5 amendment. Two things worth knowing here:

- **The Ubuntu install is offline by design**, so the NAT is not on its critical
  path at all. `--network` only matters for `install debian`, which is d-i and
  downloads everything; there `usernet`'s address comes from
  `virtio_net::UserNetConfig` so the `[network]` section and the
  `netcfg/get_ipaddress=` clause cannot disagree.
- **A profile's own `ip=` clause now wins** over the backend's appended one
  (`run_vm::host_api::direct_linux_cmdline`), which is what makes `ip=dhcp`
  askable — the one way to exercise the usernet DHCP server from a real kernel
  rather than from unit tests. That was the open phase-4 item, and it is closed:

  ```text
  host : granted the guest a DHCP lease mac=52-8f-7b-c3-42-a3 ip=192.168.74.15
  guest: IP-Config: Got DHCP answer from 192.168.74.1, my address is 192.168.74.15
         device=eth0, ipaddr=192.168.74.15, mask=255.255.255.0, gw=192.168.74.1
         nameserver0=192.168.74.1
  ```

- **The NAT leaked a flow per closed connection until now**, which nothing
  before `install debian` opened enough connections to notice. Symptom:
  d-i stalls at "Loading additional components" after 64 udebs with
  `refusing a guest connection: the NAT is at its flow limit flows=64`. Cause
  and fix in `usernet::tcp::service_flows` — the guest's FIN is propagated as
  `Shutdown::Write` on the host stream, because `CloseWait` is `is_open()` and
  the retirement test never fired there. If you touch that module: a test that
  closes both halves at once cannot see this class of bug.

One benign teardown noise to expect, unchanged by this work: after S5 the
supervisor cancels the other vCPU and WHP answers
`WHvCancelRunVirtualProcessor failed: A virtual processor with the specified
index does not exist (0x80370307)` — the VP is already gone. It is a `WARN` on a
VM that has already stopped cleanly.

## Status: phase 3 — virtio, SMP and user-mode networking

| What | Evidence |
|---|---|
| Linux boots to the marker | 3.4 s, `cargo test -p vmm-core --test whp_boot` |
| virtio-blk on virtio-mmio | 8 MiB off `/dev/vda` in 28 ms = **292571 KiB/s**, 65 interrupts on the virtio line (`--test whp_virtio_blk`) |
| 2 vCPUs | `smpboot: Total of 2 processors activated` (`--test whp_smp`) |
| user-mode networking | portable, 30 unit tests on both hosts |

The three findings that cost the time, each one the opposite of what the plan
said:

1. **The virtio-mmio bus was never Windows-specific** — only its two wiring
   primitives were. `VirtioMmioBus::attach_userspace` takes the line from
   `UserspaceIrqChip::virtio_line` and leaves kicks synchronous; `virtio.rs`
   moved out of the Linux gate and the unit tests now run on both hosts. See
   [virtio on Windows](#virtio-on-windows).
2. **SMP needs no INIT/SIPI code, and the trap breaks it.** See
   [SMP](#smp-the-trap-is-the-bug).
3. **`set_any_ip` needs a default route** or smoltcp silently drops the guest's
   SYN. See `virtio_net::usernet::tcp`.

## Status: phase 2 — Linux boots

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
cargo test --workspace          # the whole workspace builds and tests natively
cargo clippy --workspace --all-targets -- -D warnings
# the product path itself:
cargo run -p entangled -- run --headless examples\windows-whp.toml
# the whole serial log of a boot, for diagnosing anything timer- or APIC-shaped
$env:ENTANGLED_WHP_BOOT_LOG = "$env:TEMP\whpboot.log"
cargo test -p vmm-core --test whp_boot -- --nocapture
# every exit reason and RIP, for a guest that stopped making progress
$env:ENTANGLED_WHP_TRACE_EXITS = "1"
```

Host `x86_64-pc-windows-gnu` works; no MSVC toolchain needed. Linux must stay
green in the same change — run the WSL regression too (see CLAUDE.md).

The boot tests need `artifacts/bootstrap/vmlinuz` (or `artifacts/tests/vmlinuz`
as a fallback) and `artifacts/tests/test-initramfs.cpio.gz`; `whp_virtio_blk`
also needs a raw disk image at `artifacts/tests/test-root.raw`. Those are built by
Linux-side scripts; copying them in from another checkout is fine.

**The whole workspace builds natively**, including the `virtio-*` crates and
`display`, thanks to two vendored patches in `third_party/` (see their
VENDORED.md). What is still Linux-only above `vmm-core` is `machine_x86::irqfd`
and `notify` — irqfds and ioeventfds are KVM concepts. `virtio.rs` and
`virtio_pci.rs` are **not** on that list any more: see
[virtio on Windows](#virtio-on-windows).

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

Deliberately **not** handled, because WHP does it better itself:
`X64ApicInitSipiTrap` — see [SMP](#smp-the-trap-is-the-bug).

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
- **The emulator routes *every* memory operand through the memory callback**,
  not just the device window that faulted. A memory-to-memory instruction with
  one MMIO operand — EDK2's `CopyMem` out of the pflash window is `rep movs` —
  asks the callback to serve its guest-RAM side too, and a callback that only
  dispatches to the device bus silently drops those accesses. Measured symptom:
  the firmware copied its own variable store as zeroes and reported
  `Firmware Volume for Variable Store is corrupted`. The callback serves
  RAM-backed GPAs from `GuestMem` first (checked accessors) and falls through
  to `ExitHandler` for the rest. No Linux guest ever hit this: kernels do not
  point memory-to-memory instructions at device windows.
- **A triple fault does not end a WHP VM with local APIC emulation on.** KVM
  reports `KVM_EXIT_SHUTDOWN`; WHP absorbs the reset and the VP parks inside
  `WHvRunVirtualProcessor` with no exit at all — `reboot=k` (the test guests'
  default ending) therefore hangs a WHP run that waits for the guest. End WHP
  guests through ACPI S5 (`entangled.poweroff=1` for the test initramfs), which
  both hosts turn into a clean stop through the PM-block latch. Related fix in
  both backends' `join_or_stop`: one finished vCPU now stops the rest — a
  multi-CPU guest that triple-faults on the BSP leaves its APs parked forever,
  and no run loop returns while its guest is healthy.

## virtio on Windows

`VirtioMmioBus` has two constructors and one body. `attach`/`attach_with` are the
KVM ones (irqfd per device, ioeventfd per queue); `attach_userspace(mem, devices,
&irqchip)` is the other host's, and everything else — `layout` addresses,
`MmioTransport`, `cmdline_clauses`, `locate` — is shared. `bus.rs` routes the
virtio window unconditionally now.

```rust
let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), cfg.vcpu_count)?;
let mem = Arc::new(partition.memory().clone());
let virtio = VirtioMmioBus::attach_userspace(mem, devices, &irqchip)?;
let clauses = virtio.cmdline_clauses();           // goes on the kernel cmdline
let bus = MachineBus::with_virtio(serial, virtio).with_irqchip(Arc::clone(&irqchip));
```

Two things to know:

- **Kicks are synchronous, and that is fine so far.** There is no ioeventfd, so a
  `QUEUE_NOTIFY` write is a full exit *plus* instruction emulation, and the device
  then runs on the vCPU thread. Measured: 292571 KiB/s on a virtio-blk sequential
  read, debug build. A doorbell
  (`WHvRegisterPartitionDoorbellEvent`/`WHV_DOORBELL_MATCH_DATA`, both in the
  binding) would remove the emulation and the serialisation, but nothing yet needs
  it. Re-take the number with `--test whp_virtio_blk -- --nocapture` before
  deciding otherwise.
- **virtio-pci got the same split in phase 4** — `VirtioPciBus::attach_userspace`
  next to the KVM `attach`, sharing one `attach_function` body. The rebasing
  problem *dissolved* rather than got solved: with synchronous kicks nothing is
  registered at an absolute address, and every access is decoded against the
  BAR's current base by `PciRoot::locate_mmio`, so a guest moving a BAR needs no
  host follow-up at all. MSI-X delivery is the portable
  `machine_x86::msi::decode_msi_message` (Intel SDM address/data layout, unit
  tests on both hosts) feeding `InterruptDelivery` — the same seam the IOAPIC
  uses, one `WHvRequestInterrupt` per message, which also bumps the `HaltGate`.
  Acceptance: `--test whp_virtio_pci` (pciscan + blkbench with `irqmode=msix`).

## SMP: the trap is the bug

`vcpu_count` above one works with **no INIT/SIPI code at all**. WHP's xAPIC
emulation models an application processor's wait-for-startup state itself: create
*n* VPs, run every one, and each AP blocks *inside* `WHvRunVirtualProcessor` until
the guest's SIPI arrives — the same shape as a KVM AP blocking inside `KVM_RUN`.
WHP applies the INIT and the SIPI and the run call returns with the AP in the
kernel's trampoline.

`WHV_EXTENDED_VM_EXITS.X64ApicInitSipiExitTrap` (bit 6) **replaces** that handling
rather than observing it. Arm it and you get the exit with the raw ICR, and the
target VP stays in its wait-for-startup state: writing its `CS` and `RIP` by hand
does not make it runnable, its run call keeps blocking, and the guest boots
happily on one CPU and prints `CPU1 failed to report alive state` ten seconds
later.

The one rule for callers: **an AP must be left in the reset state WHP created it
in.** `machine_x86::boot::setup_long_mode_sregs` is for the bootstrap processor
only. The KVM path hands it to every vCPU because KVM's INIT discards it; WHP has
no INIT of its own to discard anything.

### Diagnosing a stalled vCPU

Both of these came out of the SMP work and are the first two things to reach for:

- `WhpPartition::processor_summary(index)` — `rip`, `rsp`, `cs`, `cr0`, `cr3`,
  `cr4`, `efer` and the decoded mode (real/protected/long) for any VP, from any
  thread. It is what showed VP 1 holding exactly the `CS` the host had written
  with `RIP` still 0.
- `$ENTANGLED_WHP_TRACE_EXITS=1` — every exit reason and RIP, one line each. One
  `Canceled` for a whole boot means WHP is blocking inside the run call, which no
  serial log can tell you.

## Phase 4: what it delivered, and what remains

All five phase-4 items are done:

1. **The run path** — `apps/entangled/src/run_vm.rs` has one shared body and a
   per-OS `host` module; `entangled run` (windowed and `--headless`) works
   natively, ends via ACPI S5 or Ctrl+C (`SetConsoleCtrlHandler`), and the
   manager needs nothing new — it already drives one CLI process per VM, which
   is what the one-mapped-partition-per-process limit demands. **Register setup
   is BSP-only on WHP in every boot mode** (direct-Linux *and* PVH): the AP rule
   from phase 3 applies to `setup_pvh_sregs` too.
2. **usernet in a boot** — `[network] backend = "usernet"` (control-api), wired
   in `run_vm` with `static_ip_cmdline()` appended for direct-Linux guests (the
   bootstrap kernel carries `CONFIG_IP_PNP`, so `ip=` is the whole client). The
   in-guest acceptance is `entangled.netprobe=<ip>/<prefix>,<gw>,<host>:<port>`
   in the test initramfs: static ioctl config, TCP out through the NAT, echo
   verified byte for byte (`--test whp_usernet`).
3. **virtio-pci + MSI-X** — see [virtio on Windows](#virtio-on-windows).
4. **UEFI** — worked through the same seams once two real bugs fell (see the
   traps below: the emulator's RAM operands, and `map_rom`). CloudHv boots to
   the Boot Manager and the NVRAM persistence numbers match the KVM run
   (`--test whp_uefi`). Reset-vector images get `WhpPartition::map_rom`
   (read+execute mapping; guest writes fault into the bus and are dropped, the
   same semantics as `KVM_MEM_READONLY`).
5. **CI (WHP-1705)** — a `windows-latest` job builds, clippys and tests the
   whole workspace, and asserts that `whp_boot` *self-skips with its hint*
   rather than failing on a runner without WHP.

Still open after phase 4:

- **guest → host file of record for reboots:** `reboot=k`'s triple fault ends a
  KVM VM (`KVM_EXIT_SHUTDOWN`) but not a WHP one — see the traps below. Guests
  should power off via ACPI S5 on both hosts; a real reboot (restart the same
  VM) is unimplemented on both.
- **DHCP in the test guest** — the netprobe configures itself statically;
  `ip=dhcp` against the usernet DHCP server would exercise that server from a
  real kernel (it is unit-tested today).
- **The doorbell optimisation** for synchronous kicks, if a measurement ever
  demands it.

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

## Pause and reset on WHP (ADR-0005)

**Resetting a virtual processor is `WHvDeleteVirtualProcessor` +
`WHvCreateVirtualProcessor`.** There is no reset call, but there is something
better: a VP that `WHvCreateVirtualProcessor` just made *is* in the
architectural reset state, including the parts no public register exposes — the
local APIC, and an application processor's wait-for-startup suspension. That
last one is the whole reason the KVM approach (write the state back by hand)
cannot be used here: this skill's SMP section records that an AP must be left
exactly as WHP created it or the guest's INIT/SIPI never makes it runnable, and
re-creating it is the only way back to that state after a boot has used it.
Safe only at a lifecycle checkpoint, where nothing is inside
`WHvRunVirtualProcessor` for that index, and it runs on the owning thread.
Measured: **7-8 ms** for a full machine reset, against KVM's 62-66 ms.

**`reboot=k` now works, and this is the host where that matters most.** Phase 4
recorded that a triple fault is absorbed by WHP with local APIC emulation on and
the vCPU simply parks, so guest-initiated shutdown had to be the ACPI S5 path.
That is still true of the *triple fault* — but a guest never starts there. It
walks a ladder (ACPI reset register, then the keyboard controller, then 0xCF9,
then the triple fault), and `machine_x86::reset` implements every earlier rung.
So `reboot=k` pulses port 0x64, the machine latches it, and the VM reboots. The
profiles and tests that avoid `reboot=k` for the old reason can stop.

**No pending-exit flush is needed here.** This backend completes every exit
before the run call returns — it advances RIP itself, the hypervisor does not —
so the top of the run loop is already a clean stop point. The KVM loop's
`immediate_exit` dance has no WHP equivalent because it has no WHP problem.

**A canceller *is* the kick.** `VcpuCanceller` implements
`vmm_core::lifecycle::VcpuKick` directly: it already wakes a halted vCPU through
the partition's halt gate as well as cancelling a run, which is exactly what a
barrier needs from both states.

Acceptance: `cargo test -p vmm-core --test whp_lifecycle` — pause (95 µs to
acknowledge), resume, host reset twice, and a guest-initiated reboot. And
end-to-end, on this host: `cargo test -p entangled --test guest_reboot --
--ignored` reboots an installed Ubuntu twice through its own firmware in 414 s,
each one arriving as `0xcf9 cold reset` and coming back through
`BdsDxe: starting Boot0006 "Ubuntu"`.
