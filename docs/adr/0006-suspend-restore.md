# ADR-0006: Suspend and restore — writing a VM down, and reading it back

## Experimental Venus disk checkpoints, 2026-10-26

The x86 engine now has a per-profile `[display] venus_checkpoints = true`
opt-in. It retains reconstruction recipes from the first context, streams GPU
state into the full VM snapshot and recreates it before releasing device workers
and vCPUs. Resume detects the GPU section and enables the required renderer and
retention automatically; no environment override is needed. Profiles without
the opt-in retain the normal refusal for live Venus resources. False is omitted
when serializing the new key, preserving older engines' profile compatibility.

Real Windows/WHP Ubuntu GNOME has survived two fresh-process headless
restore/save cycles, preserving boot ID and compositor PID with a visible
desktop and subsequent GPU scanout updates. A windowed initial save and first
fresh-process restore/resave have also passed without the developer environment
override. This proves process-independent reconstruction of that desktop;
A real Windows power-cycle and subsequent restore/resave passed on 2026-10-28,
preserving the same boot ID and GNOME PID (engine26, save5.95s). This still
does not prove every application or Vulkan workload. See
[the progress record](../venus-checkpoint-progress-2026-10-26.md) for artifacts,
current disk/snapshot pairing and pending acceptance. Never restore a historical
file onto a disk modified by a later resumed session.

This is still experimental: recipe retention is bounded and uncompacted, and
unsupported images, ambiguous ownership, actual aliasing, exposed GPU addresses,
queries or outstanding future-dependent work may refuse capture. Those refusals
keep the source running. Passing a GNOME checkpoint does not establish arbitrary
games, Vulkan workloads or another host/GPU as supported.

Shared-image payload v3 preserves pending layout-changing external transfers
alongside the existing stable releases (v1) and proven unused images (v2).
Readback and upload recreate the exact old/new layout pair, so the resumed
guest can acquire it unchanged. A native separate-process regression verifies
COLOR_ATTACHMENT→GENERAL, unchanged green pixels after a failed disk write and
cold reconstruction, then a blue clear submitted by the reconstructed guest.
The independent hardware cursor plane is carried by GPU device-state v3;
legacy v2 remains readable with no saved cursor plane.

### Historical implementation notes (initial requirement, 2026-10-25)

The requested behaviour is a whole-session checkpoint that survives both the
VMM process exiting and a Windows restart. A resident paused VM does not meet
this requirement. At the start of this work it was not implemented; the refusal
for live Venus objects and exported blobs was the required production policy.

The missing state is larger than the exported scanout images. The executor's
`Objects` table keeps native handles and validation facts; it does not retain
complete creation recipes for every live shader, pipeline, descriptor set and
recorded command buffer. The renderer also owns ring/reply-stream positions,
exported memory shared between contexts, shared-memory mappings, queue
timelines and deferred submissions. The current GPU-state encoding carries
only counts for the host-owned resources, and restore requests a device reset.
Removing the refusal cannot recover any of the missing state.

A complete implementation must save bounded, versioned reconstruction recipes
with guest identities, checkpoint buffer/image contents and their layouts and
ownership after GPU work settles, preserve cross-context memory identity and
synchronisation state, and restore all of them before reactivating the guest's
queues. Readback must not alter the running session if saving fails. Each of
these records is untrusted file input and needs allocation bounds and hostile
input tests. The acceptance is a fresh-process restore of the accelerated
desktop with applications still open and usable, including another save/restore
cycle; an in-process pause or a 2D firmware restore is not that acceptance.

Implemented groundwork: `venus::executor::checkpoint` can retain bounded,
guest-id command recipes before handle translation and reconstruct their object
namespace on a fresh backend. Every recipe also retains the completed guest
reply: different return values or object identities on replay cause cleanup
and refusal rather than resuming with a changed namespace. Failed state changes
invalidate the journal; ordinary guest execution continues. It validates the
complete request framing before starting and destroys partially recreated
objects on execution failure. GPU
submissions/waits are deliberately not reconstruction commands. Object-owned
host-visible memory has a separate streaming save/load pair, with all restored
allocation ids and sizes checked before any write. A native RTX 2070 test fills
an imported buffer on the GPU, destroys its device, recreates it from recipes,
restores the bytes and has the new GPU device copy them into another allocation.
A second native test runs save and restore in two separate processes, carrying
only the recipe, memory and fixture metadata files between them, and checks the
GPU's copied data. This proves fresh-process reconstruction of this buffer
fixture, not restoration of the accelerated desktop or a Windows reboot.
Recipe retention was initially limited to fixtures; the profile opt-in above
now enables it in the product renderer.
The fixture also carries completed fences, timeline values, binary permanent
and temporary semaphore payloads, and event signalling state through the two
processes. Its resumed GPU waits on those primitives before copying the restored
buffer. Event reconstruction retains the original host/GPU origin, queue family,
source stages and synchronization2 dependencies, even if the original command
buffer has been freed. Private reconstruction pools/fences survive timed-out GPU
work through the normal queue clocks and deferred destruction. Capture remains
non-destructive on failure. Retired event dependencies, protected-only queues and
multi-device event masks still have explicit refusals.

