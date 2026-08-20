# ADR-0005: VM lifecycle — the controlled stop, and reboot in place

- Status: accepted
- Date: 2026-08-20
- Extends: [ADR-0001](0001-mvp-architecture.md), [ADR-0002](0002-linux-first-whp-ready.md)
- Related: [ADR-0003](0003-uefi-firmware.md) (the firmware a reboot re-enters)

## Context

Two gaps, one mechanism.

**A guest could not reboot in place.** Install updates in an Ubuntu guest, click
Restart, and the VM died. On KVM the guest's triple fault reached the run loop
as `KVM_EXIT_SHUTDOWN` and ended it; on WHP with local APIC emulation the reset
was absorbed by the hypervisor and the vCPU parked for ever, so even that
ending did not arrive (ADR-0002 phase 4 recorded this as "`reboot=k` is not a
way to end a WHP VM"). For a desktop VMM that is not a missing feature, it is a
broken one: rebooting is the second thing anybody does to a machine.

**There was no pause.** No way to freeze a VM and let it continue.

Both need the same thing: bring every vCPU to a stop point that is *safe* — out
of `KVM_RUN` / `WHvRunVirtualProcessor`, between two exits, with no device
half-way through serving one — get an acknowledgement from each, stop the host
threads that touch guest memory without a vCPU behind them, and then either hold
there or put the machine back to power-on and start it again.

That seam is also the foundation of suspend/restore, which is why it is built as
a first-class mechanism rather than as two special cases. What it still needs to
get there is the last section of this ADR.

## Decision

### 1. The stop protocol

`vmm_core::lifecycle` is portable. It is a rendezvous between the thread that
asks for something and the vCPU threads that have to be somewhere safe first:

```text
  requester                          vCPU thread i
  ---------                          -------------
  phase := Park{Pause|Reset}         .. inside KVM_RUN / WHvRunVirtualProcessor
  attention := true
  kick(i) for all i, repeatedly  ->  run call returns Interrupted/Canceled
                                     loop top: flush any pending exit
                                               checkpoint()
                                                 parked += 1; notify
                                                 wait until phase == Run | Stop
  wait until parked == live vCPUs
  -- every vCPU is now OUT of the guest, between two exits --
  quiesce()                          (host device workers stop too)
  ... hold (Pause) ...
  ... or: reset_machine()            devices + guest memory, once
      phase := ResetVcpus        ->  reset_arch_state(); reset_vcpu(i)
  wait until every vCPU reported
  unquiesce(); phase := Run      ->  back into the guest
```

**Why it is safe.** The checkpoint is at the *top* of the run loop: the previous
exit has been fully dispatched and the next entry has not begun. No device is
part-way through an MMIO access, no descriptor chain is half-walked, no
transport lock is held. The one piece of hypervisor-side state that does survive
between an exit and the next entry is KVM's pending userspace-I/O completion,
and it is retired explicitly before parking — KVM applies it at the top of the
next `KVM_RUN`, *ahead* of the `immediate_exit` check, so one run with that flag
set flushes it and comes straight back. Left outstanding, it would land a stale
`IN` result in a register of a freshly rebooted guest. WHP needs no equivalent:
that backend completes every exit itself (it advances RIP; the hypervisor does
not), so the top of its loop is already clean.

**Why the requester coordinates rather than a leader vCPU.** The machine-wide
half of a reset — devices to power-on, boot images reloaded — runs on the thread
that asked for it, while every vCPU waits. The per-vCPU half runs on each vCPU's
own thread, because KVM requires vCPU ioctls to come from the thread owning the
fd. So a guest-initiated reboot is *latched* by the vCPU that saw it
(`Lifecycle::request_guest_reset`) and *driven* by the supervisor, which is the
same thread that serves a reset asked for from the window.

**The additive shape (ADR-0002).** Two small per-backend traits — `VcpuKick`
(get a vCPU out of the run call) and `ResettableVcpu` (put one back to power-on)
— plus a `MachineLifecycle` the machine above implements. `spawn_vcpus_with` and
`run_loop_with` sit beside the existing entry points; a caller that passes no
lifecycle behaves exactly as before, which is what keeps the boot-test harness's
"the guest's reboot ends the run" contract intact. Neither backend's public API
changed shape for the other.

`VmState` gains `Paused` and `Resetting` honestly, with the transitions
`Running <-> Paused`, `Running|Paused -> Resetting -> Running`, and `Stopping`
reachable from all of them.

### 2. The reset matrix

A guest never asks its VMM to reboot. It pokes hardware, in a **ladder**, moving
down a rung each time the machine fails to restart — Linux's
`native_machine_emergency_restart()` walks `BOOT_ACPI -> BOOT_KBD -> BOOT_EFI ->
BOOT_CF9_FORCE -> BOOT_TRIPLE`, and EDK2's `ResetSystemLib` for this machine's
host bridge does 0xCF9 first and the keyboard controller second. Implementing
one rung would still reboot, but only after the guest had spent its way down to
it — and on WHP the bottom rung never arrives at all. So the whole ladder is
implemented, in `machine_x86::reset`:

| Mechanism | Where | Who takes it | This machine |
|---|---|---|---|
| ACPI `RESET_REG` | I/O 0xCF9, value 0x0E, named by the FADT | Linux `acpi_reboot()` — the **first** rung | reset |
| 0xCF9 reset control | `PORT_RESET_CONTROL` | Linux `BOOT_CF9_SAFE`/`_FORCE`; EDK2 `ResetCold`/`ResetWarm`, so every UEFI guest's `reboot` | reset (cold if `FULL_RST`, warm otherwise — no difference here) |
| keyboard controller pulse | write `0xFE` to port 0x64 | Linux `BOOT_KBD`, which `reboot=k` selects outright; EDK2's fallback | reset |
| triple fault | not a device: `KVM_EXIT_SHUTDOWN` | Linux `BOOT_TRIPLE` | **boot CPU** → reset; **AP** → park, machine keeps running; **KVM only** — WHP absorbs it |
| ACPI S5 (`SLP_TYP=5`) | the PM block, since EPIC 18 | `poweroff` | power off, unchanged |

Three decisions inside that table are worth stating outright.

**`FADT.RESET_REG_SUP` is now set**, pointing at 0xCF9/0x0E — the same pair
QEMU's q35 publishes. Before, it was clear and `acpi_reboot()` was a silent
no-op. Naming it is what makes a Linux reboot land on the first attempt instead
of the fourth.

**Port 0x64 is claimed for writes only.** This machine has no PS/2 controller
and the FADT's `IAPC_BOOT_ARCH` does not claim one; answering *reads* there would
invite a guest to go looking for a controller that does not exist. 0xCF9 is
claimed for reads too, because Linux read-modify-writes it — and the `RST_CPU`
strobe reads back clear, as on real hardware, so that read-modify-write cannot
reset the machine by accident.

**An application processor's triple fault does not restart the machine.** EDK2's
`MpInitLib` wakes each AP with INIT/SIPI and carries on with however many answer
("`MpInitLib: Find 1 processors in system`"), so a machine whose AP faults during
firmware startup is one the firmware expects to keep running. Rebooting on it
would turn a rare race into a reboot loop; *ending* the VM — which is what the
backend used to do, because any finished vCPU stops the supervisor — throws away
a boot that was going to succeed. So a faulted AP parks: it never re-enters the
guest (KVM answers the next `KVM_RUN` with `KVM_EXIT_INTERNAL_ERROR`), it does
not end its thread, and a later machine reset brings it back. This was found by
the UEFI acceptance test on its second run, on a boot that had worked the first
time.

### 3. What "quiesced" means, per device class

Parking the vCPUs stops the *guest*. It does not stop the host. "Paused" has to
mean **nothing writes guest memory**, or a pause is not a point a snapshot could
ever be taken at, and a resumed guest finds descriptors it never posted.

| Class | While paused | On reset |
|---|---|---|
| vCPU threads | parked at the lifecycle checkpoint | arch state (KVM: events, LAPIC page, regs, `mp_state`; WHP: VP deleted and re-created), then the boot state the first boot used |
| ioeventfd queue workers (KVM) | park on `virtio_core::Quiesce` **before** taking the transport lock | keep running — host wiring, not guest state; their addresses are re-based (below) |
| virtio-net receive thread | parks on the same gate at the top of its loop | stopped and re-created by the device's own `reset()` |
| virtio devices + queues | reached only through a parked vCPU or a parked worker | `TransportState::power_on_reset`: device `reset()`, queues, features, status, ISR — **and** `config_generation` and the MSI-X table, which a *device* reset deliberately keeps |
| virtio-pci config space | not touched | restored from a power-on snapshot taken when the function was attached; then the notify ioeventfds re-based around the restored BARs |
| virtio-mmio window | not touched | just the transport: a fixed host-chosen window has no guest-movable BAR and nothing to re-base |
| 8259 / 8254 / IOAPIC (WHP) | the PIT's timer thread stops delivering; resuming re-arms from the current time | IOAPIC masked **first**, then PIT and PIC to power-on |
| in-kernel irqchip + PIT (KVM) | nothing to do — they only fire into a parked vCPU's LAPIC, which the LAPIC reset then clears | reset with the vCPU's LAPIC page |
| 16550 serial | nothing writes guest memory | register file to power-on, receive queue dropped; the output sink and interrupt line are host wiring and survive |
| ACPI PM block | its timer freezes and resumes where it stopped | registers cleared, timer restarted from zero, **shutdown latch cleared** |
| RTC / host-bridge stub | derived from the host clock; not frozen (see the gaps) | index latch and CMOS to power-on |
| pflash (UEFI NVRAM) | not touched | **only the CFI command state machine**. The contents are the non-volatile variable store, and a UEFI VM boots the `Boot####` entry that lives in it |
| virtio-gpu / renderer threads | fence completions wake through the (gated) queue worker; the display reads the host scanout, never guest memory | with the device |
| user-mode NAT threads | host sockets only; frames sit in a bounded host queue until the gated receive worker moves them | backend kept; the device's rings are reset |

Two details of the gate are load-bearing rather than decorative.

**A worker holds a `Pass` for the work, not just for the check.** Closing the
gate stops work that has *not started*; a pause that returned while a receive
worker was half-way through writing a frame into the RX ring would not be a
point anything could be snapshotted at. So `wait_while_paused` hands back an
RAII pass, and `quiesce()` waits — bounded, `QUIESCE_SETTLE` — for every pass to
be dropped. A worker that is stuck (a host disk that has stopped answering)
cannot wedge a pause: the wait times out, says so, and the pause proceeds,
because a VM that can never be frozen because one device is unwell is worse.

**Each waiter brings its own liveness predicate**, and the shutting-down side
pairs its flag with `Quiesce::wake()`. A device reset runs on a quiesced VM,
virtio-net's reset *joins* its receive worker, and a gate that only ever opened
on resume would deadlock exactly that.

The reset order is the machine's dependency order: interrupt sources first (they
are what could deliver into a CPU with no IDT yet), then devices, then the boot
images — `linux_boot::load` writes `boot_params` over the low memory the previous
guest was using — then the latches that said a reset was wanted.

