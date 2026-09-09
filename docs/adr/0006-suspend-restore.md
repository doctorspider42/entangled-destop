# ADR-0006: Suspend and restore — writing a VM down, and reading it back

- Status: accepted
- Date: 2026-08-21
- Extends: [ADR-0005](0005-vm-lifecycle.md) (the controlled stop this is built on)
- Related: [ADR-0002](0002-linux-first-whp-ready.md) (why the CPU state is ours),
  [ADR-0003](0003-uefi-firmware.md) (what a restored UEFI machine still owes its
  firmware), [ADR-0004](0004-virtio-gpu-3d.md) (why the 3D half cannot come back)

## Context

ADR-0005 built the controlled stop: every vCPU parked at a safe point between
two exits, every host worker quiesced, nothing writing guest memory. It ended
with a list of what suspend would still need. This is that list, built.

The thing being asked for is small to describe and unforgiving to get wrong:
*close the lid, open it again, be where you left off.* A VM that resumes and
then dies twenty minutes later in an unrelated place is worse than one that
never resumed, because the failure has no path back to its cause. Almost
everything in this ADR is a decision about **how not to lose one piece of
state**, and the recurring theme is that the pieces which are easiest to forget
are the ones whose absence is invisible.

## Decision

### 1. Full CPU state, as neutral types

`vmm_core::hv` grows an `X86CpuState`: the general and special registers it
already had, plus **MSRs**, `XCR0` and the XSAVE area, the local APIC,
`mp_state`, pending events and the debug registers. Ours, per ADR-0002 — a
`kvm_bindings` or `WHV_*` type outside `vmm-core` is still a bug.

**The MSR list is not written down.** A snapshot that forgets one MSR is not
slightly wrong; it is a guest that dies somewhere else. Lose `KERNEL_GS_BASE`
and the next `swapgs` in a syscall entry lands the kernel on a null per-CPU
base. Lose `LSTAR` and the first `syscall` after resume jumps to zero. Lose
`PAT` and the framebuffer becomes uncacheable. None of those point back here.
So each backend asks its hypervisor:

| | KVM | WHP |
|---|---|---|
| MSRs | `KVM_GET_MSR_INDEX_LIST` — everything the kernel says it can save (~100 on a 6.x kernel), minus the x2APIC window, read in batches with the ones this vCPU refuses dropped one at a time | the `WHvX64Register*` name space *is* the enumeration; 30 names mapped to architectural indices, probed the same way |
| XSAVE | `KVM_GET_XSAVE` (the fixed 4 KiB area) | `WHvGetVirtualProcessorXsaveState` |
| Local APIC | `KVM_GET_LAPIC` (the architectural 1 KiB page) | `WHvGetVirtualProcessorInterruptControllerState` |
| `mp_state` | `KVM_GET_MP_STATE` | `WHvRegisterInternalActivityState`'s startup-suspend bit |
| Pending events | `KVM_GET_VCPU_EVENTS` | `WHvRegisterPendingInterruption` + `WHvRegisterInterruptState` |
| Debug registers | `KVM_GET_DEBUGREGS` | `WHvX64RegisterDr0..7` |
| Guest clock | `KVM_GET_CLOCK` | — (a WHP partition has no paravirtual clock) |
| In-kernel chips | `KVM_GET_IRQCHIP` ×3 + `KVM_GET_PIT2` | — (they are in this process; see §3) |

Two of those are **opaque blobs tagged with the host that produced them**, and
that is honest rather than lazy: KVM's local-APIC page and WHP's
interrupt-controller state describe the same hardware in formats neither one
accepts from the other, and inventing a third would mean re-deriving one from
the other on every save. The tag is a second lock on the same door as the file
header's host field — even a snapshot whose header was edited cannot feed WHP's
blob to `KVM_SET_LAPIC`.

**Order matters on the way back in**, and differs per host, which is why each
backend documents its own:

- KVM: `mp_state` → local APIC → sregs/regs → `XCR0` → XSAVE → MSRs → debug
  registers → pending events. An application processor must be put back into
  its wait before anything else touches it; `IA32_TSC_DEADLINE` is meaningless
  until the APIC timer it arms exists; `XCR0` decides which components the XSAVE
  area may carry; pending events go last because everything above could clear
  them.