The buffer payload component (`checkpoint_buffers`) now streams live bound,
private device-local buffers through at most 1 MiB of host-private staging at
a time, and uploads them into freshly reconstructed allocations. The native
two-process fixture fills over 2 MiB in private RTX 2070 VRAM (memory type 1),
checks the nonzero pattern read by the GPU, restores three chunks and has the
new device read every byte back identically. The component currently requires
transfer usage, one unprotected queue, no uncaptured device addresses and no
shared/exported or image-aliased allocation. These are explicit refusals pending
the corresponding state tracking; they do not waive the renderer's refusal.
Every image plane binding is now retained, including non-canonical images, to
detect image/buffer aliasing before issuing a copy.

Private staging buffers, imported memory and command pools enter the normal
deferred-destruction graph with their completion fence. A timed-out readback
retains both staging and a subsequently guest-destroyed source until the GPU
finishes; disk-write failure preserves the running context. Upload failure
discards the partial reconstruction. Entire buffer-file framing, ids, parents,
sizes and bindings are validated before the first GPU upload. Streaming memory
and buffer records explicitly carry the decoder's four-byte blob padding,
including odd allocation/buffer lengths.

The image component (`checkpoint_images`) now saves private, non-aliased,
single-sample 2D color images through bounded staging tiles. Its format block
geometry comes from the vendored Vulkan registry, including compressed edges.
An executed-layout tracker handles explicit barriers, secondary recordings,
render-pass final layouts, framebuffer layer limits and multiview masks; recorded
but unsubmitted barriers do not change checkpoint state. Metadata is bounded and
charged, including expansion of repeated secondary command buffers. Equal-layout
Sync2 dependencies preserve heterogeneous subresource layouts. Each readback
restores the source layout. Each completed upload updates tracking, so later
tiles never discard earlier pixels through an UNDEFINED transition.
The native separate-process fixture now preserves every pixel of a 513x513
magenta RGBA image across two staging tiles and its shader-read layout. Entire
image files, layout/usage compatibility, sharing modes and bindings are checked
before upload. Timeout retains a guest-destroyed source image and staging;
a host upload failure discards partial reconstruction. Allocation generation
tokens distinguish an old dangling binding from a newly reused guest memory id.

A further shared-payload component (`scanout::checkpoint`) saves and recreates a
released canonical BGRA8 image, including when its exporting device has already
been destroyed. Its file carries pixels, GPU/driver UUIDs, allocation size/type,
canonical create info and external release state; it never carries an OS handle.
Restore creates a new allocation/handle, validates the current driver parameters,
and charges it to the restoring renderer's heap limits. Every native import keeps
that charge until the import itself is destroyed. Staging requires native coherent
host memory. The separate-process fixture checks a varying 513x257 pixel pattern
in five layouts, with the exporting device destroyed before every capture/readback.
A simulated disk-write failure leaves subsequent readback identical. Upload timeout
retains private targets and their heap charge until the GPU finishes.