### 4. Reset re-runs the boot path

`entangled run` describes what a VM needs in guest memory as a `BootPlan`, and
one `load_boot` + `apply_boot_state` pair serves both the first boot and every
reset. A reboot that loaded the kernel through a second, reset-only path would
be a second thing to keep correct and the first thing to rot.

It is also what makes the UEFI case need no special handling. "Reload the PVH
firmware and re-enter it" *is* the boot path run again — whereupon EDK2 re-reads
its variable store out of pflash (which the reset leaves alone), and its boot
manager starts the same entry. A reset-vector ROM needs even less: the mapping is
a host slot the guest cannot change, and the architectural reset state the vCPU
is put back into already points its first fetch at it.

### 5. Bounds, because the guest is untrusted

Every reset path here is guest-triggerable.

- A guest that reboots faster than it can boot is **stopped, not served**:
  `RESET_STORM_WINDOW` (1 s) and `MAX_RESET_STORM` (5) turn a fault loop into an
  ending. Nothing legitimate reboots twice inside a second.
- A vCPU that asked for a reset waits a bounded `RESET_WAIT` (30 s) for a
  supervisor to serve it, then ends the VM rather than hanging the host.
- Every barrier has an `ACK_TIMEOUT` (10 s), after which the VM is put back the
  way it was found rather than leaving half the vCPUs parked.