- WHP: local APIC → sregs/regs → `XCR0` → XSAVE → MSRs → debug registers →
  activity state → pending events.

#### What is deliberately skipped

| Skipped | Why |
|---|---|
| `KVM_GET_XSAVE2`'s dynamic components (AMX tile data) | The fixed 4 KiB area covers everything up to AVX-512. This machine's CPUID offers no AMX, so there is nothing to lose today — and a snapshot whose XSAVE header *claims* a dynamic component is refused rather than truncated. |
| Nested state (`KVM_GET_NESTED_STATE`) | The CPUID policy exposes no VMX or SVM; there is no L2. |
| The x2APIC MSR window (`0x800..=0x8ff`) on KVM | It is the same local APIC the `KVM_GET_LAPIC` page carries. Restoring both would have the two fight. |
| `IA32_MISC_ENABLE` on WHP | WHP has no register name for it. KVM does, and carries it. |
| A pending exception *event* on WHP (`WHvRegisterPendingEvent`) | 128 bits with a fault parameter the neutral struct has no room for. A vCPU parked between two exits should never have one — so if one turns up the **snapshot is refused**, rather than written without it. |
| The RTC's time of day | It is computed from the host clock on every read and has no stored form. A resumed guest sees wall-clock time that has moved on — which is what a laptop's own suspend does, and what `hwclock`/NTP exist to correct. |

#### Where the capture happens

On each vCPU's **own thread**, at the lifecycle checkpoint. Not an aesthetic
choice: `spawn_vcpus` *moves* each `Vcpu` into its thread, so no other thread
has a handle to it — and KVM wants its vCPU ioctls from the owning thread
anyway. `Lifecycle::capture_cpus` is therefore a second barrier with the same
shape as the reset one: phase `SaveVcpus`, every parked vCPU reads its own
state into a shared slot, the requester waits, and the VM is still paused when
it returns. A vCPU that cannot report fails the *whole* capture — a snapshot
missing a CPU restores a guest with one fewer.

### 2. Device state beside every `reset()`

ADR-0005's table said, device by device, what goes back to power-on. Read the
other way, it says what a suspend has to write down:

| Device | Saved | Restored |
|---|---|---|
| `virtio_core::TransportState` | features (offered *and* negotiated) and their selectors, status, activation, per-queue geometry, `SHM_SEL`, `config_generation`, the ISR | features re-acknowledged, queues rebuilt through the same `build` the guest's own `DRIVER_OK` goes through, then the device re-activated |
| shared-memory placement (VEN-2001) | where the **host** put each region, by `shmid`, on both transports since phase 2 (on PCI the driver derives the address from BAR 2 and never reads this field — it is recorded purely for the file) | *not* restored — the new machine allocates its own host pages and maps them where the restored BAR says, in that order, and the transport's own `load` then compares. A machine that would place the window elsewhere, which for this window means a machine with a different memory size, is a named refusal: the guest's blob mappings point at the old address. A window the guest had unmapped when the snapshot was taken records nothing, so it does not insist on an address nobody was using |
| virtqueue positions | the device's `next_avail`/`next_used` | applied to the rebuilt queues **before** the device sees them |
| MSI-X | message control, the table, the PBA, config and per-queue vectors | all of it; nothing pending is delivered — the guest unmasking a vector is what sends it |
| PCI configuration space | all 64 dwords of every function, plus the latched `CONFIG_ADDRESS` | written back and everything derived from it re-published (the INTx flag, the mirrored MSI-X control), then the queue-notify ioeventfds re-based around the restored BARs |
| 8259 / 8254 / IOAPIC (userspace, WHP) | both chips including their ICW step, the three 8254 channels, the redirection table **and its pending-edge bits** | IOAPIC first, then the 8254 and the 8259s |
| in-kernel chips (KVM) | `KVM_GET_IRQCHIP` ×3 and `KVM_GET_PIT2` | 8259s, 8254, then the IOAPIC — see §3 |
| 16550 | the register file and the *unread host input* | both; the line is **not** re-raised, because the interrupt controller's own state carries whatever the guest was owed |
| RTC | index latch and CMOS bytes | both |
| ACPI PM block | the registers, the timer's reading, the shutdown latch | the timer re-anchored to the new epoch, the latch restored |
| pflash | the CFI command state machine only | the same; the contents are the NVRAM file, which the restored machine opens |
| virtio-snd | all four queues' positions (control, event, TX, RX) and the guest's stream state | both. The **host sink** is not saved: the WASAPI or ALSA device this process opened is gone, and a stream that was mid-playback resumes with a gap — what a real machine's suspend does to it |
| reset controls | the latches, including "the guest asked to reboot" | both — a guest that asked and was suspended before being served is still owed its reboot |

