---
name: virtio-device
description: Implementing virtio devices and both virtio transports (mmio and pci) for Entangled Desktop — virtqueues, feature negotiation, PCI config space, irqfd/ioeventfd, and the untrusted-guest safety rules (backlog EPICs 3, 4, 5, 8, 9, 19; crates virtio-core, virtio-block, virtio-net, virtio-gpu, virtio-input). Load before any virtio work.
---

# VirtIO devices

Scope: both transports (EPIC 3, EPIC 19) and every device crate. The
transports and shared safety live in `crates/virtio-core`; one crate per device.

## Architecture contract

- Devices implement `virtio_core::VirtioDevice` and see **queues, features,
  config space** — never transport registers. A transport owns registers,
  status and queue plumbing. This is not aspirational: adding virtio-pci
  changed `virtio-core` and the machine's bus wiring and **no device crate at
  all**. Keep it that way — a device that needs to know its transport is a
  design bug, and a trait change to accommodate one needs a hard justification.
- The shared half of a transport is `virtio_core::state::TransportState`:
  feature negotiation, the device-status state machine, per-queue
  `QueueConfig`, activation, reset, `DEVICE_NEEDS_RESET`, notify-offload
  bookkeeping. **A transport module is only an address decoder.** If you find
  yourself adding state-machine logic to `transport.rs` or `pci.rs`, it belongs
  in `state.rs` instead.
- Modern interface only: `VIRTIO_F_VERSION_1` is mandatory, no legacy mode,
  mmio `VERSION = 2`, PCI revision ≥ 1 with no legacy I/O BAR. Register offsets
  are in `virtio_core::mmio` and `virtio_core::pci::common` — use the
  constants, never magic numbers.
- Ring parsing comes from the `virtio-queue` crate; Entangled Desktop policy on top of
  it lives in `virtio_core::chain`.

## The two transports

Chosen per VM by `transport = "mmio" | "pci"` in the profile (default `mmio`);
one is attached and the other stays empty. Their address ranges are disjoint, so
`MachineBus` can route both unconditionally.

| | virtio-mmio (EPIC 3) | virtio-pci (EPIC 19) |
|---|---|---|
| Module | `virtio_core::transport` | `virtio_core::pci` |
| Machine bus | `machine_x86::virtio` | `machine_x86::virtio_pci` + `machine_x86::pci` |
| Discovery | `virtio_mmio.device=4K@base:irq` on the cmdline; nothing enumerates | guest walks bus 0; nothing on the cmdline |
| Probe order → device names | cmdline clause order | PCI device number (dense from `00:01.0`) |
| Register window | one 4 KiB slot from `layout::virtio_mmio_slot(n)` | one 32 KiB memory BAR from `layout::pci_bar_slot(n)`, found via the capability list |
| Access widths | 32-bit aligned only | 1/2/4/8 bytes, per field |
| Config access | none (the window *is* the device) | mechanism #1 on `0xcf8`/`0xcfc`; no ECAM (needs an ACPI MCFG we do not publish) |
| Queue kick | `QUEUE_NOTIFY`, queue index in the value | notification area, `notify_off_multiplier = 4`, queue index in the **address** |
| ioeventfd | one shared address + 4-byte datamatch on the index | one address per queue, **no datamatch** (so any write width works) |
| Interrupt | single IRQ, `INTERRUPT_STATUS` + write-to-`INTERRUPT_ACK` | **MSI-X** (`queues + 1` vectors) with INTx underneath it; the driver picks |
| Guest kernel needs | `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` | `CONFIG_PCI` + `CONFIG_VIRTIO_PCI` |
| UEFI | unusable — EDK2 CloudHv ships no virtio-MMIO driver | **required** for an ISO boot (ADR-0003) |

Both use `virtio_core::LineInterrupt` for their single-line path: the ISR bits and
the mmio `INTERRUPT_STATUS` bits are the same two bits in the same positions. On
pci that object is wrapped by `virtio_core::msix::MsixInterrupt`, which delivers
an MSI instead whenever the driver has MSI-X enabled — see "Interrupts" below.