- A failed machine reset does **not** resume: re-entering a half-reset machine is
  worse than stopping.
- The reset registers decode one byte at a constant port, latch a `bool`, and
  cannot allocate, block or panic.

### 6. How it is reached

| Surface | Pause | Reset |
|---|---|---|
| the VM window | `Ctrl+Alt+P` | `Ctrl+Alt+R` |
| `entangled run --control-stdin` | `pause` / `resume` | `reset` |
| `entangled-manager` | Pause/Resume on a running card | Restart |
| the guest itself | — | the reset matrix above |

The window bindings join `Ctrl+Alt+G`/`Q`/`O` and `F11`; both are consumed by the
host with the modifiers handed back first, so a frozen guest is not left holding
Ctrl+Alt. The control channel is a pipe rather than a socket because it is the
one channel that exists identically on both hosts, needs no path, no permissions
and no cleanup, and dies with the process it controls — which for "pause this
VM" is exactly the lifetime wanted. It also carries `type <text>`, the same
host-to-guest serial path the unattended installer already uses to drive GRUB:
a headless VM whose console can be watched but never answered is half a console,
and it is how the reboot acceptance logs in and asks the guest to restart.

The manager tracks what it *asked for*, not what the VM is: a VM can also be
frozen from its own window, and the manager would not know. Its buttons are
disabled for a VM it did not start, because a control channel is a pipe to a
child.

