---
name: whp-backend
description: Implementing the native Windows host backend of Entangled Desktop on WHP (Windows Hypervisor Platform) — partition and vCPU lifecycle, WHvMapGpaRange, register mapping, run-loop exit translation, and the userspace PIC/IOAPIC/PIT that WHP does not provide (backlog EPIC 17, crate vmm-core, module `whp`). Load before any work touching the `windows` crate or WHv* APIs.
---

# WHP backend

Scope: backlog EPIC 17 (WHP-1701…1705), ADR-0002. Code lives in
`crates/vmm-core/src/whp/` (`partition.rs`, `vcpu.rs`, `regs.rs`) with tests in
`crates/vmm-core/tests/whp_smoke.rs`. Everything is behind `#[cfg(windows)]`;
the `windows` crate is declared only under
`[target.'cfg(windows)'.dependencies]`.

## Status: phase 1 is done

Working and verified on hardware: partition lifecycle, guest memory mapping,
vCPU creation, register access through `vmm_core::hv::VcpuRegisters`, the
`WHvRunVirtualProcessor` loop with port-I/O emulation, and stop via
`WHvCancelRunVirtualProcessor`. A real-mode guest executes machine code, writes
to an I/O port and terminates; 100 create/destroy cycles leak nothing.

Not done — see [Phase 2](#phase-2-what-is-still-missing).

## Build and test

```powershell
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\entangled-target-whp"
cargo test -p vmm-core                          # 18 unit + 7 WHP smoke
cargo clippy -p vmm-core --all-targets -- -D warnings
```

Host x86_64-pc-windows-gnu works; no MSVC toolchain needed. Linux must stay
green in the same change — run the WSL regression too (see CLAUDE.md).

**Do not run `cargo build --workspace` on Windows and conclude the port is
broken.** `machine-x86`, `linux-boot`, `control-api` and `debian-media` build
natively; the `virtio-*` crates and `display` do not, because upstream
`virtio-queue` 0.17 depends on `vm-memory` without `default-features = false`
and so re-enables the unix-only `rawfd` feature for the whole graph. Cargo
features are additive — no manifest change here can subtract it. Unblocking it
(upstream PR, vendored crate, or `[patch.crates-io]`) is a prerequisite for
WHP-1703 onward.

WHP needs the **"Windows Hypervisor Platform"** optional feature (admin +
reboot; `dism /Online /Enable-Feature /FeatureName:HypervisorPlatform`). It
coexists with WSL2 — both ride Hyper-V. Without it every test self-skips with
that hint; `WhpHypervisor::probe()` reports it instead of failing, the way
`Hypervisor::probe()` does for `/dev/kvm`.

## Existing pieces — build on these, don't duplicate

- `vmm_core::hv` holds the neutral types: `X86Registers`, `X86SpecialRegisters`,
  `X86Segment`, `MachineConfig`, `RunOutcome`, `ExitHandler`, `VcpuRegisters`.
  Machine code (`machine-x86`) goes through these and must never see a
  `WHV_*` type, exactly as it never sees a `kvm_bindings` one.
- `vmm_core::create_guest_memory` / `GuestMem` are **portable**. `vm-memory`'s
  mmap backend is VirtualAlloc-backed on Windows; only its default `rawfd`
  feature is unix-only, and the workspace manifest keeps that off. Do not write
  a second guest-memory type.
- `whp::regs` owns every name/value table and the bit packing. Add registers
  there, keeping `*_NAMES` index-for-index in sync with
  `*_values`/`*_from_values`; the unit tests pin the order.
- `whp::partition::whp_err` turns an `HRESULT` into
  `VmmError::Whp { call, message }`. Never let a WHP failure surface as a bare
  hex code.

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

## Exit translation

| `WHV_RUN_VP_EXIT_REASON` | Backend behaviour |
|---|---|
| `X64Halt` | `RunOutcome::Halted` — WHP really does surface `hlt`, unlike KVM with an in-kernel irqchip |
| `X64IoPortAccess` | `ExitHandler::io_out`/`io_in`, then RIP += instruction length, and RAX write-back for `IN` |
| `MemoryAccess` | decoded and reported as `VmmError::WhpUnsupportedExit` (phase 2) |
| `UnrecoverableException`, `InvalidVpRegisterValue` | `RunOutcome::Shutdown` — the triple-fault equivalent |
| `Canceled`, `None` | re-check the stop flag, then re-enter or `RunOutcome::Stopped` |
| anything else | `VmmError::Vcpu` naming the reason and RIP |

Two things KVM does for us that **WHP does not**:

1. **RIP is never advanced.** Every emulated instruction must add
   `VpContext.InstructionLength` (the low nibble of `VpContext._bitfield`) to
   `VpContext.Rip` and write RIP back.
2. **Nothing is decoded.** The port-I/O exit does carry port, direction and
   access size; the MMIO exit carries only the GPA and the raw instruction
   bytes.

## Traps that cost real debugging time

- **16-byte alignment is mandatory.** `WHV_UINT128` is `DECLSPEC_ALIGN(16)` in
  the header, but the generated binding drops it, leaving
  `WHV_REGISTER_VALUE` at alignment 8. A bare `[WHV_REGISTER_VALUE; N]` local
  lands on an 8-mod-16 address roughly half the time and WHP faults with
  `STATUS_ACCESS_VIOLATION` *inside* the call — no error, no backtrace, just a
  dead process. Always use `regs::Aligned16`; `WhpVcpu::batch_count` rejects a
  misaligned buffer with a typed error rather than trusting the caller.
- **One mapped partition per process.** A second concurrent partition is
  created fine, but its first `WHvMapGpaRange` fails with `0xC0370008`
  ("another partition with the same name already exists"). Sequential
  create/destroy is unaffected. Consequence: on Windows a multi-VM `entangled`
  needs one process per VM, and concurrent WHP tests must serialise
  (`whp_guard()` in `whp_smoke.rs`).
- **`apic_base` is not readable until APIC emulation is on**, and asking for it
  inside a batched call fails the whole batch. Hence the separate call.
- **Ordering is strict.** Partition properties only before
  `WHvSetupPartition`; GPA ranges and virtual processors only after.
- **Teardown order matters.** `WHvDeletePartition` must run before the guest
  RAM is freed, and every VP must be deleted before its partition.
  `Partition` owning `GuestMem` plus `Arc<Partition>` in each `WhpVcpu` makes
  both orderings structural rather than a convention to remember.
- **The `windows` crate constants are `WHV_*(i32)` newtypes**, so matching on
  them needs `#[allow(non_upper_case_globals)]` (CI runs `-D warnings`).

## Phase 2: what is still missing

In dependency order:

0. **Unblock `virtio-queue` on Windows** (see the build note above) — nothing
   below can be tested without it.
1. **Userspace PIC/IOAPIC/PIT (WHP-1703, the real gap).** KVM gives us an
   in-kernel PIC, IOAPIC and PIT (`create_irq_chip`, `create_pit2` in
   `vmm-core/src/vm.rs`); WHP provides only the local APIC, so all three must
   be emulated in userspace, as QEMU/WHPX does. Set
   `WHvPartitionPropertyCodeLocalApicEmulationMode` and route through
   `WHvRequestInterrupt`. `virtio_core::Interrupt`/`IrqLine` already abstract
   delivery, so device crates need no change — this is the one piece with no
   KVM analogue to copy.
2. **MMIO via the instruction emulator.** `WHvEmulatorCreateEmulator` +
   `WHvEmulatorTryMmioEmulation` (WinHvEmulation.dll, already exposed by the
   `windows` crate) decode the faulting instruction and call back for memory
   and register access; those callbacks map onto `ExitHandler::mmio_read`/
   `mmio_write` directly. `WHvEmulatorTryIoEmulation` covers string/`REP`
   port I/O at the same time. Until this lands, virtio-mmio devices cannot
   work under WHP.
3. **Serial wiring.** `vm-superio` and the serial trigger are OS-agnostic; the
   port-I/O path already reaches `ExitHandler`, so this is bus plumbing plus an
   interrupt line from step 1.
4. **Boot path.** `machine-x86` and `linux-boot` already write registers only
   through `VcpuRegisters`, so long-mode setup should work as-is; what is
   missing is CPUID policy (WHP wants
   `WHvPartitionPropertyCodeCpuidExitList`/`CpuidResultList` rather than KVM's
   `set_cpuid2`) and an `entangled doctor` arm for WHP capabilities.
5. **Networking (WHP-1704).** No TAP on Windows, and wintun/tap-windows6 are
   GPL (blocked by `cargo deny`). Plan: user-mode NAT on `smoltcp` (0BSD),
   which also buys rootless networking on Linux.
6. **CI (WHP-1705).** A `windows-latest` matrix job can build, clippy and run
   the non-WHP tests; GitHub's runners have no nested virtualisation, so the
   WHP tests will self-skip there — which is exactly why they self-skip rather
   than fail.

## Hard rules that apply here

- Every `unsafe` block gets a `// SAFETY:` comment (`undocumented_unsafe_blocks`
  is `deny`). WHP FFI is the densest `unsafe` in the tree — say *why* the
  pointer is valid and which union arm is live, not that the call is an FFI
  call.
- No `unwrap`/`expect` on a runtime path; typed `VmmError` variants only.
- Guest-controlled values (exit contexts, register contents) never index host
  memory directly — go through `vm-memory`'s checked APIs.
- Keep the Linux API untouched. WHP additions are new types under
  `whp::`, never changes to `Vcpu`/`Vm` signatures.
