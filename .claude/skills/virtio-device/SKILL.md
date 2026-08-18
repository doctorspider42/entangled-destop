---
name: virtio-device
description: Implementing virtio devices and the virtio-mmio transport for Entangled Desktop — virtqueues, feature negotiation, irqfd/ioeventfd, and the untrusted-guest safety rules (backlog EPICs 3, 4, 5, 8, 9; crates virtio-core, virtio-block, virtio-net, virtio-gpu, virtio-input). Load before any virtio work.
---

# VirtIO devices

Scope: the transport (EPIC 3) and every device crate. The transport and
shared safety live in `crates/virtio-core`; one crate per device.

## Architecture contract

- Devices implement `virtio_core::VirtioDevice` and see **queues, features,
  config space** — never transport registers. The mmio transport (and the
  post-MVP pci transport) owns registers, status and queue plumbing.
- Modern interface only: `VIRTIO_F_VERSION_1` is mandatory, no legacy mode,
  mmio `VERSION = 2`. Register offsets are in `virtio_core::mmio` — use the
  constants, never magic numbers.
- Ring parsing comes from the `virtio-queue` crate; Entangled Desktop policy on top of
  it lives in `virtio_core::chain`.

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
| `virtio_gpu::MAX_COMMAND_BYTES` | ≈256 KiB | bytes gathered for one controlq command | `virtio_gpu::device::tests::gather_request_refuses_a_chain_over_the_command_cap`, `gpu_queue::oversized_commands_are_rejected_without_staging_them` |
| `virtio_gpu::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick (controlq and cursorq) | same shape as the blk budget test |
| `virtio_input::MAX_PENDING_EVENTS` | 1024 | host-buffered input events while the guest is not draining (oldest dropped) | `input_queue::a_starved_queue_buffers_events_up_to_the_bound_and_drops_the_oldest` |
| `virtio_input::config::PAYLOAD_MAX` | 128 | config-space payload bytes | `virtio_input::config::tests::{bitmap_drops_codes_beyond_the_payload, from_slice_truncates_at_the_payload_size}` |
| `virtio_net::MAX_FRAME_LEN` / `MAX_BUFFER_LEN` | 1514 / 1526 | bytes staged per frame, either direction | `net_queue::{tx_oversized_frames_are_dropped, an_oversized_host_frame_is_dropped_before_the_ring, rx_chain_too_small_for_the_frame_drops_it}` |
| `virtio_net::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick | same shape as the blk budget test |
| `machine_x86::virtio::MAX_VIRTIO_SLOTS` | 8 | devices on the mmio bus (IOAPIC pins) | `queue_notify::attaching_more_devices_than_slots_is_refused` |
| `machine_x86::notify::MAX_OFFLOADED_QUEUES` | 16 | ioeventfds and epoll slots one device may demand | `queue_notify::queue_notify_offload_is_capped_per_device` |
| `machine_x86::serial::RX_CAPACITY` (private) | 4096 | buffered guest serial input | `machine_x86::serial::tests::rx_overrun_is_bounded` |
| `display::MAX_PENDING_BATCHES` / `MAX_PENDING_CONTROL` | 256 / 64 | un-drained host input batches / control events | `display::input::tests::{queue_drops_the_oldest_batch_when_the_guest_stalls, control_queue_is_bounded}` |

Two more bounds are load-bearing but not ours to name: `virtio-queue` refuses a
ring whose available count exceeds the queue size (so a guest cannot jump
`avail_idx` arbitrarily far ahead — asserted in the blk budget test), and every
pixel buffer is allocated with `try_reserve_exact`, so a cap the host cannot
satisfy becomes `ERR_OUT_OF_MEMORY` rather than an abort.

## Transport implementation notes (MVP-301…307)

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

## Per-device references

- **blk** (EPIC 4): request = header (type/reserved/sector) + data + status
  byte. Types in `virtio_block::RequestType`; status `S_OK/S_IOERR/S_UNSUPP`.
  Unknown type → `S_UNSUPP`, out-of-range → `S_IOERR` (test exists).
- **net** (EPIC 5): TX before RX (easier to debug); `virtio_net_hdr` is 12
  bytes with num_buffers when MRG_RXBUF — MVP negotiates **no offloads, no
  mergeable buffers, no multiqueue**: correct first, fast later. MAC from
  `virtio_net::MacAddr::derive(vm_name)` unless pinned in config.
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