### virtio-pci BAR layout

One BAR, six page-aligned regions. The first four are published as
vendor-specific PCI capability records (`pci::capability_records`), the last two
through the MSI-X capability (`msix::capability_record`). Page-sized so a region
could later get its own KVM slot without anything moving.

| Structure | `cfg_type` | BAR offset | Length |
|---|---:|---:|---:|
| common configuration | 1 | `0x0000` | `0x1000` (60 bytes used) |
| ISR status | 3 | `0x1000` | `0x1000` (1 byte) |
| notification area | 2 | `0x2000` | `0x1000` (one dword per queue) |
| device configuration | 4 | `0x3000` | `0x1000` |
| MSI-X table | — | `0x4000` | `0x1000` (16 B × 256 vectors) |
| MSI-X PBA | — | `0x5000` | `0x1000` (one bit per vector) |

`VIRTIO_PCI_BAR_SIZE` is therefore **32 KiB** (the next power of two; the sizing
protocol cannot express 24 KiB), and `layout::PCI_MMIO_SLOT_SIZE` must equal it.
One BAR rather than two on purpose: the aperture allocator, the DSDT `_CRS`,
`locate_mmio` and the ioeventfd rebase are all written around one window per
function, and a second window would double what the rebase has to converge on
while EDK2 permutes BARs.

Identity: vendor `0x1af4`, device `0x1040 + virtio type`, revision 1, subsystem
vendor `0x1af4` (Linux reads the virtio vendor id from *that* field), INTA#.
`pci::device_id` and `pci::class_code` are free functions because the machine
builds a device's config space before its transport exists and the two must
agree.

### Interrupts on virtio-pci: MSI-X, with INTx underneath

Every function publishes **both**, and the driver picks. One
`msix::MsixInterrupt` serves both and decides **per signal** from the message
control register the guest last wrote, because a driver moves between them:
Linux tries per-queue MSI-X vectors, then a shared vector, then INTx, and
`pci_free_irq_vectors` on an unbind puts the function back on INTx — all while
the device holds one `Arc<dyn Interrupt>` for its whole life.

- **MSI-X** (`crates/virtio-core/src/msix.rs`): `queues + 1` vectors, table and
  PBA in the BAR, delivery through `virtio_core::MsiSink` — an (address, data)
  pair, implemented on Linux by `machine_x86::msi::KvmMsiSink` over
  `KVM_SIGNAL_MSI`. Under MSI-X the ISR byte is unused (spec 4.1.4.5) and the
  INTx line is never raised. Vector assignment is the two common-config registers
  (`queue_msix_vector` per selected queue, `config_msix_vector`); an out-of-range
  request reads back `VIRTIO_MSI_NO_VECTOR`, which is the spec's own way of
  saying "refused".
  - Two guest-owned registers reach the transport without `machine_x86::pci`
    interpreting them: message control via `ConfigSpace::mirror_dword`, and a
    *change* to it via `ConfigWrite::mirror_changed` →
    `PciTransport::msix_control_changed`, which releases anything the PBA holds.
  - `KVM_SIGNAL_MSI` was chosen over irqfd + `KVM_SET_GSI_ROUTING` because every
    device here is a userspace thread that already holds the message: same one
    syscall, no GSI allocator, no routing table to rebuild on every guest table
    write, and no need for virtio-core to know what a vector *means*. The
    argument is written out in `machine_x86::msi`'s module docs; revisit it only
    if something kernel-side (vhost) ever needs to signal.
- **INTx**: one IOAPIC pin per device, an edge through a KVM irqfd, ISR byte
  read-to-clear, `INTX_DISABLE` honoured. Not a legacy path — it is where a
  driver starts, falls back to, and returns to, and what a host without
  `KVM_CAP_SIGNAL_MSI` or `ENTANGLED_PCI_MSIX=off` gets. Its three costs are
  still real and MSI-X retires all three: pins are **never shared** (one device
  per pin, bounded by `MAX_PCI_DEVICES`), `interrupt_line` had to be made
  read-only because `PciBusDxe` scribbles on it, and the guest logs
  `can't find IRQ for PCI INT A` because we publish ISA interrupt sources and no
  `_PRT`. A `_PRT` in the DSDT is still the clean fix *for INTx*; MSI-X means
  nothing depends on it any more.