Three per-device answers are worth stating outright.

**virtio-blk has no state beyond its queue.** Every request is served
synchronously inside `notify`: the chain is walked, the file read or written,
the used entry added and the interrupt signalled before `notify` returns. So a
quiesced device holds nothing — there is nowhere for a request to be in flight.
What still has to be carried is the *position*, because the guest may have
posted requests the device has not been kicked for, and a restored device
starting at zero would serve every completed request again.

**virtio-net's flows cannot survive, and the device does not need resetting.**
The user-mode NAT's connections live in host sockets this process no longer has;
a TAP interface is re-opened from scratch. But the *device* — MAC, rings,
negotiated features — restores exactly, so the guest experiences a network blip
and its TCP stack notices in the ordinary way, with a retransmit that is never
answered. That is precisely what a laptop's own suspend does to it. Resetting
the interface toward the guest would be a bigger lie, not a smaller one.

**virtio-gpu's 2D half comes back; the host-owned halves cannot.** A 2D resource is a host
BGRA buffer plus the list of **guest** pages the driver attached to it — and the
snapshot already carries those pages in full. So the pixels are not saved at
all: the resource table records identity, geometry and backing list, and the
restore re-runs the transfer the guest would have run. For a 1920×1080 desktop
that is eight megabytes saved per resource, several times over, and it is the
difference between a resumed desktop and a black window. A 3D resource is a
texture inside the host GL driver, reached through virglrenderer, and a
rendering *context* is a live command-stream state machine; nothing hands either
back. Neither does a **blob resource** (VEN-2001): it is a mapping into a
host-visible window this process did not exist to make, and a `HOST3D` blob's
bytes never left the renderer at all. Both are therefore *counted* rather than
described — there is nothing useful to write down, only the driver can make them
again — and a guest that had either open is told its device needs a reset when
it resumes, the same signal and the same recovery path GPU-012 already uses when
the isolated renderer crashes.

The scanout binding follows the same three-way split: a 2D source is rebound and
pushed to the window, a 3D or blob source is not, and the window keeps its
initial frame until the driver programs a new one.

### 3. The chips that were not there

The first restored guest came back, kept drawing on its GPU, and never printed
another line.

On KVM the 8259 pair, the IOAPIC and the 8254 live **in the kernel**. Nothing in
`machine_x86::state` had ever seen them, because on that host nothing in
userspace owns them. A VM restored with a power-on IOAPIC has every pin masked,
so the 16550 could never interrupt again — and the GPU kept working because
MSI-X bypasses the IOAPIC entirely and goes straight to the local APIC.

That asymmetry is the whole lesson: **the two hosts do not lose the same things,
so "every device has a save" has to be checked against the hypervisor as well as
against the device list.** `vmm_core::hv::HostIrqChip` is the seam that closes
it, and a snapshot that is missing the section on a host that needs one is now a
named refusal rather than a one-way VM.

### 4. Guest memory, sparsely

One section per RAM region — a guest above 3 GiB has two, low RAM and the
remainder at 4 GiB — encoded as a run of `(offset, length, bytes)` triples
covering only the pages that are not entirely zero.

This is not an optimisation. A guest is handed zero-filled RAM and touches a
fraction of it; writing the rest would make every suspend a full-size write and
every resume a full-size read, which is the difference between suspend being
usable and being a feature nobody turns on. The restore side needs no special
case: both hypervisors hand out zero-filled pages, so a page that was skipped is
already what it was.