For private scanout copies, layout transitions are now separate from the matching
external acquire/release operations. The externally agreed layout is restored
before returning ownership, following the
[Vulkan queue-family ownership rules](https://docs.vulkan.org/spec/latest/chapters/synchronization.html#synchronization-queue-transfers).
This payload component requires all writers drained and proof of the active
canonical image at offset zero with a stable-layout external release. It does not
yet persist the blob directory, cross-context attachment permissions, allocation
links or ownership proof; those remain requirements for renderer integration.

`checkpoint_shared` now gives shared allocations context-local generation ordinals
and a global, dense payload-id table for the file. Runtime handle identity is kept
in checkpoint-enabled allocation objects after a blob disappears; it never enters
the file. Replay imports the already recreated payload before binding any image
or buffer. It checks device, allocation generation, type/size and native export
compatibility, shares the payload's existing budget charge, and verifies that every
linked allocation is still the expected live generation after replay. A stale link
to a freed/reused id discards the partial reconstruction. Link files are bounded
and reject duplicate generations/ids, unknown payload ids and trailing bytes.
Pure memory-resource property queries are not object recipes: their old blob may
be gone and they do not construct context state.

The native shared-image two-process fixture additionally writes recipes, generation
links and sync state for an exporter and importer on two native Vulkan devices.
Both original devices are destroyed before saving their green image. The fresh
process recreates the payload and both contexts without the old directory, writes
blue through the restored exporter and checks those pixels through the restored
importer's own device. This demonstrates actual shared backing across reconstructed
contexts, including the importer's original blob-properties query. Direct native
fixtures must drive `execute_wait` to completion; `execute` only polls a wait once.

Recipe format version 2 additionally stores historical import geometry: generation,
device/id, actual host allocation size and backing kind. The entire bounded footer
is checked against decoded allocation commands before creating host objects; its
storage shares the command journal budget. Retired imports rebuild using private
scratch VRAM or host pages with the original host size, preserving the guest's
binding/dedication rules. Later history must free every scratch allocation; any
live placeholder refuses reconstruction and triggers cleanup. Active shared links
must match the captured import geometry. This permits retired imports after their
blobs disappear, without replaying historical GPU work or publishing private
copies as live shared memory. The native separate-process fixture now exercises
both retired VRAM and retired host-page imports before its active shared allocation.
Host-memory payload encoding remains version 1, independent of recipe framing.

A host-page graph component (`checkpoint_pages`) now inventories live Vulkan
allocations and accepts ring/reply/blob roots into one dense page-id table. Each
allocation is streamed once in 64 KiB chunks, including all mapped padding, with
its declared length, mapped span and alignment. Restore validates the entire file
before allocation, rejects private/poisoned roots, and refunds partially loaded
pages if a later allocation fails. Owner/share budgets are supplied by the renderer;
imports retain the loaded Arc and never charge the same bytes again. Checked mapped
byte access retains RingPages atomic and private-lock access rules.

Generation/page links also preserve whether memory already has a blob. The complete
memory replay entry point requires every live host allocation to be linked and
checks page identity afterwards; missing/stale/duplicate/cross-kind links or changed
geometry discard reconstruction. Device addresses and multi-device page allocations
remain refused. Object-only replay keeps its older separate host-memory payload path.
The native two-process fixture now carries a live host-page allocation shared by
both contexts. It checks the saved bytes, writes through the restored exporter's
CPU mapping and has the importer's GPU copy those new bytes into a readback buffer,
while retaining the shared-image and retired-import checks.

The ring pump now has a fixed-size versioned progress record and a non-mutating
adoption path. It validates the host cursor against restored head, status against
the pump mirror (independent of monitor ALIVE), and pending tail against capacity.
Idle, stalled and fatal states survive; a fatal ring may retain the invalid tail
that stopped it. Capture is only valid between complete sink calls while quiesced,
with the corresponding layout, shared pages and sink state. Tests cover counter
wrap, skipping already consumed bytes, stalled wakeup and hostile records. Live
worker capture and reply-window restoration originally remained separate; the
on-demand worker and renderer ring manifests below now connect these codecs.
The service adoption path restarts polling with IDLE cleared, preserving fatal
stoppage and checking queued work without relying on a pre-save doorbell. Tests
cover work queued behind a parked service and refusal to run a fatal ring's sink.
The native buffer fixture now loops bounded `execute_wait` calls until the fence
actually completes: a single `execute` only polls, so it must not guard readback.

The sink component (`checkpoint_ring`) now retains context/ring identity, reply
blob id and generation, window offset/size/cursor and the virtqueue roundtrip
count in a bounded versioned record. The directory must restore exact generations
first; assigning new ones could turn an old stale reference into a live blob.
Restoring checks ownership/type/range for live references and keeps Gone windows
stale, preserving the point at which the next reply-bearing command fails.
Malformed input does not change the sink or roundtrip. Tests cover stale replies,
counter values near u64::MAX, wrong identities, geometry and reused resource ids.
The codec still requires a whole-call quiesce point. Queries of a worker blocked
inside a nested command stream refuse unless that call reaches quiescence.

The directory manifest (`renderer::checkpoint_directory`) now retains resource
generations and the next counter, global page/payload ids, owners and import
attachments, canonical image records, pending binds, and the exact reverse handle
index. Its decoder bounds all records and validates them before restoring a fresh
directory. All roots resolve before native image tokens. The context resolver
checks the actual image recipe, binding/allocation generation and shared payload,
and installs image-owned liveness tokens. Release metadata is not an executed
ownership proof; that still belongs to payload capture after draining all writers.

On-demand `RingWorker::checkpoint` queries run on the owning thread under a closed,
settled gate, without stopping the source or writing shared control words. Queries
interrupt a park without ringing or consuming its doorbell; a wake already consumed
at the pause gate survives capture. Request serials discard late timeout responses.
Fatal ended workers retain their final state, while teardown and unsupported sinks
refuse capture. The renderer's `checkpoint_rings` manifest now captures all contexts,
relative layouts, monitor metadata, page roots, roundtrip counts and pump/sink state
with a shared deadline, bounding metadata and rejecting malformed/duplicate records.

The native separate-process fixture now restores its real blob directory and two
executing ring workers too. Both append a new reply to the saved window without
rebinding it; the original reply bytes, cursor and roundtrip count survive. This
still is not the complete renderer/transport/VM restore or desktop acceptance.

The blob manifest (`checkpoint_blobs`) now restores blob identities, accounting,
accepted scanouts and the guest aperture's exact publication offsets into a fresh
renderer. It validates inventory, ownership, geometry, budgets, range overlap and
canonical-image matches before any mapping. The native factory separately checks
the real allocation span: two agreeing file fields cannot prove an opaque handle
is that large. Scanout preparation failure rolls back prior publications and
partial targets. Presenter handles are regenerated; an accepted scanout with no
live canonical recipe refuses capture rather than saving an unrestorable session.
The native process fixture now exercises these restored aperture mappings too.

Ring restoration now preflights every page/control word and every sink before
clearing IDLE or starting threads. It recreates context poison state and monitors;
native sink/renderer roundtrip copies must agree even when one is zero. A portable
renderer test restores mappings, two executing workers, reply cursors, a monitor
and an empty poisoned context. It also rejects a malformed later sink without
changing the earlier ring's shared control words.

Budget-owner metadata now preserves exact factory root/share identities across
context-id reuse and orphan allocations. File caps resolve against current trusted
policy; live allocations recharge empty shares. Host-page roots retain original
owners, while anonymous ring pages remain renderer-budgeted. Native replay checks
original owner identity and requires imports to share the same factory root.
Owned shared-image reconstruction checks the exact heap share before allocation.

The factory object manifest now joins budget links, recipes, global allocation
links and sync state for every native context, with bounded framing and a common
deadline. All graph links resolve before native replay. Prepared context guards
destroy incomplete native objects on failure; failure in a later context cleans
earlier replay too, leaving the factory empty. Adoption rejects foreign/duplicate
contexts. The native two-process fixture now uses this manifest, the budget-aware
factory and the renderer's actual ring restore path.

Executed canonical shared ownership is now retained separately from recorded
directory barriers. Epochs reserved before native submission prevent a delayed
older CPU commit from replacing newer cross-context work. Only accepted submits
publish state. Missing payload claims/marks, ordering timeouts and unrepresentable
shared bindings refuse capture. The private uploader returns an ownership proof
after successful pixel upload/release, which the matching cold factory validates
before retaining state for a later save. A native regression records but never
submits a newer barrier and requires capture to retain the actual old layout.

The renderer object-graph entry point now joins native objects, directory/blobs,
rings and actual budget owners. Inventories agree before replay. Private contents
restore before mappings and paused workers; failure rolls back adopted contexts
and all publications. Outstanding transport retirements/native graveyards still
refuse capture. The native process fixture now uses the integrated loader and
explicitly publishes three aperture mappings, checking a fresh page address as
well as resumed ring replies and shared pixels; earlier equality of mapping counts
alone could pass with no mapping at all. File-backed page restoration validates
complete framing/owners before allocation and reads payloads in 64 KiB chunks,
refunding incomplete allocations after an I/O failure.

This still needs complete allocation payload/container and VM transport integration
and recipe compaction (the journal is still bounded history). Those requirements
are not waived by the component fixtures.

This image component still requires transfer usage, one unprotected queue with
unit transfer granularity, and exclusive backing allocation. Host-visible optimal
images, depth/stencil, multisampling, multiple queues, active aliases and shared
payloads remain unsupported. General image support, buffer ownership/alias/address
state, orphan exports, held submissions, ring-fence
retirement timelines and transport integration remain missing, so the whole-session
requirement above is still open.

The renderer now has a single streamed archive containing its object graph,
shared host pages, canonical shared-image pixels and private buffer/image payloads.
It computes component sizes without a second GPU readback and bounds every reader
to that component. The native RTX separate-process fixture loads this archive,
checks private GPU contents and all three nonzero aperture publications, then
saves it again. Page/payload ids are local to one capture: a later manifest must
be used with the table captured alongside it, not a previous table's numbering.

The virtio front-end blob/context components retain logical ids, capsets, guest
backing, exact aperture reservations and accepted scanouts. Reservation restore
never clears checkpointed contents or remaps native pages. Context adoption never
issues CTX_CREATE again. The renderer archive has a checked entry point that
compares both namespaces before output or native reconstruction. An object-safe,
opt-in Renderer3d/Gpu3d seam dispatches this to the native factory; legacy factories
refuse by default. This seam alone is not GpuDevice/transport integration.

The whole-VM container now reserves section kind 14 for GPU archives, version 1,
instanced by the corresponding virtio GPU slot. Explicit GPU-aware save streams
those sections into the existing atomic/digested file. Explicit GPU-aware restore
validates GPU integrity before changing guest RAM and retains the same opened
file for subsequent bounded random-access uploads. VerifiedSectionReader checks
the complete digest and authenticates every cache refill against that pass,
including rereads after eviction. Ordinary restore refuses files containing GPU
sections rather than silently dropping them. Device/lifecycle orchestration still
must apply every retained archive before starting queues, workers or vCPUs.

MachineLifecycle.prepare_save now runs after all CPUs/workers stop but before CPU
capture. GPU preparation may return already completed responses and their IRQs
there, so the used ring and CPU/APIC snapshot describe the same moment. Failure
leaves the source paused and retryable; the hook does not itself drain GPU work.
The device, transports, MachineBus and VM coordinator now connect this stream
behind the developer-only ENTANGLED_VENUS_CHECKPOINT=1 switch, set before the
first context and required again on resume. Default production Venus suspend
remains refused. The experimental path still refuses unsupported GPU state in
the archive validators. Restore requires exact GPU-slot agreement before RAM
changes, and both hosts hold workers/timers stopped until devices, GPU and CPU
state have been applied. GPU preparation polls truly retired responses/IRQs
without consuming another guest request or using the runtime timeout watchdog;
timeout preserves pending work and permits a later retry. Full missing resource/
synchronisation state, journal compaction and desktop acceptance remain required.

A real Ubuntu/GNOME probe reached the streamed native writer and refused an
address-capable allocation. It remained responsive with the same boot id after
refusal. Capability flags alone do not prove that a device address reached the
guest. Capture now uses a monotonic context flag set by every nonzero
GetBufferDeviceAddress reply, and continues to refuse exposed addresses even
after their buffer/memory is freed. Unqueried address-capable resources use normal
reconstruction and payload transfer. This does not implement exposed-address
replay. Vulkan defines a valid address relative to a queried buffer address;
see [Vulkan fundamentals](https://docs.vulkan.org/spec/latest/chapters/fundamentals.html).

## Amendment, 2026-10-25: a refused save preserves the live session

The window's Suspend button exposed a destructive error path: Venus correctly
refused a snapshot containing live host Vulkan state, but the lifecycle supervisor
stopped the VM even though no snapshot existed. Only a successful save may now
request shutdown. On refusal, the supervisor restores the previous pause state,
reports the error on the control channel, keeps serving controls, and shows a
30-second notice in the window title and toolbar, including fullscreen. The
manager consumes the failure while the child is alive, ends its spinner and
allows another attempt. This supersedes the older failed-save exit policy below.

This does not make active Venus Vulkan objects serializable. A live 3D session
still refuses suspend to disk when its GPU state cannot be carried; Pause keeps
that session in memory. Neither an error nor Pause is reported as a saved session.
Regression coverage exercises supervisor survival, a device refusal after CPU
capture with running and paused guests, continued CPU progress after refusal,
and manager retry after receiving the error.

Native WHP acceptance on this machine: the Ubuntu Venus desktop refused its
three exported device-local blobs while running, returned to Running, and
continued answering SSH. A second attempt while paused left it Paused; Resume
then restored SSH access. Ubuntu subsequently powered off normally. Separately,
a diskless 256 MiB, one-vCPU UEFI/2D control VM saved successfully, exited,
restored in a new process, and processed a serial Enter into BootManagerMenuApp;
it then saved again and exited. This is a firmware-level save/restore check,
not evidence of successful desktop or 3D hibernation. Runtime logs are
`F:/VMs/Entangled/suspend-fixed.log` and
`F:/VMs/Entangled/local-build/suspend-2d{,-resume}.log`. The notice was inspected
using the toolbar's actual rasterizer; native window capture timed out, while
the accessibility title confirmed that the live window received the notice.

Verification: Windows workspace 2412 passed / 18 ignored; Linux workspace 2428
passed / 26 ignored with `--test-threads=1`; workspace Clippy with warnings
denied and formatting passed on both hosts, and cargo-deny passed. A parallel
Linux run reproduced the two existing TCP teardown timing failures (the
same-breath close and flow-table saturation tests); the first parallel run
and the final complete serial run passed. No networking code was changed.

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

Since the dirty-page amendment below, the runs are scanned in place and in
parallel and stored as LZ4 blocks; the run encoding itself is unchanged, and the
section is at version 2.

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
| a memory **block** that is empty, over-long, of an unknown codec, or that unpacks to a different length than it claims | added with compression; see the dirty-page amendment for the full list |
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
8. **Every suspend still writes every non-zero page, on purpose.** Dirty-page
   tracking exists (`vmm_core::hv::DirtyLog`, both hosts) and is *not* what
   decides a snapshot's contents: neither host's log can see a write this
   process makes into guest RAM, which on the bootstrap guest is 16.7 % of what
   the snapshot has to carry. See the dirty-page amendment below for the
   measurement and the decision.
9. ~~**The snapshot is not compressed.**~~ Closed by the same amendment: memory
   sections are LZ4 blocks, 2.2–2.3× on a real desktop guest. Still **not
   encrypted**.
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

## Amendment, 2026-09-09 — dirty-page tracking, and why the snapshot is still whole

The TODO below opened with *"dirty-page tracking is not implemented; every
suspend writes every non-zero page"*, and named `KVM_GET_DIRTY_LOG` and
`MEM_WRITE_WATCH` as what would turn a repeated suspend into an incremental one.
Both are now behind the seam. **Neither is used to decide what a snapshot
contains**, and the reason is a measurement rather than a preference.

### 1. The seam

`vmm_core::hv::DirtyLog` is a peer of `GuestClock` and `HostIrqChip`: the layer
above holds it without holding a `VmFd` or a `WHV_PARTITION_HANDLE`.
`DirtyPages` is the portable half — one bit per 4 KiB, LSB first, with the page,
run and byte arithmetic unit-tested on both hosts.

| | KVM | WHP |
|---|---|---|
| arming | `KVM_MEM_LOG_DIRTY_PAGES`, set by re-registering each RAM slot — **at any time** | `WHvMapGpaRangeFlagTrackDirtyPages`, a flag on `WHvMapGpaRange` — **when the range is mapped, and never again** |
| reading | `KVM_GET_DIRTY_LOG` per slot | `WHvQueryGpaRangeDirtyBitmap` per region |
| reading clears | yes (no `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT` here) | yes |

That first row is the asymmetry, and it is why `set_dirty_logging` is a request
rather than a setter: WHP agrees with the answer `WhpOptions::track_dirty`
already gave and refuses to change it, saying why. Unmapping and re-mapping
every byte of a running guest's RAM to flip a flag is not a trade a backend
makes on a caller's behalf, and pretending the two hosts have the same switch
would have hidden exactly the thing a reader needs to know.

### 2. What the log cannot see, and what that costs

**Both hosts track the guest's writes by protecting the guest's second-level
page tables. A write this process makes through the host allocation those tables
point at faults neither one and is reported by neither.** Every virtio-blk read
completion, every received packet, every used-ring update, every boot image is
such a write. `crates/vmm-core/tests/dirty_log.rs` states it as a test on both
hosts: arm the tracking, write two pages of guest RAM from the VMM, read back
zero dirty pages.

The log is inexact in the other direction too. On this AMD machine KVM reports a
page the guest merely **executed** from. That direction is harmless — an
incremental that copied it would only copy too much — but together the two mean
the dirty log is *neither a subset nor a superset of "what changed"*.

On a real guest, `ENTANGLED_TRACK_DIRTY=1` puts a number on it. The bootstrap
guest, 256 MiB, at the moment of suspend:

| | pages the snapshot must carry | of those, reported | **never reported** |
|---|---|---|---|
| WHP | 19 251 | 16 038 | **3 213 — 12.55 MiB, 16.7 %** |
| KVM (AMD) | 19 253 | 19 250 | 3 — 12 KiB |

The 12.55 MiB is, almost exactly, the kernel image this process loaded into
guest RAM. An incremental snapshot built on the log would have left the guest's
own kernel text at whatever the base happened to hold. And the two hosts
disagreeing by three orders of magnitude on the same guest is the argument by
itself: KVM's near-complete coverage here is an artefact of the execute
over-report on one vendor's silicon, not a property anything may rely on.

Making it sound is possible and is what QEMU does: every DMA path calls
`memory_region_set_dirty()` by hand. That would be a **fourth thing every device
owes the machine**, beside `reset()`, the `Quiesce` gate and the `save`/`load`
pair — and the one whose absence is the least visible of the four. A forgotten
`reset()` haunts a reboot; a forgotten `Quiesce` makes "paused" a lie; a
forgotten `save` makes a resumed guest subtly wrong. A forgotten dirty report
makes a resumed guest's **page cache** subtly wrong, which surfaces as
filesystem corruption an hour later in a place with no path back here.

### 3. So: no chain. The win goes into making one save fast

The other half of the trade is what an incremental would buy. A chain is a base
plus deltas, and:

- **the dominant case gets nothing.** The first suspend of a booted guest has no
  base. That is the suspend a desktop user actually performs;
- **the resting state stops being one file.** `<name>.esnap` beside the profile
  is what the manager looks for, what "Start fresh" deletes and what a bare
  `save` overwrites — and a later save writes back to the file it was resumed
  from, deliberately (see the manager amendment). A chain needs new names, a new
  deletion rule, a new "which of these may I remove" question in the GUI, and a
  restore that opens files it was not handed;
- **it adds a refusal that cannot always be checked.** A missing base is easy; a
  base that was *edited* between the delta being written and being read is a
  digest away, and a base restored from a backup that is byte-identical but
  describes a different guest is not detectable at all;
- **it does not save space.** Base plus deltas is larger than one snapshot.

So the decision is: **a snapshot stays a whole machine in one file**, and the
effort goes where the numbers said the time actually was.

### 4. Where the time was

Three changes to `vm_snapshot::memory`, in the order the measurements asked for
them:

1. **Scan the guest where it lies.** The old scanner read every region into a
   1 MiB buffer and looked for zeroes there — a full-RAM `memcpy` performed to
   discover that three quarters of it is zero. `region_bytes` reads through the
   bounds-checked `VolatileSlice` the region already offers. One `unsafe`, whose
   justification is this module's existing contract: a snapshot is taken at a
   lifecycle stop point, so nothing is writing guest memory while it is read. A
   save taken without that guarantee was already a torn picture whichever API it
   used; this makes the requirement explicit instead of paying for a copy that
   hid it.
2. **Slabs in parallel.** 4 MiB pieces to a pool of up to eight workers. The
   workers return only the **run boundaries** — a first attempt had them return
   the encoded bytes, and four threads came out *slower* than one, all of it in
   the allocator. The writer emits slabs in order, so the file does not depend on
   the thread count; `the_thread_count_does_not_change_the_file` asserts it byte
   for byte for both codecs.
3. **LZ4 block compression**, on the worker that scanned the slab — the one part
   of the pipeline with cores to spare. This closes TODO 9.

`ENTANGLED_SNAPSHOT_THREADS` and `ENTANGLED_SNAPSHOT_COMPRESS` exist so the
numbers below can be reproduced, and so a host where the CPU is scarcer than the
disk can say so.

### 5. The format change, and every new refusal

Only the third change touches the file. `MEMORY_VERSION` is **2**: the region
header's reserved word became a codec, and each slab is now a block:

```text
  gpa u64 | region length u64 | page size u32 | codec u32
  ( raw length u64 | stored length u64 | stored bytes ) *   until the section ends

  a block, once decoded:
  ( offset u64 | length u64 | length bytes ) *              until the block ends
```

Deliberately a *framing around the unchanged run encoding* rather than a new
encoding: how a run is spelled did not change, which keeps the change — and the
risk — in one place. A wholly zero slab writes no block at all, so an idle
guest's snapshot is still a few hundred bytes.

The parallel scan did **not** bump the version, and that is a rule worth
stating: a version buys a refusal, and a refusal that protects nothing costs
somebody their snapshot for no reason.

New refusals, each named and each tested (`the_block_framing_refuses_by_name`):

| Refusal | Why it exists |
|---|---|
| a memory section at version 1 | its reserved word is not a codec, and reading it as one would decode a zero as "uncompressed" and then disagree about every length |
| an unknown codec | a future codec must be a refusal on an old build, not a mis-decode |
| a raw block length above 64 MiB | a decoder must not allocate on somebody else's number |
| a stored block length above that plus a megabyte | the same, for the buffer the bytes arrive in |
| an uncompressed block whose two lengths disagree | one of them is a lie and there is no way to tell which |
| a zero-length block | the writer never emits one, and a decoder that accepted it would make no progress |
| bytes that are not an LZ4 block | |
| a block that decompresses **short** of what it claims | the bytes past the end would restore as zeroes — a hole in the guest that nothing else would ever see |
| a run header or a run that ends past the block | |
| a run that ends past its region | already there; now checked inside the block |

`fuzz/fuzz_targets/snapshot_parse.rs` grew a fourth layer for exactly this
surface: a region header this build agrees with, and everything after it from
the fuzzer — block lengths, codec, LZ4 payload, the run headers inside. What is
asserted beyond "no panic" is that anything which decodes survives a re-save and
a second restore unchanged, which is the memory section's version of the exact
re-encode the other sections get. **165 196 runs in 241 s, no crashes.**

### 6. Measured

Two references, because they answer different questions.

**`crates/vm-snapshot/tests/memory_bench.rs`** is the controlled one: the same
synthetic guest, the same fill, before and after, in the same minute. It reports
a **cold** and a **warm** pass, and the distinction turned out to matter — a
real guest's untouched RAM is not resident in the host, so the scan takes a
minor fault per page to read a zero, and a benchmark that discarded its first
run would have understated the old scanner's cost by a third. Cold figures,
release:

| 2 GiB guest, 25 % touched | KVM (WSL) before | after | WHP before | after |
|---|---|---|---|---|
| save | 5.07 s | **844 ms** | 4.93 s | **804 ms** |
| file | 512.3 MiB | **320.0 MiB** | 512.3 MiB | **320.0 MiB** |
| restore | 1.19 s | 1.21 s | 765 ms | 780 ms |
| 256 MiB guest, save | 393 ms | **129 ms** | 638 ms | **116 ms** |
| all-zero 2 GiB, save | 1.63 s | **156 ms** | 1.68 s | **154 ms** |

**The real guests** are the end-to-end confirmation, release, through
`entangled run --control-stdin` and `entangled resume`:

| | KVM before | after | WHP before | after |
|---|---|---|---|---|
| **bootstrap**, 256 MiB, direct Linux, virtio-pci | | | | |
| suspend (the seam's own time) | 421 ms | **159 ms** | 270 ms | **129–164 ms** |
| file | 78.8 MB | **47.9 MB** | 78.8 MB | **49.8 MB** |
| **installed Ubuntu**, 2 GiB, 2 vCPUs, UEFI, GPT disk | | | | |
| suspend | 4.13 s | **888 ms** | 5.71 s | **1.47 s** |
| memory written | 519 MiB | 520 MiB | 511 MiB | 543 MiB |
| file | 544 MB | **243 MB** (2.24×) | 536 MB | **248 MB** (2.30×) |
| resume, to the guest answering | 5.26 s | 5.25 s | — | 5.26 s |

Three things those numbers say.

**Restore did not get faster, and did not need to.** Decompressing gives back
what reading less costs; the resumed guest answers in the same five seconds it
did before, of which the memory read was never the largest part.

**Real guest memory compresses better than the synthetic guest does** — 2.24–2.30×
against 1.60×. The synthetic fill is deliberately three pseudorandom bytes in
every eight, which is closer to the pessimistic end; a real guest has text,
page cache and zeroed structures.

**The machine is shared, and one reading proves it.** An earlier KVM Ubuntu
suspend read 2.99 s where the repeat read 888 ms; the first was taken with a
Windows workspace build running beside it, which is the exact combination the
`dev-environment` skill warns about. Numbers from this machine are only
comparable against numbers taken beside them.

### 7. What arming the tracking costs

Boot to `VMHOST_GUEST_READY` on the bootstrap guest, which unpacks its whole
initramfs into a tmpfs on the way there and so takes a first-write fault on
almost every page it will ever touch:

| | tracking off | tracking on |
|---|---|---|
| KVM | 3.68 / 3.82 / 4.41 s | 3.66 / 3.72 / 4.54 s |
| WHP | 3.49 / 3.68 / 3.79 s | 3.44 / 3.68 / 3.69 s |

Within the run-to-run spread, on both hosts: ~19 000 write-protection faults do
not show against a three-and-a-half second boot. What a boot cannot show is the
cost that outlives it — a KVM memslot with dirty logging on cannot use huge
pages, so the guest runs with 4 KiB second-level entries for its whole life, and
on WHP the flag cannot be taken back at all. Both are reasons to leave tracking
off by default, and neither is a reason to disbelieve the table.

### 8. What this changes about the resting state

Nothing. A snapshot is still one file, still self-contained, still bound to its
host, its build and its disks. `ENTANGLED_TRACK_DIRTY=1` arms the log and makes
the suspend log say what the guest wrote against what had to be saved; nothing
else in the product reads it. The instrument ships rather than living in a
branch nobody can run, because the decision above is one somebody should be able
to re-examine on another machine — on Intel, where the execute over-report does
not happen, or on a guest whose disk is busy, where the unreported fraction will
be larger than 16.7 %, not smaller.