Because a Linux guest offered MSI-X never chooses INTx, the INTx path is kept
tested by asking for it: `PciInterruptMode::IntxOnly` (or
`ENTANGLED_PCI_MSIX=off`). `tests/boot/tests/pci_transport.rs` boots both.

## Untrusted-guest rules (non-negotiable, from CLAUDE.md)

1. Every descriptor-chain walk is guarded by `ChainWalkGuard` — bounded by
   `MAX_DESC_CHAIN_LEN`, index-checked against the ring size. A guest must
   not be able to loop or overrun the host.
2. All guest memory access through `vm-memory` checked APIs. No raw offsets.
3. Malformed input → fail the request (write error status, use the buffer as
   far as valid) or set DEVICE_NEEDS_RESET. Never `panic!`/`unwrap()` on a
   guest-controlled path.
4. Validate guest-supplied geometry before allocating or copying:
   `virtio_block::validate_range` (sectors), `virtio_gpu::Rect::fits_within`
   (pixels), `virtio_gpu::MAX_RESOURCE_PIXELS` (allocations). Extend these
   helpers rather than inlining checks.
5. Status writes are validated with `virtio_core::status::write_is_valid`;
   write of 0 = reset. `reset()` must be infallible and return the device to
   pre-ACKNOWLEDGE state (acceptance criterion EPIC 3).

## Resource bounds (MVP-1407 audit)

Every allocation or iteration a guest can influence is capped by a **named
constant**, and every cap has a test that proves it holds. Extend this table when
you add a device: a bound without an enforcing test is not done.