## Measured

Debug builds, on the development machine.

| | KVM (WSL Ubuntu) | WHP (native Windows) |
|---|---|---|
| pause acknowledged | 180 µs | 97 µs |
| full in-place reset | 80 ms | 6.7 ms |
| repeated guest reboots | 42 in 90 s (`examples/boot-test.toml`), no leaked thread or fd | — |

WHP's reset is an order of magnitude faster because deleting and re-creating a
virtual processor is cheaper than writing an architectural reset state back
register by register — and it is also more complete, which is the happier half
of that trade.

Acceptance, both hosts: `tests/boot/tests/lifecycle.rs` and
`crates/vmm-core/tests/whp_lifecycle.rs` assert pause (the guest's heartbeat
probe goes silent), resume (the same boot continues — the ready marker is not
printed again), host reset (it is, twice) and a guest-initiated reboot. The
guest-reboot half is what could not have worked on WHP before: `reboot=k` pulses
port 0x64, an *earlier* rung than the triple fault the hypervisor absorbs.

`apps/entangled/tests/guest_reboot.rs` is the end-to-end one: an installed
Ubuntu, logged into over its serial console, running `sudo reboot` — twice —
and counting EDK2 boot-manager runs to prove the firmware ran again off a
variable store the reset did not clear. It passes on KVM in **467 s** for three
boots, and the console says the whole chain in five lines:

```text
[  133.402652] reboot: Restarting system
machine_x86::reset: guest requested a machine reset via="0xcf9 cold reset (also the ACPI reset register)"
entangled::run_vm::host_api: machine reset: devices at power-on, tables and boot images reloaded entry=0x4fffd0 kind=Pvh
vmm_core::lifecycle: VM reset complete; guest restarted resets=1
BdsDxe: starting Boot0006 "Ubuntu" from HD(1,GPT,33AD6DCE-...)/EFI/ubuntu/shimx64.efi
```

That is the guest's `reboot` going out through EFI `ResetSystem` to 0xCF9, the
machine coming back **19 ms** later with the PVH firmware reloaded, and the
firmware finding `Boot0006` in the NVRAM the reset left alone. The second
reboot is the same five lines with `resets=2`, and a third login prompt follows
it. A reset that had wiped the variable store would have booted to the EFI
shell instead, which is why the boot-manager count is the assertion and not the
login count.

## What this still needs to become suspend/restore