**Runs rather than file holes.** A hole-punched file would carry the guest's
*apparent* size even when almost none of it is allocated, and the first `cp` of
it would expand to the full size. The run list keeps the file itself small. The
snapshot is still marked sparse through `disk_image::ops::mark_sparse` — this
workspace has exactly one home for hole-aware file operations and `vm-snapshot`
adds none of its own.

### 5. A versioned format, and what it refuses

```text
  0        magic "ENTGLSNP", format version, flags, host, arch,
           index offset + length + SHA-256                       (72 bytes)
  72       section payloads, back to back, in the order written
  index    one 64-byte entry per section: kind, version, instance,
           offset, length, SHA-256 of the payload
```

The index is at the end because the largest section is guest memory and it is
streamed: one forward pass over multi-gigabyte data, then a four-field patch of
the header. The whole file is written to `<path>.part` and renamed, so an
interrupted suspend never leaves half a snapshot where one is supposed to be.

The codec is hand-written, not `serde` on whatever structs exist — ADR-0005 asked
for exactly that. A snapshot outlives the build that wrote it, so field order and
field width *are* the format and have to be visible in one place; deriving them
from Rust structs would make an innocuous field reorder a silent format change.

**A snapshot is untrusted input.** It is host-side data, but it outlives its
build, gets copied between machines, truncated by a full disk, half-synced by a
host that lost power, and handed over by someone else. So:

| Refusal | Why it exists |
|---|---|
| bad magic | it is not one of ours |
| format version ≠ this build's | half a machine is worse than none |
| unknown header flags | a newer build put something in it we would ignore |
| foreign host / foreign arch | the CPU blobs are that hypervisor's |
| truncated, or an index that points outside the file | a cut-short copy |
| section digest mismatch | a torn memory section reads as plausible pages |
| unknown section kind | state the guest expects and would silently not get |
| a **section version** that is not this build's | a section whose *shape* changed. Venus phase 1 bumped `virtio` to 2 (`SHM_SEL` and the host's region placement) and the virtio-gpu blob to 2 (the scanout's source became three-way, blob resources joined the table); a version-1 snapshot has neither, and defaulting them would restore a guest whose driver had selected a region into one that had not. The message names the section and both versions |
| a shared-memory region the host placed elsewhere | the guest's blob mappings point at the address it read from the registers. Live since VEN-2001 phase 2 (2026-09-09): the aperture starts at the top of RAM, so a restore into a differently sized guest moves the window and this is what catches it |
| trailing bytes in a section | written by a build that put more in it |
| a count or length that cannot fit | nothing is ever allocated on an unchecked number |
| vCPU count, memory size, transport, boot mode, device list/order | a guest whose `/dev/vda` is now somebody else's disk |
| **a disk whose size or mtime moved** | see below |

The disk refusal is the one that matters most. Restoring a guest is putting a
live kernel back on top of storage it thinks it still owns: its page cache holds
inodes, directory entries and journal state describing the filesystem *as it was
at the instant of the snapshot*. Let it loose on an image that has moved on —
mounted elsewhere, resized, restored from a backup, written by a second VM — and
it writes its stale metadata over the new contents. So a changed disk is a
refusal, and the refusal names the disk and what about it changed. Size and
mtime rather than a content digest: hashing 32 GiB at both ends of every suspend
would cost more than the snapshot, and the pair catches every accident this is
meant to catch. Firmware, kernel and initramfs images are recorded too but are
*advisory* — they are only re-read by a later in-place reset, and the restored
guest is long past them.

The parser has a cargo-fuzz target (`fuzz/fuzz_targets/snapshot_parse.rs`) which
asserts no panic, no allocation driven by an unchecked length, and an **exact
re-encode** for everything that decodes. That last property found a real bug in
under five minutes: an absent `Option` could carry a non-zero payload, so
`(false, 7)` and `(false, 0)` both meant `None` and the format had two spellings
for one state.

### 6. How it is reached

| Surface | Suspend | Resume |
|---|---|---|
| the VM window | `Ctrl+Alt+S` | — (a new process, with a new window) |
| `entangled run --control-stdin` | `save [path]` | — |
| the command line | `run --snapshot <file>` names the target | `entangled resume <file>` |
| `entangled-manager` | a running machine's **Suspend** button | **Resume** on a suspended machine's card, or on any row of the Snapshots view |

`Ctrl+Alt+S` joins `Ctrl+Alt+P`/`R`/`G`/`Q`/`O` and `F11`, and like them it hands
the modifiers back to the guest first — with a stronger reason: the guest is
about to be frozen for good and must not be left holding a key it will never see
released.

**The snapshot carries the profile it was taken from**, verbatim, so
`entangled resume <file>` needs nothing else. A snapshot that had to be paired
with a TOML someone might have edited in the meantime would be a snapshot with a
second, unversioned half.

`VmState` gains `Suspending` and `Suspended`, and the arm is **one-way** into
`Stopping`. A snapshot is taken because the VM is about to stop existing in this
process; letting `Suspended` go back to `Running` would leave two live copies of
one machine, both convinced they own its disks. A *failed* suspend goes back to
`Paused`, which is where the seam actually leaves it.

**Restore is a boot path.** The machine is assembled exactly as a cold boot
assembles it — same memory size, same devices in the same order, the same BAR
assignments — and then, instead of loading a kernel or a firmware, the snapshot
is loaded over it. Order: guest memory first (restoring a transport *activates*
it, and activation validates the driver's rings against the memory they point
into), then the devices, then the hypervisor's own chips, then the clock, then
every vCPU. The MP table and the ACPI tables are **not** rewritten: they are
already in the snapshot's memory, and the guest may have reused the pages around
them.

## Measured

**Release builds**, on the development machine, with other VMs running on it —
so these are ordinary numbers rather than best cases.

| | KVM (WSL Ubuntu) | WHP (native Windows) |
|---|---|---|
| **bootstrap guest** — 256 MiB, 1 vCPU, virtio-pci | | |
| suspend (the seam's own time) | 294–380 ms | 281–333 ms |
| resume, process start to the guest's next line | 364 ms | 207 ms |
| memory written / guest RAM | 75.0 MiB of 256 MiB — **29.3%** | the same |
| file on disk | 78.6 MB | the same |
| **installed Ubuntu** — 2 GiB, 2 vCPUs, UEFI, GPT disk | | |
| suspend (the seam's own time) | 2.76 s | — |
| suspend, wall clock including process exit | 3.31 s | — |
| resume, process start to the guest answering a keystroke | 5.25 s | — |
| memory written / guest RAM | 514 MiB of 2.0 GiB — **25.1%** | — |
| file on disk | 539.5 MB | — |

Two things those numbers say.

**The ratio is the whole design.** A desktop-shaped guest writes a quarter of
its RAM, and the bootstrap guest's 29% is *higher* only because it unpacks its
entire initramfs into a tmpfs — almost all of its 256 MiB really is touched. A
snapshot that wrote every page would be four times the size and four times the
wait, on a guest whose interesting state is a quarter of it.

**Debug builds cost an order of magnitude**, which is worth knowing before
anyone measures the wrong binary: the same Ubuntu suspend takes 25.7 s in a
debug build against 2.76 s in release. The zero-page scan is the hot loop and it
is exactly the kind of code `opt-level=0` ruins.

## Acceptance

**The bootstrap guest's heartbeat continues.** `entangled.heartbeat=200` prints
a monotonic counter from a real timer loop. Suspended after
`VMHOST_HEARTBEAT 8`, the resumed VM's first line is:

```text
VMHOST_HEARTBEAT 9
```

— with no second `VMHOST_GUEST_READY`. A machine whose registers, MSRs or local
APIC came back merely plausible does not carry on counting; it faults, or it
goes quiet. The same test, the same evidence, on both hosts
(`apps/entangled/tests/suspend_restore.rs`).

**Every refusal, by name.** One snapshot, corrupted six ways, plus a disk that
grew while the VM was suspended:

```text
not an Entangled snapshot: the file does not start with the "ENTGLSNP" magic
snapshot is truncated: section index needs 78630318 more bytes, 39315159 are left
snapshot format version 8 cannot be restored by this build (it writes and reads version 1)
snapshot header carries unknown flags 0x1; it was written by a newer build
snapshot was taken on Windows/WHP and this is Linux/KVM: a saved CPU carries that
  hypervisor's own interrupt-controller and extended-state blobs, which the other
  one cannot load
snapshot section memory[0] is corrupt: its contents do not match the recorded digest
disk /tmp/.../root.raw has changed since the snapshot was taken (size: was 8388608,
  is 16777216); restoring onto it would corrupt the guest's filesystem
```

**An installed Ubuntu is the same session afterwards** —
`apps/entangled/tests/guest_suspend.rs`, `#[ignore]`d. It logs in over the serial
console, records the guest's identity, suspends mid-session, resumes, and asks
again:

```text
boot 3fca9132-d589-40ce-bf01-11ee665bb238 -> 3fca9132-d589-40ce-bf01-11ee665bb238,
shell pid 1171 -> 1171, uptime 133s -> 137s
```

The kernel's `boot_id` is generated once per boot and cannot coincide; the shell
answering is the same process it was before; the monotonic clock moved forward
by the four seconds the suspend took rather than restarting. Beside those, the
resumed process shows **no login prompt** (the getty did not run again) and the
shell's `history` still holds the marker typed before the suspend. Then it
powers off through systemd and ACPI, which means the kernel is not merely alive
but able to run its whole shutdown path.

## Consequences

- Every new device owes the machine a third thing, beside ADR-0005's `reset()`
  and its `Quiesce` gate: a `save`/`load` pair, and — if it holds queues — a
  `queue_positions`. A device that skips them makes a resumed guest subtly
  wrong, which is the hardest kind of wrong to find.
- A snapshot is bound to its host, its build and its disks. That is three ways
  for a file to become unusable, and all three are deliberate: the alternative
  to each refusal is a guest that misbehaves later.
- `entangled-manager` is wired to this — see the amendment below.
- **A resumed guest is not identical to one that never stopped.** The honest
  list is in the TODO below.

## What a resumed guest still gets wrong

1. **The host audio sink restarts.** A virtio-snd stream that was mid-playback
   resumes with a gap: the device this process had open is gone and a new one
   starts wherever it starts. The guest's stream state comes back, so it plays
   on rather than stalling.
2. **Wall-clock time jumps.** The RTC is derived from the host clock, so a guest
   suspended for an hour resumes an hour behind and corrects itself through NTP.
   Correct for a laptop, wrong for a VM that was supposed to be frozen, and the
   right answer differs per use — it needs a policy, not a patch. (ADR-0005 left
   the same debt for pause.)
3. **The paravirtual clock is restored on KVM and absent on WHP.** A WHP guest
   using an invariant TSC comes back consistently; there is no equivalent of
   `KVM_SET_CLOCK` to put a kvmclock back, and no kvmclock in that partition to
   put back.
4. **Network connections die.** By construction; see §2.
5. **3D contexts and blob resources die.** By construction; see §2. The guest is
   told, which is more than a silent failure, but a compositor that does not act
   on `DEVICE_NEEDS_RESET` will need restarting. A guest scanning out of a blob
   keeps the window's initial frame until it programs a new scanout.
6. **The 2D resource restore has not been watched on a full desktop.** It is
   exercised by the acceptance guests' framebuffer console (the resource table
   comes back with its pixels, `restored=1 blank=0 presented=true`) and by unit
   tests over a scattered 2048-page backing list — but a GNOME session's
   compositor has more resources and re-draws differently, and nobody has yet
   put a suspended one back and looked at the screen.
   `entangled resume --screenshot-after` exists to make that a one-command
   check; it needs a desktop image that is not in use by anything else.
7. **Host input queued while the VM was frozen is dropped**, as it is by a pause.
8. **Dirty-page tracking is not implemented.** Every suspend writes every
   non-zero page. `KVM_GET_DIRTY_LOG` and `MEM_WRITE_WATCH` are what would turn a
   repeated suspend of the same VM into an incremental one; the alias in
   `vmm_core::memory` exists for exactly that divergence.
9. **The snapshot is not compressed** and not encrypted. A desktop guest's file
   is the size of its touched RAM.
10. **`Suspended` never goes back to `Running` in the same process.** Resuming is
   always a new process. Nothing needs it to be otherwise today, but a manager
   that wanted a "hibernate and wake" button inside one process would.
11. **A snapshot pins its disks by size and mtime.** A filesystem with coarse or
   absent mtimes (some network mounts) weakens the check to size alone, and the
   code says so rather than pretending otherwise.

## Amendment, 2026-09-09: the manager's three states

`entangled-manager` now reaches all of this, and doing so turned up one thing
the ADR had not had to name.

**A machine with a snapshot is a third resting state.** The manager's `Status`
gains `Suspending` and `Suspended` beside Running/Stopping/Installing/Stopped,
and only the first of those is a property of a process. `Suspended` is *the
absence of a child plus the presence of a file* — which is what makes it
survive the manager being closed and reopened, unlike "running", which this
process only knows because it started the child (ADR-0005's known gap about
orphan adoption is unchanged, and does not apply here). The file is looked for
at exactly one place, `<name>.esnap` beside the profile, because that is where a
bare `save` writes; a copy someone made by hand is a snapshot of the same
machine, appears in the Snapshots view, and is deliberately *not* what the card
offers to resume.

**Every refusal is computed before the button is drawn.** `vm_snapshot::inspect`
costs a file open, so the manager takes the verdict during its directory scan
and greys the Resume button out with the reason on its hover, rather than
letting a child process discover it. Two of the checks the engine cannot make
for the manager:

- `SnapshotInfo::restorable_here` answers for *this* process, and the manager is
  never the process that restores anything. On Windows a machine may be set to
  run through WSL, where the hypervisor is KVM and a Linux snapshot is exactly
  right — so the host check is redone against the machine's chosen backend, and
  the refusal names the backend that *would* work.
- A path recorded by the other engine (`/mnt/d/vms/root.raw` seen from Windows)
  is not a disk that has vanished. It is left to the engine that can see it,
  with a note saying so, because "your disk is missing" is the loudest refusal
  in the product and it must not fire on a file that is fine.

**Editing the machine orphans its snapshot**, and the manager says so twice: the
Configure button's hover warns before the edit, and a shape that no longer
matches is a named refusal on the card afterwards ("memory: was 2048 MiB, is
4096 MiB"). The engine's own `MachineShape::check` is the backstop.

**"Start fresh" deletes the snapshot first**, with a confirmation. A cold-booted
guest writes to the disk within seconds, and a snapshot pinned to that disk as
it was is then a file the engine will refuse — so keeping it would leave a card
that goes on offering a Resume which cannot work. Deleting a machine takes its
snapshot with it for the same reason (`discovery::plan_delete`).

**A resume does not consume the file**, deliberately — a restore that fails at
startup has to be retriable, and `entangled resume` also points a later bare
`save` back at the same path, which is what makes closing and re-opening a
machine a loop rather than a one-way trip. The consequence is visible in the
manager and is the honest reading rather than a bug: a machine that was resumed
and then *stopped* (instead of suspended again) goes back to `Suspended` with
its old file, and — once the running guest has written to its disk — a card that
says "cannot resume here: the disk has changed". "Start fresh…" is the way out,
and it deletes the file with the user watching.

The vocabulary of the control channel moved to `control_api::control` in the
process: `entangled run` and `entangled-manager` are two crates at the ends of
one pipe, and a reply prefix spelled as a literal on each side would have
drifted silently — the Suspend button would have spun until the child exited and
then reported the wrong thing. A failed suspend is the case that makes this
matter: the engine exits with status 0 either way, so the reply line is the only
place the reason ever appears, and the manager lifts it out of the log as it
goes past rather than scanning the tail afterwards (a desktop guest can push the
whole log buffer through in the seconds a suspend takes).

## Amendment, 2026-09-09 — virtio-snd rewinds its queue positions

The device matrix says virtio-snd saves "all four queues' positions and the
guest's stream state", which is true but hides a deliberate asymmetry worth
recording, because the next device author will have to make the same choice.

virtio-snd reports `queue_positions` **rewound by the number of un-retired
messages**, so a restore re-delivers periods the guest had posted but the
device had not completed. virtio-gpu does the opposite and reports the real
position. Both are right, for opposite reasons: a GPU command in flight has
already had its side effect on host state, so replaying it would repeat that
effect, whereas an audio period that was never played has had none — dropping
it would silently lose the descriptors the guest is still waiting on.

The rule this implies: **rewind only what a restore can safely do again.**
