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
| Register window | one 4 KiB slot from `layout::virtio_mmio_slot(n)` | one 16 KiB memory BAR from `layout::pci_bar_slot(n)`, found via the capability list |
| Access widths | 32-bit aligned only | 1/2/4/8 bytes, per field |
| Config access | none (the window *is* the device) | mechanism #1 on `0xcf8`/`0xcfc`; no ECAM (needs an ACPI MCFG we do not publish) |
| Queue kick | `QUEUE_NOTIFY`, queue index in the value | notification area, `notify_off_multiplier = 4`, queue index in the **address** |
| ioeventfd | one shared address + 4-byte datamatch on the index | one address per queue, **no datamatch** (so any write width works) |
| Interrupt | single IRQ, `INTERRUPT_STATUS` + write-to-`INTERRUPT_ACK` | INTx, ISR byte, **read-to-clear**; `INTX_DISABLE` honoured |
| Guest kernel needs | `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` | `CONFIG_PCI` + `CONFIG_VIRTIO_PCI` |
| UEFI | unusable — EDK2 CloudHv ships no virtio-MMIO driver | **required** for an ISO boot (ADR-0003) |

Both use `virtio_core::LineInterrupt` unchanged: the ISR bits and the mmio
`INTERRUPT_STATUS` bits are the same two bits in the same positions.

### virtio-pci BAR layout

One BAR, four page-aligned regions, published as four vendor-specific PCI
capability records (`pci::capability_records`). Page-sized so a region could
later get its own KVM slot without anything moving.

| Structure | `cfg_type` | BAR offset | Length |
|---|---:|---:|---:|
| common configuration | 1 | `0x0000` | `0x1000` (60 bytes used) |
| ISR status | 3 | `0x1000` | `0x1000` (1 byte) |
| notification area | 2 | `0x2000` | `0x1000` (one dword per queue) |
| device configuration | 4 | `0x3000` | `0x1000` |

Identity: vendor `0x1af4`, device `0x1040 + virtio type`, revision 1, subsystem
vendor `0x1af4` (Linux reads the virtio vendor id from *that* field), INTA#.
`pci::device_id` and `pci::class_code` are free functions because the machine
builds a device's config space before its transport exists and the two must
agree.

### Known INTx limitation

The machine publishes ISA interrupt sources in its MP table, not PCI ones, so
Linux logs `can't find IRQ for PCI INT A; probably buggy MP table` and then keeps
the GSI the host wrote into `interrupt_line`. That works — the acceptance boot
asserts both the GSI and a non-zero interrupt count — but it means:

- pins are **never shared** (one device per pin, bounded by `MAX_PCI_DEVICES`),
  because the injection is an edge through an irqfd, not a level-triggered
  `INTA#` that the ISR read would deassert;
- publishing PCI interrupt routing (MP table entries, or an ACPI MADT + `_PRT`)
  is the clean fix, and MSI-X would make the question disappear.

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
| `virtio_core::pci::MAX_NOTIFY_QUEUES` | 1024 | *derived* (notify region ÷ multiplier); queues a device may expose on pci, since each needs its own notification address | `virtio_core::pci::tests::a_device_with_more_queues_than_notify_slots_is_refused` |
| `virtio_core::pci::VIRTIO_PCI_BAR_SIZE` | 16 KiB | guest-addressable register space per pci device; every capability's `offset + length` must fit | `virtio_core::pci::tests::capability_records_describe_the_real_bar_layout`, `regions_do_not_overlap_and_the_common_struct_fits` |
| `machine_x86::pci::MAX_PCI_DEVICES` | 9 | config spaces, BAR windows and IOAPIC pins on the root bus (8 devices + the host bridge) | `machine_x86::pci::tests::the_bus_is_bounded` |
| `machine_x86::layout::PCI_MMIO_SLOTS` | 8 | 16 KiB aperture slots at `0xc000_0000`; one per device, and `locate_mmio` decodes nothing outside the aperture | `machine_x86::pci::tests::{addresses_outside_the_aperture_are_never_claimed, a_bar_moved_out_of_the_aperture_decodes_nothing}` |
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
- MSI-X is **absent, not partial**: no capability at all. If you add it, the
  capability must be complete, the vector fields must stop reading
  `VIRTIO_MSI_NO_VECTOR`, and `LineInterrupt` needs a per-vector sibling.

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
- **A change to shared transport state must be exercised on both transports.**
  `virtio-block` is the reference pair: `tests/blk_queue.rs` (mmio) and
  `tests/blk_pci.rs` (pci) drive the same untouched device. The boot harness takes
  `BootSpec::transport`, and `repeat_boot` takes `ENTANGLED_BOOT_TRANSPORT=pci`,
  so endurance and leak accounting cover both buses too.
- For the pci transport, "the read succeeded" is **not** evidence that
  interrupts work — a driver finds used buffers whenever anything else wakes it.
  The guest probe reports `irqs=` from `/proc/interrupts` for exactly this
  reason; assert on it.