The seam stops the machine and puts it back to power-on. Suspend needs it to
stop the machine and write down *what it was*. What is missing, in the order it
would have to be built:

1. **Full CPU state.** `ResettableVcpu` writes state; nothing reads it. A
   snapshot needs, per vCPU: the general and special registers (already
   neutral), **MSRs** (`KVM_GET_MSRS` / `WHvGetVirtualProcessorRegisters` over an
   MSR name list — `EFER`, `STAR`/`LSTAR`/`CSTAR`/`SFMASK`, `KERNEL_GS_BASE`,
   `TSC`, `TSC_AUX`, `IA32_APIC_BASE`, the PAT and the SYSENTER trio at minimum),
   **XSAVE/XCRS** (`KVM_GET_XSAVE`+`KVM_GET_XCRS`; WHP exposes the same through
   its FP/XMM register names), the **local APIC** page, `mp_state`, and pending
   `vcpu_events`. Most of these have no home in `vmm_core::hv` yet, and inventing
   that home is the real work: the register structs are deliberately ours
   (ADR-0002), so an `X86CpuState` has to be a neutral type both backends can
   fill without either one's vocabulary leaking.
2. **Time.** TSC and the guest clock. A restored guest that finds the TSC has
   jumped by however long the snapshot sat on disk behaves badly; KVM has
   `KVM_GET_CLOCK`/`KVM_SET_CLOCK` and TSC offsets for this. The PM timer and
   the PIT already know how to be frozen and resumed (pause needs it), which is
   the same problem one step smaller.
3. **Device state serialization.** Every `reset()` in the table above has to gain
   a matching *save* and *load*. The shape is already right — the state is
   reachable, per device, from the machine — but it is a per-device format, and
   `virtio_core::state::TransportState` is where most of it lives (negotiated
   features, per-queue geometry and the driver's `avail`/`used` indices, the
   status word, MSI-X table). The device-owned halves are the harder ones: an
   open disk file's identity, the GPU's host resources, the NAT's live flows.
4. **Guest memory.** The largest and the easiest: it is one `GuestMem`, and
   pause already guarantees nothing is writing it. Dirty-page tracking
   (`KVM_GET_DIRTY_LOG`, `MEM_WRITE_WATCH` on Windows) is what turns a 4 GiB
   write into an incremental one — the alias in `vmm_core::memory` exists for
   exactly that divergence (ADR-0002).
5. **A versioned snapshot format.** Explicitly not "serde on whatever structs
   exist": a snapshot outlives the build that wrote it, and refusing to restore
   an incompatible one has to be a *checked* refusal, not a crash. It needs a
   magic, a format version, the machine shape (vCPU count, memory size,
   transport, device list and order — a snapshot restored onto a different
   device order is a corrupted guest), and a per-device version.
6. **The lifecycle states.** `VmState` would gain `Suspending`/`Suspended`, and
   `MachineLifecycle` a `save`/`load` pair beside `reset_machine` — the same
   "runs once with every vCPU parked" contract, which is why the pause half of
   this ADR is the foundation rather than a neighbour.

Two smaller debts this ADR leaves behind, worth clearing on the way:

- **The RTC is not frozen by a pause.** A resumed guest sees wall-clock time
  jump. Correct for a wall clock and wrong for a suspended machine, and the
  right answer differs per use — it needs a policy, not a patch.
- **A paused VM still holds its host resources**: disk files open, NAT sockets
  bound, the renderer process alive. Fine for a pause, and precisely what a
  suspend must undo.

## Consequences

- A guest's `reboot` reboots, on both hosts, in both boot modes, and a UEFI VM
  comes back through its own firmware and NVRAM.
- The boot-test harness's guests still end their runs by rebooting, because the
  seam is opt-in per boot. A test that wants the new behaviour asks for it.
- One more thing every new device owes the machine: a `reset()`, and — if it runs
  a thread of its own that touches guest memory — a `Quiesce` gate before it does.
  Both are one line in the table above; a device that skips them makes "paused"
  a lie and a reboot a haunting.
- `entangled run` grew a control channel. It is a small surface (five commands)
  and it is what makes the manager's buttons real rather than a stop and a start.