| Constant | Value | Bounds | Enforcing test |
|---|---:|---|---|
| `virtio_core::MAX_DESC_CHAIN_LEN` | 128 | descriptors visited per chain walk | `virtio_core::chain::tests::{looped_chain_is_cut_off, self_referencing_chain_is_rejected, small_queue_caps_at_queue_size}` |
| `virtio_core::MAX_QUEUE_SIZE` | 256 | ring size a device may advertise | `virtio_core::transport::tests::rejects_devices_that_break_the_contract` |
| `virtio_core::status::KNOWN` | `0xcf` | bits a guest status write may leave in `STATUS` | `virtio_core::transport::tests::reserved_status_bits_are_dropped_not_stored` |
| `virtio_block::MAX_REQUEST_BYTES` | 4 MiB | bytes staged for one disk request | `virtio_block::request::tests::{total_len_caps_oversized_requests, validate_range_enforces_the_payload_cap_on_its_own}`, `blk_queue::oversized_requests_are_rejected` |
| `virtio_block::MAX_DATA_SEGMENTS` | 126 | *derived* (`MAX_DESC_CHAIN_LEN - 2`); reported to callers, enforced by the chain walk | the chain-walk tests above |
| `virtio_block::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick | `blk_queue::one_notification_is_bounded_and_never_truncates_a_full_ring` |
| `virtio_gpu::MAX_RESOURCE_PIXELS` | 4096×2304 | pixels in one 2D resource | `gpu_queue::zero_sized_oversized_and_duplicate_resources_are_rejected` |
| `virtio_gpu::resource::MAX_TOTAL_RESOURCE_PIXELS` | 8× the above | pixels across all live resources | `virtio_gpu::resource::tests::resource_count_and_total_pixels_are_capped`, `gpu_queue::too_many_resources_run_out_of_host_memory_cleanly` |
| `virtio_gpu::resource::MAX_RESOURCES` | 64 | live host resources | same tests |
| `virtio_gpu::resource::MAX_BACKING_ENTRIES` | 16384 | entries in one attach-backing list | `gpu_queue::transfers_without_backing_or_with_short_backing_are_rejected` |
| `virtio_gpu::MAX_COMMAND_BYTES` | ≈256 KiB | bytes gathered for one controlq command (2D-only device) | `virtio_gpu::device::tests::gather_request_refuses_a_chain_over_the_command_cap`, `gpu_queue::oversized_commands_are_rejected_without_staging_them` |
| `virtio_gpu::MAX_COMMAND_BYTES_3D` | 1 MiB + 32 B | gather cap with a 3D renderer attached (a full `SUBMIT_3D`) | `virtio_gpu::device::tests::the_3d_command_cap_covers_a_full_submit_and_nothing_more`, `gpu_3d::a_malicious_3d_guest_is_answered_in_band` |
| `virtio_gpu::renderer::MAX_SUBMIT_BYTES` | 1 MiB | one `SUBMIT_3D` command stream | `gpu_3d::a_malicious_3d_guest_is_answered_in_band`, `null_renderer::tests::the_validation_front_rejects_what_the_spec_says_it_must` |
| `virtio_gpu::renderer::MAX_CONTEXTS` | 128 | live 3D rendering contexts | `null_renderer::tests::the_context_and_resource_caps_hold` |
| `virtio_gpu::renderer::MAX_3D_RESOURCES` | 16384 | live 3D resources | same test |
| `virtio_gpu::renderer::MAX_TOTAL_3D_ELEMENTS` | 2³⁰ | width×height×depth×layers summed over live 3D resources (bytes-per-element is the renderer's) | same test (asserts the budget is exact) |
| `virtio_gpu::fence::MAX_PENDING_FENCES` | 64 | fenced responses (and therefore descriptor chains) the device holds for host fences; past it a fence completes synchronously | `fence::tests::the_cap_holds_and_returns_the_payload`, `gpu_fence::the_pending_fence_table_is_capped_and_never_wedges_the_device` |
| `virtio_gpu::device::FENCE_TIMEOUT` | 2 s | how long a deferred response waits before the watchdog answers it anyway | `gpu_fence::a_fence_that_never_retires_is_completed_by_the_watchdog` |
| `virtio_gpu::remote::MAX_FRAME_BYTES` | 40 MiB | bytes in one message either way across the renderer-process boundary, refused *before* reserving | `remote::tests::an_oversized_length_header_is_refused_before_allocating`, fuzz target `gpu_remote_protocol` |
| `virtio_gpu::remote::protocol::REMOTE_XFER_WINDOW` / `REMOTE_MAX_BACKING` | 8 MiB / 64 MiB | backing bytes per transfer across that boundary / shadow backing per resource | `gpu_remote::the_isolated_renderer_serves_the_whole_3d_path` (round trip), server-side refusal |
| `virtio_gpu::renderer::MAX_3D_WIDTH` / `MAX_3D_DIM` / `MAX_3D_ARRAY` / `MAX_3D_LAST_LEVEL` / `MAX_3D_SAMPLES` | 2²⁸ / 16384 / 2048 / 15 / 32 | per-axis geometry of one `RESOURCE_CREATE_3D` | `gpu_3d::a_malicious_3d_guest_is_answered_in_band` |
| `virtio_gpu::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick (controlq and cursorq) | same shape as the blk budget test |
| `virtio_input::MAX_PENDING_EVENTS` | 1024 | host-buffered input events while the guest is not draining (oldest dropped) | `input_queue::a_starved_queue_buffers_events_up_to_the_bound_and_drops_the_oldest` |
| `virtio_input::config::PAYLOAD_MAX` | 128 | config-space payload bytes | `virtio_input::config::tests::{bitmap_drops_codes_beyond_the_payload, from_slice_truncates_at_the_payload_size}` |
| `virtio_net::MAX_FRAME_LEN` / `MAX_BUFFER_LEN` | 1514 / 1526 | bytes staged per frame, either direction | `net_queue::{tx_oversized_frames_are_dropped, an_oversized_host_frame_is_dropped_before_the_ring, rx_chain_too_small_for_the_frame_drops_it}` |
| `virtio_net::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick | same shape as the blk budget test |
| `machine_x86::virtio::MAX_VIRTIO_SLOTS` | 8 | devices on the mmio bus (IOAPIC pins) | `queue_notify::attaching_more_devices_than_slots_is_refused` |
| `machine_x86::notify::MAX_OFFLOADED_QUEUES` | 16 | ioeventfds and epoll slots one device may demand | `queue_notify::queue_notify_offload_is_capped_per_device` |
| `virtio_core::pci::MAX_NOTIFY_QUEUES` | 1024 | *derived* (notify region ÷ multiplier); queues a device may expose on pci, since each needs its own notification address | `virtio_core::pci::tests::a_device_with_more_queues_than_notify_slots_is_refused` |
| `virtio_core::pci::VIRTIO_PCI_BAR_SIZE` | 32 KiB | guest-addressable register space per pci device; every capability's `offset + length` must fit | `virtio_core::pci::tests::capability_records_describe_the_real_bar_layout`, `regions_do_not_overlap_and_the_common_struct_fits` |
| `virtio_core::msix::MAX_MSIX_VECTORS` | 256 | *derived* (table region ÷ 16 B); vectors one function may publish, so `queues + 1` must fit or the transport refuses the device | `virtio_core::msix::tests::table_size_for_*`, `virtio_core::pci::tests::a_device_with_more_queues_than_msix_vectors_is_refused` |
| `machine_x86::pci::MAX_PCI_DEVICES` | 9 | config spaces, BAR windows and IOAPIC pins on the root bus (8 devices + the host bridge) | `machine_x86::pci::tests::the_bus_is_bounded` |
| `machine_x86::layout::PCI_MMIO_SLOTS` | 8 | 32 KiB aperture slots at `0xc000_0000`; one per device, and `locate_mmio` decodes nothing outside the aperture | `machine_x86::pci::tests::{addresses_outside_the_aperture_are_never_claimed, a_bar_moved_out_of_the_aperture_decodes_nothing}` |
| `machine_x86::pci::reg::SIZE` | 256 | one function's config space; capability records are refused rather than truncated when they would run past it | `machine_x86::pci::tests::capability_space_is_bounded` |
| `machine_x86::serial::RX_CAPACITY` (private) | 4096 | buffered guest serial input | `machine_x86::serial::tests::rx_overrun_is_bounded` |
| `display::MAX_PENDING_BATCHES` / `MAX_PENDING_CONTROL` | 256 / 64 | un-drained host input batches / control events | `display::input::tests::{queue_drops_the_oldest_batch_when_the_guest_stalls, control_queue_is_bounded}` |

Two more bounds are load-bearing but not ours to name: `virtio-queue` refuses a
ring whose available count exceeds the queue size (so a guest cannot jump
`avail_idx` arbitrarily far ahead — asserted in the blk budget test), and every
pixel buffer is allocated with `try_reserve_exact`, so a cap the host cannot
satisfy becomes `ERR_OUT_OF_MEMORY` rather than an abort.

## virtio-mmio implementation notes (MVP-301…307)

- One 4 KiB mmio slot per device from `machine_x86::layout::virtio_mmio_slot(n)`,
  IRQs from `VIRTIO_MMIO_FIRST_IRQ + n`, announced to the guest via
  `virtio_core::mmio::cmdline_clause`.
- Queue notify (MVP-307, **done**): every (device, queue) has an `EventFd`
  registered with KVM as an **ioeventfd** on `slot_base + QUEUE_NOTIFY` with a
  4-byte datamatch on the queue index, and one **worker thread per device**
  epolls those eventfds and calls `MmioTransport::queue_notify`. KVM completes
  the kick inside the kernel, so the vCPU never exits and device work runs
  concurrently with guest execution. All eventfd/epoll/KVM plumbing lives in
  `machine_x86::notify`; `virtio-core` only records *which* queues are offloaded
  (ADR-0002), so its register path can drop a `QUEUE_NOTIFY` write the host
  primitive already owns instead of running the device twice.
  - Offload is per queue and best-effort: a refused registration leaves that
    queue on the synchronous MMIO path. `ENTANGLED_QUEUE_NOTIFY=sync` (or
    `QueueNotifyMode::Synchronous`) disables it wholesale, which is how the
    before/after measurement is taken.
  - `VirtioMmioBus::shutdown` — and its `Drop`, so error paths are covered —
    joins every worker and deassigns every ioeventfd. A guest-driven *device
    reset* deliberately keeps the worker running: the registration is host
    wiring, and the transport’s `activated` flag already drops kicks that arrive
    while the device is down.
- Interrupts: `irqfd` bound to the device’s GSI; set `INTERRUPT_STATUS` bit
  `INT_VRING` before signaling, clear on `INTERRUPT_ACK` write.
  **Known defect:** the machine model publishes no MP table or MADT, so the guest
  falls back to virtual-wire ExtINT through the 8259 and loses roughly one device
  interrupt in three (a boot with a `[[disk]]` then stalls on the first read).
  See the `IrqFdLine` docs in `machine_x86::virtio` and the reproducer in the
  vm-testing skill.
- Feature negotiation is 2×32-bit windows selected by `DEVICE_FEATURES_SEL` /
  `DRIVER_FEATURES_SEL`; reject FEATURES_OK (leave the bit unset) when the
  driver subset is unacceptable (`ack_features` returning false).

## virtio-pci implementation notes (EPIC 19)

- `machine_x86::pci` is **portable** — no KVM, no eventfds, no virtio types — so
  the whole config-space model is unit-testable on Windows too. Keep it that
  way; the Linux-only half (irqfds, ioeventfds, the transports) is
  `machine_x86::virtio_pci`.
- Config space is `[u32; 64]` plus a parallel **write mask**. Read-only-ness is
  therefore a property of the data, not of a match arm, and sub-dword accesses
  and capability walks fall out for free. To add a writable field, widen its
  mask; never special-case a write.
- BAR sizing needs no special code: the guest owns exactly the address bits
  `!(size - 1)`, so writing all-ones reads back the size mask. `size` must be a
  power of two and `base` naturally aligned — both checked, both host errors.
- `locate_mmio` refuses anything outside `layout::PCI_MMIO_BASE..PCI_MMIO_END`
  **and** anything while the command register's memory-enable bit is clear. A
  guest that moves a BAR elsewhere simply stops being decoded; it can never make
  the machine dispatch a foreign address into a device.
- The queue-notify offload (`machine_x86::notify`) is generic over the transport.
  To add a third transport, implement `QueueNotifyTarget` and pick a
  `NotifyAddressing` — do not fork the worker loop.
- MSI-X is **complete**, not partial: capability, table, PBA, per-vector masks,
  function mask, real vector assignment, and `LineInterrupt`'s per-vector sibling
  (`msix::MsixInterrupt`). See "Interrupts on virtio-pci" above. Two rules if you
  touch it: `machine_x86::pci` must stay ignorant of what MSI-X *means* (it
  publishes a dword and a write mask, nothing more), and the guest-facing
  decisions — is MSI-X enabled, is this vector masked — must be read at signal
  time rather than cached at activation, because Linux toggles them mid-probe.

## Per-device references

- **blk** (EPIC 4): request = header (type/reserved/sector) + data + status
  byte. Types in `virtio_block::RequestType`; status `S_OK/S_IOERR/S_UNSUPP`.
  Unknown type → `S_UNSUPP`, out-of-range → `S_IOERR` (test exists).
- **net** (EPIC 5): TX before RX (easier to debug); `virtio_net_hdr` is 12
  bytes with num_buffers when MRG_RXBUF — MVP negotiates **no offloads, no
  mergeable buffers, no multiqueue**: correct first, fast later. MAC from
  `virtio_net::MacAddr::derive(vm_name)` unless pinned in config.
  Two backends behind `NetBackend`: `tap` (Linux) and `usernet`, the in-process
  smoltcp NAT that is the only one on Windows. **The NAT's close path needs a
  workload that closes thousands of connections before you can call it done** —
  it leaked one flow per completed download until a real `entangled install
  debian` found it (stalled after exactly `MAX_FLOWS` udebs), because a guest
  that closes first leaves smoltcp in `CloseWait`, which `is_open()` still
  reports as open. `usernet::tcp::service_flows` propagates the guest's FIN as
  `Shutdown::Write` on the host stream; a unit test that closes both halves at
  once cannot see that class of bug.
- **gpu** (EPIC 8): wire format in `virtio_gpu::protocol` (constants, `CtrlHdr`,
  one struct per command, lengths asserted at compile time), host resources in
  `virtio_gpu::resource`, device in `virtio_gpu::device`. Only the two
  32-bit BGRA layouts (`FORMAT_B8G8R8A8_UNORM` = ARGB8888,
  `FORMAT_B8G8R8X8_UNORM` = XRGB8888, which is what Linux actually sends) —
  check with `is_supported_format`. controlq processes commands, cursorq is
  drained and ignored (MVP-812). Every rect through `Rect::fits_within` before
  any copy; every guest page read via `resource::read_backing`, which
  pre-validates the whole span so a rejected transfer changes nothing.
  The device reaches the window through the `virtio_gpu::ScanoutSink` trait
  (`GpuDevice::new(display_handle)`) — `display` depends on `virtio-gpu`, never
  the other way round.
  **3D (ADR-0004, GPU-002…012):** `GpuDevice::with_renderer` adds
  `VIRTIO_GPU_F_VIRGL`, capsets and the 3D command set. All decode/validation
  is portable (`virtio_gpu::renderer::Gpu3d` — bounded id tables, per-mip box
  checks, the `SUBMIT_3D` length walk); the host renderer sits behind the
  `Renderer3d` trait: `NullRenderer` (portable, tests/fuzzing) and
  `virtio_gpu::virgl::VirglRenderer` (Linux; dlopens libvirglrenderer at
  runtime, EGL surfaceless, lazy init on the worker thread because EGL is
  thread-affine, guest resets deferred to the next worker-thread call, iovec
  arrays + an `Arc<GuestMem>` owned for as long as the C side holds the
  pointers, never `virgl_renderer_cleanup`/dlclose — mesa TLS destructors
  SIGSEGV). In virgl mode **both halves of the one id namespace are live**:
  mesa's objects arrive through `RESOURCE_CREATE_3D`, while the guest kernel
  still creates its dumb/console framebuffer with `RESOURCE_CREATE_2D` and then
  attaches *that* to its 3D context — so scanout/cursor/attach/detach/unref/
  backing all route by which table owns the id, and a command that only one
  half can serve (`CTX_ATTACH_RESOURCE` has no renderer handle for a 2D id)
  completes after validation instead of being refused (ADR-0004's 2026-08-20
  amendment); flushes read back through `Renderer3d::read_rect_bgra` into
  the same `ScanoutSink` — **packed into the caller's buffer** with an explicit
  stride (phase 2; the phase-1 full-frame shadow got partial rects wrong).
  Enable per VM with `[display] virgl = true`.

  **Phase 2 (ADR-0004's 2026-08-20 amendments), three things to know:**
  - **Fences are real.** A fenced `SUBMIT_3D`/`TRANSFER_*_3D` keeps its chain
    out of the used ring until the host fence retires. Completion has to happen
    on the device's worker thread (EGL is thread-affine), so the renderer asks
    to be *called* via `virtio_core::HostWaker` — implemented in
    `machine_x86::notify` as a write to **queue 0's existing eventfd**, i.e. a
    wake is an ordinary `queue_notify(0)`. `DeferredWaker` covers the ordering
    (the device is inside its transport before the worker exists). Bounds that
    are load-bearing: `MAX_PENDING_FENCES` (64, checked *before* asking the
    renderer for a fence), a 2 s watchdog (`FENCE_TIMEOUT`) so a stalled host
    never becomes a stalled guest, immediate answers for failed fenced
    commands, and a reset that drops everything held. `ENTANGLED_GPU_FENCES=sync`
    forces phase 1 back for a measurement.
  - **The renderer runs in another process by default** (`[display]
    virgl_isolation = "process"`, GPU-012). `virtio_gpu::remote` is the whole
    story: a portable protocol (builds/tests on Windows too), a client that
    turns any wire failure into a dead renderer, and a helper (`entangled
    gpu-renderer`, socket on stdin). **The helper never sees a guest address** —
    the client ships bytes it read through `vm-memory` and the helper keeps a
    per-resource shadow `GuestMem` region. When it dies the device releases
    every held response as `ERR_UNSPEC`, drops the renderer, refuses 3D in band,
    keeps 2D working, and *then* raises `DEVICE_NEEDS_RESET`.
  - **The device measures frame pacing** (`virtio_gpu::pacing`): a
    `RESOURCE_FLUSH` on the scanout resource is a guest present, so flush
    intervals are the host's frame clock, logged every 120 frames with the
    fence statistics. Use it for any GPU before/after.

  Tests: `tests/gpu_3d.rs` (transport-level, any OS), `tests/gpu_fence.rs`
  (deferral, cap, watchdog, reset, renderer death — any OS),
  `tests/gpu_remote.rs` (a real helper process, including `SIGKILL` under
  load — unix), `tests/virgl_host.rs` + `virgl_fence_host.rs` +
  `virgl_scanout_host.rs` (real GL, one binary each because virglrenderer is a
  process singleton, all self-skipping), `boot-tests/virgl_gnome.rs` (GNOME
  live on virgl, `--ignored`), fuzz targets `gpu_3d_commands` (now including
  the fence surface) and `gpu_remote_protocol`.
- **input** (EPIC 9): event model in `virtio_input` (`ev`, `abs`, `btn`,
  `InputEvent`). Absolute pointer: window coords →
  `InputEvent::abs_from_window` (0..=32767). Every batch ends with
  `InputEvent::SYN_REPORT`. On focus loss release all pressed keys (MVP-906).

## Testing

- Pure-logic tests (validation, negotiation, parsing) run everywhere; queue
  round-trip tests need `vm-memory` mock memory (see `virtio-queue`'s own
  `mock` module — usable in dev-dependencies).
- Every device needs "malicious guest" tests: looped chains, out-of-range
  indices/addresses, oversized requests (MVP-309, MVP-1402 fuzzing comes on
  top with `cargo-fuzz` targets under `fuzz/`).
- Kernel-facing verification happens in WSL: boot a test initramfs with the
  device on the cmdline and probe from the guest (see vm-testing skill).
- **A change to shared transport state must be exercised on both transports.**
  `virtio-block` is the reference pair: `tests/blk_queue.rs` (mmio) and
  `tests/blk_pci.rs` (pci) drive the same untouched device. The boot harness takes
  `BootSpec::transport`, and `repeat_boot` takes `ENTANGLED_BOOT_TRANSPORT=pci`,
  so endurance and leak accounting cover both buses too.
- For the pci transport, "the read succeeded" is **not** evidence that
  interrupts work — a driver finds used buffers whenever anything else wakes it.
  The guest probe reports `irqs=` from `/proc/interrupts` for exactly this
  reason; assert on it.
- And with MSI-X, a climbing `irqs=` is **not** evidence that *MSI-X* worked:
  every other symptom is identical on INTx. The probe also reports `irqmode=`
  (the `/proc/interrupts` controller column), `irqnames=` (per-source
  `virtio0-config` / `virtio0-req.0` vs a shared `virtio0`) and `msix=` (entries
  in sysfs `msi_irqs/`, which exist only once the kernel enabled MSI-X). Assert
  on all three, and boot the INTx variant too — `PciInterruptMode::IntxOnly`,
  because a guest offered MSI-X will never pick INTx on its own.
