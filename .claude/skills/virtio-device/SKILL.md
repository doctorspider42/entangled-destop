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

## Transport implementation notes (MVP-301…307)

- One 4 KiB mmio slot per device from `machine_x86::layout::virtio_mmio_slot(n)`,
  IRQs from `VIRTIO_MMIO_FIRST_IRQ + n`, announced to the guest via
  `virtio_core::mmio::cmdline_clause`.
- Queue notify: register an `ioeventfd` on `slot_base + QUEUE_NOTIFY` so the
  guest write doesn't take a full VM exit round-trip through the vCPU thread.
  **Not done yet (MVP-307):** `MmioTransport::queue_notify` currently runs the
  device inline on the vCPU thread that took the MMIO exit. The single entry
  point and `DeviceResources` are already shaped so the switch to an eventfd
  plus a per-device worker thread needs no interface change — see the TODO in
  `virtio_core::transport`.
- Interrupts: `irqfd` bound to the device's GSI; set `INTERRUPT_STATUS`
  bit `INT_VRING` before signaling, clear on `INTERRUPT_ACK` write.
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
