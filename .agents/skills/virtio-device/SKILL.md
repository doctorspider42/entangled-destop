---
name: virtio-device
description: Implementing virtio devices and both virtio transports (mmio and pci) for Entangled Desktop — virtqueues, feature negotiation, PCI config space, irqfd/ioeventfd, and the untrusted-guest safety rules (backlog EPICs 3, 4, 5, 8, 9, 19, 21; crates virtio-core, virtio-block, virtio-net, virtio-gpu, virtio-input, virtio-sound). Load before any virtio work.
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
6. **A query is not malformed input.** A guest asking "what about value X?"
   for a value some spec or extension defines but the device does not serve
   gets "unsupported" or "nothing", never a dead context; only *creating*
   something of that value is refused. The Venus executor learned this from
   Zink, whose format table probes `VK_FORMAT_A1B5G5R5_UNORM_PACK16_KHR`
   (maintenance5) at startup: a fatal answer killed every GL client
   (ADR-0004, S2b amendment; `venus::executor::query_tests`).

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
| `virtio_block::MAX_DISCARD_SEG` | 256 | segments in one DISCARD / WRITE_ZEROES array (also the guest-visible `max_discard_seg`); `MAX_DISCARD_ARRAY_BYTES` is the derived 4 KiB staging bound | `virtio_block::request::tests::the_segment_array_length_must_describe_whole_segments_and_not_too_many`, `blk_queue::a_malformed_segment_array_is_unsupported_and_too_many_segments_is_an_io_error` |
| `virtio_block::MAX_DISCARD_SECTORS` | 2Mi (1 GiB) | sectors one discard segment may cover — one punch syscall regardless of size | `virtio_block::request::tests::the_per_command_maximum_is_enforced_and_differs_by_command`, `blk_queue::out_of_range_and_oversized_segments_are_io_errors` |
| `virtio_block::MAX_WRITE_ZEROES_SECTORS` | 64Ki (32 MiB) | sectors one write-zeroes segment may cover; **tighter than discard on purpose** — the zero-writing fallback has to write every byte | same tests |
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
| `virtio_gpu::blob::MAX_BLOB_RESOURCES` | 8192 | live blob resources (VEN-2001); above the Venus renderer's own blob caps so theirs bind and name the client | `blob::tests::the_resource_count_and_byte_budgets_hold`, fuzz target `gpu_blob` |
| `virtio_gpu::blob::MAX_BLOB_BYTES` | 1 GiB | size of one blob; also the size the helper re-checks on its own side | `blob::tests::sizes_must_be_whole_pages_inside_the_budget`, `gpu_blob::a_malicious_blob_guest_is_answered_in_band` |
| `virtio_gpu::blob::MAX_TOTAL_BLOB_BYTES` | 8 GiB | bytes promised across every live blob (a Venus guest's 1 GiB of host blobs and 2 GiB of host-visible memory, plus exported device-local buffers) | `blob::tests::the_resource_count_and_byte_budgets_hold` (asserts the budget is exact) |
| `virtio_gpu::blob::MAX_BLOB_ENTRIES` | 16384 | page-list entries in one `RESOURCE_CREATE_BLOB` | `gpu_blob::a_malicious_blob_guest_is_answered_in_band` (a lying `nr_entries` and an overflowing one) |
| `virtio_gpu::blob::MAX_HOST_VISIBLE_MAPPINGS` | 4096 | live mappings the host-visible window tracks; never below `vmm_core::MAX_HOST_RANGES` | fuzz target `gpu_blob`, `entangled::run_vm::tests::one_venus_client_at_its_shares_leaves_most_of_the_window_ranges` |
| `virtio_gpu::venus::renderer::VENUS_HOST_VISIBLE_BYTES` | 4 GiB (profile: `[display] host_visible_mib`, 64..=4096, a power of two) | the Venus host-visible window, guest-visible as BAR 2; holds every host blob and host-visible byte the budgets admit, so it never bites first. No host pages behind it | `venus::renderer::tests::the_default_window_holds_everything_the_budgets_admit_and_a_profile_may_resize_it`, `control_api::config::tests::the_host_visible_window_is_a_power_of_two_inside_the_bar_cap` |
| `vmm_core::shm::MAX_HOST_RANGES` | 4096 | renderer ranges (hypervisor mappings) in one window; four times what one Venus context may map at its shares | `vmm_core::shm::tests::{renderer_ranges_are_bounded, a_gpu_composited_desktop_fits_in_the_renderer_ranges}` |
| `virtio_gpu::venus::executor::MAX_HOST_VISIBLE_BYTES` / `_PER_CONTEXT` | 2 GiB / 1 GiB | host pages behind host-visible Vulkan memory, and host RAM the driver allocates for a heap that is not device local, every context / one context (`PageBudget::share`) | `venus::shmem::tests::a_budget_share_is_bounded_by_itself_and_by_the_whole`, `executor::memory_tests::one_context_cannot_take_the_host_visible_budget_from_the_rest` |
| `virtio_gpu::venus::renderer::MAX_MEMORY_BLOBS` / `_PER_CONTEXT` | 4096 / 1024 | blobs of `VkDeviceMemory` (each mapped one is a window range) | `executor::memory_tests::one_context_cannot_take_the_memory_blobs_from_the_rest` |
| `virtio_gpu::venus::renderer::MAX_RING_BLOBS(_BYTES)` / `_PER_CONTEXT` | 1024, 1 GiB / 64, 128 MiB | host blobs: rings, reply and command-stream pools | `venus::renderer::tests::{a_gpu_composited_desktop_of_venus_clients_fits_in_the_host_blob_budget, one_context_cannot_take_the_host_blob_budget_from_the_rest}` |
| `virtio_gpu::venus::renderer::MAX_RINGS` / `MAX_RINGS_PER_CONTEXT` | 256 / 32 | rings, and with them ring-worker threads (one TLS ring per guest thread that creates pipelines) | `venus::renderer::tests::{a_gpu_composited_desktop_and_a_game_fit_in_the_ring_caps, the_ring_caps_hold_per_context_and_overall_and_bound_the_threads}` |
| `virtio_gpu::venus::executor::timeline::MAX_FENCE_THREADS` | 256 | host fence threads, every context together (one context alone could have 63) | `executor::sync_tests::fence_threads_are_capped_across_every_context_and_given_back` |
| `virtio_gpu::venus::executor::limits::Class` (`Caps::default`) | per context / renderer-wide: objects 65 536 / 262 144; devices 4 / 64; memory objects 4096 / 16 384; pipelines, shader modules, command buffers, fences, semaphores, events 16 384 / 65 536; descriptor pools 4096 / 16 384; pipeline caches 256 / 2048; query and command pools 1024 / 4096; SPIR-V + cache bytes 256 MiB / 1 GiB; descriptors (pool `maxSets` + counts) 8 Mi / 32 Mi; query slots 1 Mi / 4 Mi; recorded command bytes 256 MiB / 1 GiB; decode bytes in flight 512 MiB / 2 GiB; held submits (and the ring fences behind them) 1024 / 4096 and their wire bytes 16 MiB / 64 MiB | every host object and every cost one carries, charged by the table entry that holds it (`objects::KindTable`), so every implicit free refunds; exhausted → the create's `VkResult` (`OUT_OF_HOST_MEMORY`, `TOO_MANY_OBJECTS` for memory, `OUT_OF_DEVICE_MEMORY` for descriptors and queries), or the context ends where the command has none (a `vkCmd*`, a decode) | `executor::limits_tests::*`, `executor::limits::tests::*` |
| `virtio_gpu::venus::executor::limits::DEVICE_LOCAL_WHOLE` / `DEVICE_LOCAL_SHARE` (profile: `[display] gpu_memory_mib`, 256..=1 Mi) | ¾ of each device-local heap / ¾ of that | plain `vkAllocateMemory` of VRAM, per heap of each GPU, held by the memory object and by any handle blob or import of it; the guest's heap size is the share (`policy::guest_heaps`); a heap that is not device local is charged to the host-visible share | `executor::limits_tests::device_local_memory_is_refused_at_the_share_and_the_whole_and_given_back`, `s1_tests::a_handle_blob_keeps_its_device_local_charge_until_it_goes`, `host_vulkan::pipeline_tests::allocating_past_the_device_local_cap_is_out_of_device_memory_and_the_gpu_keeps_working` (real GPU) |
| `virtio_gpu::venus::executor::limits::MAX_COMMAND_DECODE_BYTES` | 256 MiB | host bytes one command's decode may allocate (also taken from the context's `DecodeBytes` share while it runs, with the copies `vkExecuteCommandStreamsMESA` makes) | `executor::limits_tests::{one_commands_decode_is_bounded_whatever_count_it_declares, a_decode_past_the_pool_ends_its_context_and_gives_the_pool_back}`, `wire::tests::a_pooled_decode_takes_from_the_pool_and_gives_it_back` |
| `virtio_gpu::venus::executor::hold` (`Class::HeldSubmits`, `Class::HeldBytes`, `hold::NAP`) | 1024 / 4096 held items, 16 MiB / 64 MiB; 1 ms naps | guest submits whose waits no signal the driver already has covers, held on the host with the rest of their queue (and its ring fences) until covered; a host-side wait whose answer is behind held work naps off the lock instead of asking the driver; past a cap the context ends | `executor::hold_tests::*` (every one asserts `FakeVulkan::unsatisfiable_waits` is empty), `host_vulkan::pipeline_tests::a_wait_before_its_signal_never_reaches_the_driver_and_runs_once_signalled` (real GPU, child process) |
| `virtio_gpu::venus::executor::objects::TEARDOWN_WAIT` | 500 ms | how long a context teardown waits for its devices' GPU work (`HostVulkan::device_idle_within`) before it parks them, still charged, in the `Graveyard` | `executor::limits_tests::a_context_whose_gpu_work_never_finishes_is_parked_not_waited_for` |
| `virtio_gpu::blob::BLOB_PAGE_SIZE` | 4096 | granularity every blob size and map offset must be a multiple of | `blob::tests::window_reservations_cannot_overlap_or_run_off_the_end` |
| `virtio_gpu::MAX_COMMAND_BYTES_BLOB` | 256 KiB + 56 B | gather cap once blob resources are offered — 24 bytes above the 2D cap, because a full-length `RESOURCE_CREATE_BLOB` really is 24 bytes longer than a full-length attach-backing | `virtio_gpu::device::tests::command_buffer_bound_matches_the_entry_limit` (compile-time `assert!`s in `device.rs`) |
| `virtio_gpu::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick (controlq and cursorq) | same shape as the blk budget test |
| `virtio_input::MAX_PENDING_EVENTS` | 1024 | host-buffered input events while the guest is not draining (oldest dropped) | `input_queue::a_starved_queue_buffers_events_up_to_the_bound_and_drops_the_oldest` |
| `virtio_input::config::PAYLOAD_MAX` | 128 | config-space payload bytes | `virtio_input::config::tests::{bitmap_drops_codes_beyond_the_payload, from_slice_truncates_at_the_payload_size}` |
| `virtio_input::gamepad::MAX_EVENTS_PER_REPORT` | 20 | events one capture tick can push (11 buttons + 8 axes + the `SYN_REPORT`); *derived*, and what makes "one host tick is one bounded push" true by construction | `virtio_input::gamepad::tests::every_event_the_pump_can_produce_is_one_the_profile_advertises` (asserts the budget is exact) |
| `virtio_input::gamepad::evdev::MAX_SCANNED_DEVICES` | 64 | `/dev/input` nodes one rescan opens and inspects — host-owned, so a sanity bound rather than a security one | `virtio_input::gamepad::evdev::tests::one_scan_looks_at_a_bounded_number_of_nodes_and_at_the_lowest_ones` (also pins that the sort happens *before* the cap) |
| `virtio_input::gamepad::evdev::MAX_EVENTS_PER_POLL` | 256 | host `input_event` records folded into one `PadState` per tick; the remainder waits for the next one rather than being dropped | `virtio_input::gamepad::evdev::tests::one_poll_folds_a_bounded_number_of_events_and_leaves_the_rest` |
| `virtio_net::MAX_FRAME_LEN` / `MAX_BUFFER_LEN` | 1514 / 1526 | bytes staged per frame, either direction | `net_queue::{tx_oversized_frames_are_dropped, an_oversized_host_frame_is_dropped_before_the_ring, rx_chain_too_small_for_the_frame_drops_it}` |
| `virtio_net::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick | same shape as the blk budget test |
| `virtio_net::usernet::MAX_FLOWS` | 64 | concurrent guest TCP connections the NAT terminates; each is two 16 KiB socket buffers and, briefly, one connect thread | `usernet::tcp::tests::{the_flow_count_is_capped, a_full_flow_table_refuses_politely_and_then_recovers}`, fuzz target `usernet_frames` |
| `virtio_net::usernet::FLOW_IDLE_TIMEOUT` / `FLOW_KEEPALIVE` | 60 s / 15 s | how long a flow may go unanswered by the guest before it is aborted and its slot returned. Not a size bound but a *liveness* one, and load-bearing for the same reason: a guest that stops existing (reboot, reset, pause) otherwise holds all 64 for the life of the process | `usernet::tcp::tests::{a_flow_the_guest_abandons_is_retired_by_the_keepalive, a_full_flow_table_refuses_politely_and_then_recovers}` |
| `virtio_net::usernet::MAX_QUEUED_FRAMES` | 256 | frames queued for a guest that has stopped draining its RX queue | `usernet::tests::the_guest_queue_is_bounded`, fuzz target `usernet_frames` |
| `virtio_net::usernet::MAX_DNS_QUERIES` / `DNS_QUERY_TTL` | 64 / 5 s | outstanding forwarded DNS queries, and how long one is remembered — the guest picks how many it sends | `usernet::tests::a_guest_dns_query_is_forwarded_and_its_answer_comes_back` (the round trip), oldest-first eviction in `DnsRelay::forward` |
| `virtio_sound::stream::MIN_PERIOD_BYTES` / `MAX_PERIOD_BYTES` | 64 / 64 KiB | one PCM period — and therefore both the payload of one playback message and the **room one capture buffer may grant** | `virtio_sound::stream::tests::period_and_buffer_geometry_is_bounded_and_never_divides_by_zero`, `snd_queue::{set_params_refuses_everything_the_device_never_advertised, capture_messages_the_stream_cannot_accept_are_answered_not_filled}` |
| `virtio_sound::stream::MAX_BUFFER_BYTES` | 1 MiB | the host PCM ring a guest can make the device allocate, **per stream** (`buffer_bytes` + one period of slack). On TX it bounds the bytes queued; on RX it bounds the room the un-retired buffers *reserve*, which is claimed at post time | same tests, plus `virtio_sound::device::tests::the_ring_capacity_follows_the_negotiated_buffer`, `snd_queue::flooding_the_capture_ring_beyond_the_negotiated_buffer_is_an_io_error` |
| `virtio_sound::stream::MIN_PERIODS` / `MAX_PERIODS` | 2 / 1024 | periods one buffer may be divided into — the buffer must be a whole number of them | same tests |
| `virtio_sound::stream::MAX_CHANNELS` | 2 | channels one stream may carry (what the chmap describes), the same either way | `virtio_sound::stream::tests::channel_counts_are_bounded_by_what_the_chmap_describes` |
| `virtio_sound::stream::STREAMS` / `JACKS` / `CHMAPS` | 2 / 2 / 2 | items the config space advertises — one of each per direction, ids fixed as `OUTPUT_STREAM` = 0 and `INPUT_STREAM` = 1. Every id and every `*_INFO` range is checked against these, **and** every I/O message against `direction_of(id)` matching the queue it arrived on | `snd_queue::{info_queries_outside_the_advertised_range_or_with_the_wrong_record_size_are_refused, the_information_queries_answer_what_the_config_space_promised, capture_messages_the_stream_cannot_accept_are_answered_not_filled}`, `virtio_sound::stream::tests::the_two_advertised_streams_are_one_out_and_one_in` |
| `virtio_sound::MAX_CONTROL_MSG_BYTES` | 4 KiB | bytes gathered for one control message, refused *before* reading | `snd_queue::malformed_descriptor_chains_are_dropped_without_taking_the_device_down` |
| `virtio_sound::MAX_XFER_BYTES` | 4 B + 64 KiB | bytes gathered for one **playback** message (header + a period) | same test, fuzz target `snd_device` |
| `virtio_sound::MAX_CAPTURE_HEADER_BYTES` | 4 KiB | bytes gathered for one **capture** message's device-readable half. Deliberately the control cap and not `MAX_XFER_BYTES`: a capture message's readable part is a four-byte `virtio_snd_pcm_xfer` and nothing else, so a guest must not be able to make the device gather 64 KiB of padding on that queue | `snd_queue::malformed_capture_chains_are_dropped_or_refused` |
| `CaptureBuffer::room` (derived) | ≤ `MAX_PERIOD_BYTES` | the device-writable bytes one capture buffer granted, measured **once** from the chain walk (writable total minus the status word) and validated as whole frames within one period. Every write into guest memory is bounded by this *and*, segment by segment, by the length the walk reported — the single rule the RX path rests on | `virtio_sound::device::tests::{a_capture_fill_never_writes_past_the_room_the_guest_granted, a_capture_buffer_outside_guest_memory_costs_its_audio_and_nothing_else}`, fuzz target `snd_device` (no used-ring entry may exceed the writable bytes a chain offered) |
| `virtio_sound::MAX_PENDING_PERIODS` | 256 | I/O messages held un-retired, **per stream** — playback periods staged, or capture buffers waiting to be filled | `snd_queue::{flooding_the_ring_beyond_the_negotiated_buffer_is_an_io_error, flooding_the_capture_ring_beyond_the_negotiated_buffer_is_an_io_error}` |
| `virtio_sound::CHAINS_PER_NOTIFY` | 1024 | chains drained per kick, on every queue | same shape as the blk budget test |
| `virtio_sound::MAX_RECORDING_BYTES` | 16 MiB | bytes a `RecordingSink` keeps before it plays on without storing | `virtio_sound::backend::tests::a_recording_is_capped_rather_than_unbounded` |
| `machine_x86::virtio::MAX_VIRTIO_SLOTS` | 8 | devices on the mmio bus (IOAPIC pins) | `queue_notify::attaching_more_devices_than_slots_is_refused` |
| `machine_x86::notify::MAX_OFFLOADED_QUEUES` | 16 | ioeventfds and epoll slots one device may demand | `queue_notify::queue_notify_offload_is_capped_per_device` |
| `virtio_core::pci::MAX_NOTIFY_QUEUES` | 1024 | *derived* (notify region ÷ multiplier); queues a device may expose on pci, since each needs its own notification address | `virtio_core::pci::tests::a_device_with_more_queues_than_notify_slots_is_refused` |
| `virtio_core::pci::VIRTIO_PCI_BAR_SIZE` | 32 KiB | guest-addressable register space per pci device; every capability's `offset + length` must fit | `virtio_core::pci::tests::capability_records_describe_the_real_bar_layout`, `regions_do_not_overlap_and_the_common_struct_fits` |
| `virtio_core::ShmRegion::len` | non-zero | a declared shared-memory region must have a length; zero is refused at construction because "present with length 0" is the state the all-ones convention exists to avoid | `virtio_core::transport::tests::a_zero_length_shm_region_is_refused` |
| `vmm_core::shm::MAX_SHM_WINDOW_BYTES` | 4 GiB | host memory one shared-memory window may commit; a host-configuration bound, not a guest one | `vmm_core::shm::tests::a_window_must_be_whole_pages_and_bounded` |
| `machine_x86::layout::MAX_SHM_BAR_BYTES` | 4 GiB | one device's shared-memory BAR, which also bounds how far alignment can push the first allocation into the aperture. Checked on the running **sum** inside `shm::plan`'s loop, *before* `next_power_of_two` sees it — that method panics in debug and wraps to zero in release above 2^63, and a region length is not always this crate's value (an isolated renderer decodes it off the helper's pipe) | `machine_x86::shm::tests::a_plan_refuses_what_no_bar_could_carry` |
| `machine_x86::layout::PCI_MMIO64_SIZE` | 64 GiB | the 64-bit aperture, starting at the top of RAM; a BAR outside it is **never mapped**, which is what stops a guest parking host pages over its own RAM. Derived, not picked: a `const` assertion holds it at ≥ `(PCI_MMIO_SLOTS + 1) × MAX_SHM_BAR_BYTES`, since a naturally aligned allocator needs one max window per slot plus one alignment gap. It is *narrower* than EDK2's own `Pci64Size` (2^46), so a firmware that ever placed a window past `pci_mmio64_end` would get a silent refusal | `machine_x86::shm::tests::a_bar_outside_the_aperture_is_never_mapped`, `shm_bus::a_window_the_guest_moves_out_of_the_aperture_is_unmapped` |
| `virtio_core::ShmBacking` accesses | the **region's** length, not the BAR's | every host-side read/write/fill is bounded in u64 against the span the device declared, so one region cannot be reached through another's offsets | `machine_x86::shm::tests::a_region_backing_cannot_reach_another_region`, fuzz target `gpu_blob` |
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

## Shared-memory regions and blob resources (EPIC 20, VEN-2001)

A shared-memory region is a window of **host** memory the guest maps directly.
virtio-gpu needs one for host-visible blob resources, which is the reason Venus
was out of reach in ADR-0004 phase 1. The plumbing is deliberately inert unless
a device asks for it:

- A device declares regions with `VirtioDevice::shm_regions() -> Vec<ShmRegion>`
  (`{id, len}`, default empty). It never learns *where* the window lands — that
  is the transport's and the machine layer's business.
- **mmio**: `SHM_SEL` selects by `shmid`; `SHM_LEN_LOW/HIGH` and
  `SHM_BASE_LOW/HIGH` answer for a region that is declared **and** placed
  (`MmioTransport::set_shm_base(id, gpa)`). Everything else reads **all-ones**.
  Preserve that exactly: a zero looks to Linux' `virtio_gpu` like a present
  zero-length region at address 0, it tries to reserve it, and the probe fails.
  Two tests pin it (`a_device_without_shm_regions_still_reads_all_ones`,
  `a_declared_shm_region_answers_only_after_the_host_places_it`).
- **pci**: `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` (`cfg_type` 8) as a
  `virtio_pci_cap64` — 24 bytes, offset and length split across two field pairs
  — in `VIRTIO_PCI_SHM_BAR_INDEX` (**2**, not the register BAR: BAR 0 is a
  32 KiB 32-bit window sized to the register file and a host-visible region is
  hundreds of megabytes and wants to be 64-bit prefetchable). Build the records
  with `pci::shm_capability_records(&placements)`; `pci::place_shm_regions`
  packs them page-aligned and refuses a set that does not fit.
  **The machine layer must allocate that second BAR before any of this is
  published** — a capability pointing at a BAR nothing decodes is worse than no
  capability.

### Backing it with real host memory (phase 2, 2026-09-09)

The window is host pages now, on both hosts, and there are four things to know
before touching any of it:

- **`machine_x86::shm` owns the placement.** A device gets host memory through
  `VirtioDevice::set_shm_backing(id, Arc<dyn ShmBacking>)` — `len`, `read`,
  `write`, `fill`, every offset bounded in u64, no pointers and no hypervisor —
  and the backing is scoped to *that region's* span inside the BAR, not to the
  BAR. Default is a no-op, so a device that declared a region and got no
  backing behaves exactly as it did in phase 1.
- **The address is not ours to choose freely.** The 64-bit aperture starts at
  `layout::pci_mmio64_base(mem_bytes)` = the top of guest RAM, which is
  precisely what EDK2 publishes as `Pci64Base` (measured: `0x140000000` for a
  4096 MiB guest). It moves with the guest's memory size, so **never write a
  constant above 4 GiB** — high RAM is up there and it grows.
- **A BAR is guest-writable, so every placement needs a veto.**
  `ShmWindow::follow` maps only inside that aperture and otherwise leaves the
  window unmapped; "the BAR decodes nothing" always means *unmap*. That sweep
  runs on **both** hosts, unlike `crate::notify`'s ioeventfd rebase, because a
  stale mapping is host memory at an address the guest has reused — not a lost
  kick. A machine reset unmaps too.
- **Sweep in two passes: release every window, then claim.**
  `ShmWindow::release_for` then `ShmWindow::claim`, never `follow` in a loop.
  `PciBusDxe` permutes BAR addresses, so one function's new address is another's
  old one, and both hypervisors refuse an overlapping range (KVM an overlapping
  memory slot, WHP a failed `WHvMapGpaRange`). One pass leaves the loser
  **unmapped with nothing to retry it** — worse than the ioeventfd case, where
  the next sweep picks it up. Regression:
  `shm_bus::two_windows_that_swap_addresses_both_end_up_mapped`, which uses a
  `GpaMapper` that refuses overlaps the way a real one does.
- **A span handed to a guest is zeroed, and only that span.**
  `BlobTable::reserve_mapping` clears before it returns, so it is a property of
  the table rather than of one caller; clearing past either end would wipe a
  neighbour's bytes under a guest that is using them. Both halves are fuzzed
  (`gpu_blob`).

`vmm_core::shm::UnmappedGpaMapper` is what makes all of this testable with no
hypervisor at all — `crates/machine-x86/tests/shm_bus.rs` runs on both hosts in
a second. The real thing with a real guest is `tests/boot/tests/pci_shm.rs`.

Blob resources themselves (`virtio_gpu::blob`) are the device half:

- Three memory types. `BLOB_MEM_GUEST` is guest pages and needs no renderer at
  all; `BLOB_MEM_HOST3D` is a renderer allocation named by `blob_id`;
  `BLOB_MEM_HOST3D_GUEST` is both. Unknown types and unknown flag bits are
  **refused, never ignored** — ignoring a "use" flag hands the guest a resource
  that silently cannot do what it asked for. (Phase 1 guessed the venus command
  ring would be a `BLOB_MEM_GUEST` blob. It is not — see the Venus section
  below — so a guest blob still never reaches a renderer.)
- The blob table is a *third* owner in the one id namespace. Every existing
  command routes by ownership (the rule ADR-0004's mixed-namespace amendment
  established): attach/detach-backing and `TRANSFER_*_3D` are refused for a
  blob, `CTX_ATTACH_RESOURCE` is accepted, `SET_SCANOUT` is refused in favour of
  `SET_SCANOUT_BLOB`, and `RESOURCE_UNREF` releases a window span the guest
  forgot to unmap.
- `RESOURCE_MAP_BLOB` carries the sharpest guest value in the epic: an offset
  into a *host* mapping. `HostVisibleWindow::reserve` checks it in u64 against
  the window length, the page grid and **both** neighbouring mappings before
  the renderer sees it, and the caller rolls the reservation back if the
  renderer refuses. Never let a reservation outlive a failed map.
- Feature bits follow capability, not hope: `VIRTIO_GPU_F_RESOURCE_BLOB` and
  `VIRTIO_GPU_F_CONTEXT_INIT` are offered only when the attached renderer's
  `BlobSupport` / capsets justify them, so a virgl-only host is byte-identical
  to before.

### Venus, and the second window mode (VEN-2003, 2026-09-10)

A **renderer-mapped** window is the other shape a shared-memory region can
take, and the two cannot coexist in one BAR — they overlap, and both
hypervisors refuse that. Read this before touching `vmm_core::shm`,
`machine_x86::shm` or `virtio_gpu::blob`:

- The mode travels renderer → device → machine: `BlobSupport::host_mapped` →
  `ShmRegion::host_mapped` → `SharedWindow::new_host_mapped`. In that mode
  **nothing is mapped until a blob is**, the window's own pages are never shown
  to the guest, and `ShmBacking::read/write/fill` are **refused** rather than
  silently landing in memory no guest reads. Branch on
  `ShmBacking::host_mapped()`, never on getting an error.
- The renderer puts host memory in through `unsafe ShmBacking::map_host(offset,
  host_addr, len)`, whose safety contract is the whole design: *those pages stay
  mapped at that address until `unmap_host` returns*. It reaches the hypervisor
  as `vmm_core::HostRange`, whose only general constructor is `unsafe` for the
  same reason.
- **Order is the safety property.** Every teardown path takes the *guest*
  mapping down before the renderer frees the pages —
  `VirglRenderer::take_mapping_down` is the one place that does it, and
  `unmap_blob`, `destroy_blob`, `reset` and `Drop` all go through it. A device
  reset unmaps immediately (safe from any thread) even though the library-side
  teardown is deferred to the worker thread.
- `MAX_HOST_RANGES` (`vmm_core`, 4096) bounds **hypervisor objects**, not
  bookkeeping: KVM reserves `1 + MAX_HOST_RANGES` memory-slot numbers per window
  (fewer when `KVM_CAP_NR_MEMSLOTS` is short, keeping 16 for the ROMs mapped
  after it) and allocates from that pool, so a guest that maps blobs without
  unmapping gets an in-band failure rather than a VMM out of slots somewhere
  else.
- **Size a guest-facing cap for a desktop, not for one client.** With GNOME
  composited through Zink every GL client is a venus instance that keeps its
  rings, 8 MiB command-stream chunks and mapped memory for life. Four clients
  held 69 window ranges and 75 MiB of host blobs, past the old 64 / 64 MiB caps.
  vkcube died on its next swapchain, and nothing said why: the guest kernel
  does not wait for `RESOURCE_CREATE_BLOB`'s answer, so a refused blob shows
  up only as a later `mmap` `EINVAL`. Give a per-client share *and* a global
  cap (the Venus renderer's `MAX_RING_BLOB*_PER_CONTEXT`), and measure the
  real desktop before choosing the numbers (ADR-0004, amendment on caps sized
  for one client).
- **Measure with the usage log, and read a runaway for what it is.** The
  renderer logs every cap-bound quantity, current and peak, at debug on target
  `virtio_gpu::venus::usage` (`VenusRenderer::usage` / `peak_usage`) when a
  context comes or goes, when a peak rises and every 10 s when it changed.
  With GNOME on zink, gnome-shell sometimes starts allocating a fresh
  host-visible buffer of a client's frame size 60–80 times a second and frees
  none, until whichever cap comes first — 1 GiB, 3 GiB, the window's ranges. No budget absorbs that; the
  per-context shares are what keep such a client from taking everything from
  everyone else (ADR-0004, the capacity amendment).
- **Every host object a guest makes is charged where it is held**
  (ADR-0004, the resource-exhaustion amendment). The executor's caps live
  in one place, `venus::executor::limits`: a per-context share and a
  renderer-wide whole per class (`PageBudget::share`, as the host-visible
  pages always were), charged by the object table's entry
  (`objects::KindTable`) — so a pool freeing its children, a device taking
  everything with it, a context going and a reset all refund, with no
  bookkeeping to forget. A new create path reserves before the host call
  (`Objects::reserve`, released after every command) and answers the
  refusal's `VkResult`; a new cost an object carries goes on its entry
  (`create_costing`, `charge_extra`); new device memory holds its charge in
  the memory object (`MemoryObject::charge`), shared with anything that keeps
  the allocation alive. Mesa allocates and creates asynchronously, so a
  refusal the guest never reads ends that client's context at its next use
  of the id — the client, not the renderer.
- **A destroy never waits for the GPU** (ADR-0004, the amendment on CSS
  pages). Every queue has a `objects::Clock`: each submit that reaches the
  driver takes a serial, the guest's fence stands for it when there is one,
  and a *mark* (an empty submit with a host fence of ours) is added only when
  something needs to know about serials nothing covers yet. A destroy or
  free with work in flight takes the object out of the table at once and
  **dooms** it (`objects::Doomed`): destroyed on the host once every serial
  in flight at the time has finished (`vkGetFenceStatus`, never a wait), at
  device teardown at the latest, still charged to its caps until then. New
  destroy paths go through `Objects::take_to_doom` + `retire_object`, never
  `settle` + an immediate host destroy: Zink destroys hundreds of buffers a
  frame, each of a batch that has finished while the next one runs, and the
  old wait-for-the-queue before each put a whole frame of GPU time into the
  ring (a third of a CSS page's ring time in Firefox). What must see finished
  work before it acts — a pool reset, freeing command buffers or descriptor
  sets, an event reset something waits on, a fence reset — still `settle`s,
  on the clock's fences, never on the driver's unbounded queue idle.
- **Profile a frame before guessing** (ADR-0004, the amendment on CSS pages):
  `RUST_LOG=info,virtio_gpu::venus::profile=debug` logs every 2 s, per ring,
  each command's count, host time, longest and replies, the decode time and
  the driver's share (`venus ring commands`); per ring worker the time
  pumping, yielding, sleeping, parked and blocked (`venus ring worker`); per
  context the device worker's transport and blob calls, its
  `vkWaitRingSeqnoMESA` waits included (`venus device calls`); and the
  scanout flips per second (`venus scanout flips`). Off, it costs one branch
  per command. **A page's `requestAnimationFrame` rate is not what the user
  sees**: Firefox's compositor animates CSS off the main thread, and with
  software WebRender a CSS page ticked rAF at 52 fps while GNOME flipped 4.4
  frames a second. Count flips for a window that is the only thing moving.
- **A teardown never waits for the GPU without a bound.** A queue whose work
  waits on a timeline value nothing signals never goes idle, and the thread
  tearing a context down is the device's worker: `destroy_all` waits
  `TEARDOWN_WAIT` and parks what is still busy (`executor::Graveyard`,
  reaped whenever a context comes or goes and at the usage look). On the
  RTX 2070 a test process with such a wait on one device and GPU work on
  another hung twice in seven runs, in the driver, unkillably: do not write
  a real-GPU test that leaves a wait-before-signal pending.
- **The driver never sees a wait it cannot meet** (ADR-0004, the
  wait-before-signal amendment of 2026-09-26). Every submit goes through
  `executor::hold`: a timeline wait past every signal the *driver* has been
  given (`SemaphoreState::host_value`), a binary wait whose signal is itself
  held (`host_pending`), a `vkCmdWaitEvents` on an event nobody set — each is
  held host-side with everything behind it on its queue, ring fences
  included, and released in order when a covering signal is submitted
  (another queue, `vkSignalSemaphore`, `vkSetEvent`). Keep the guest's order
  (`pending`, `temporary`) and the driver's (`host_*`) apart: they differ
  exactly while something is held. Anything new that submits to a guest
  queue goes through `VulkanContext::enqueue`, never straight to the driver;
  anything new that waits in the driver must be bounded or emulated (the
  query `WAIT` bit is polled, not passed), and a wait whose answer is behind
  held work naps (`hold::NAP`) off the context lock. Test against the fake:
  `FakeVulkan::unsatisfiable_waits` lists every wait the fake driver was
  handed that nothing submitted before it could satisfy, and must stay
  empty. A real-GPU test of it runs in a child process with a kill timeout
  (`pipeline_tests::a_wait_before_its_signal_never_reaches_the_driver_and_runs_once_signalled`).
- **A timed wait on Windows is not the time it asks for.** A
  `std::thread::sleep` of 10–160 µs lasts 0.35–0.6 ms there, and a
  `Condvar::wait_timeout` of 1–5 ms lasts 15.6 ms (the system tick); on Linux
  (WSL) both land within ~0.1 ms of the request. Anything that polls guest
  memory on a latency path must yield, not sleep, for the window that
  matters: the Venus ring worker yields for `venus::service::HOST_SPIN`
  (2 ms on Windows, zero on Linux) after its last progress, and that alone
  took an empty `vkQueueSubmit` round trip from 1.5–2 ms to 0.4 ms and
  glmark2 on Zink from ~150 to ~450 FPS (ADR-0004, the GPU-time amendment of
  2026-09-25). Put numbers on a host-side wait's real length before trusting
  its backoff.
- **Read GPU timestamps next to the GPU's P-state.** A native process gets
  full clocks for ~2 s from its first device; a guest's device is a late
  device in a long-lived VMM, and the RTX 2070 ran vk-smoke's sparse guest
  work at P8 (300 MHz core, 405 MHz memory): 3–10× the native timestamps, all
  of it clocks. `nvidia-smi --query-gpu=pstate,clocks.gr,clocks.mem
  --format=csv -lms 50` on the host shows it; `host_vulkan::perf_tests`
  measures every memory placement the renderer can give a guest.
- **A host-mapped window has no pages of its own.** `SharedWindow::
  new_host_mapped` allocates nothing, so a Venus window's size costs
  guest-physical address space only; on Windows an untouched
  `VirtualAlloc(MEM_COMMIT)` of it would still have been commit charge.
- An **isolated** renderer (GPU-012) withholds the window entirely
  (`host_visible_bytes: None`): an `Arc` does not cross a pipe and a pointer in
  the helper's address space names nothing in the VMM's. The guest is told at
  create time, not at map time.
- Venus needs `virgl_renderer_init` with `VENUS | RENDER_SERVER` **together** —
  the flag alone does nothing in virglrenderer 1.1, every Venus entry point
  checks `state.proxy_initialized`. The decoder therefore runs in a subprocess
  virglrenderer spawns, whatever `[display] virgl_isolation` says.
- `Renderer3d::create_blob` takes the header's `ctx_id`, because a host blob is
  resolved *through the context that asked for it*.
- Tests: `virtio-gpu/tests/venus_host.rs` (real library, self-skipping),
  `machine_x86::shm::tests::a_renderer_mapped_window_places_renderer_pages_at_the_regions_offset`
  (both hosts, no hypervisor), fuzz target `venus_window`,
  `boot-tests/venus_vulkan.rs` (`--ignored`, a real guest).
- The library is not packaged anywhere: build it with
  `guest/virglrenderer/build-virglrenderer.sh` and point `ENTANGLED_VIRGL_LIB`
  at the result. Without it every Venus probe answers "no" and the device is
  byte-identical to a virgl-only one.

### Attaching the Venus executor (this VMM's own renderer, both hosts)

- **The product switch is `[display] venus = true`** (ADR-0004, "how a user
  turns it on"). It is refused beside `virgl = true`: one device, one
  `Renderer3d`. It is serialized only when true, because an older engine
  denies unknown keys. Reason with `control_api::GpuRenderer`
  (`DisplaySection::gpu_renderer`/`set_gpu_renderer`), never with the two
  booleans.
- `run_vm::gpu_plan` is the one place the choice is made, and it is
  unit-tested. `ENTANGLED_VENUS_CAPTURE` wins over everything.
  `ENTANGLED_VENUS=vulkan` is a **developer override** that puts Venus under
  any profile and warns what it replaced. Use it for installer runs and A/B
  comparisons on one file; a profile a user runs says `venus = true`.
- The attach line names who asked: `attaching the Venus EXECUTING renderer …
  source=[display] venus = true host_visible_mib=4096`. Grep for that line,
  not for the variable, when checking that a run attached it.
- `entangled doctor` runs the same probe `run` refuses with (`doctor::venus_host`).
  Its `3D` section names the device and says whether the host has
  `VK_EXT_external_memory_host` (required: a device without it is hidden) and,
  on Windows, `VK_KHR_external_memory_win32` (without it GNOME's scanout stays
  in software).
- A guest needs its GL sent to Zink and its idle blank off for the desktop.
  `install ubuntu --venus` writes both through the autoinstall
  (`seed::venus_late_commands`). An existing guest is configured by hand, as
  the user guide's "A GPU-accelerated desktop" says. Zink needs **two**
  switches: `/etc/drirc` for the system's Mesa, and
  `MESA_LOADER_DRIVER_OVERRIDE=zink` in the session's `environment.d` for a
  snap's own Mesa, which a `DRIRC_CONFIGDIR` in its wrapper keeps from ever
  reading `/etc/drirc` (ADR-0004, the 2026-09-26 amendment on Firefox).
- **The guest always has `DEVICE_LOCAL | HOST_VISIBLE` memory.** Hiding the
  BAR type (WHP cannot back a guest mapping with the driver's BAR) left the
  RTX 2070 with none, and Zink then maps buffers it put in plain device-local
  memory (a persistent or write-only `zink_buffer_map` of a `DEFAULT` or
  `DYNAMIC` buffer; Mesa 25.2.8, 26.0.8 and main alike): the blob is refused,
  `vkMapMemory` fails and the application writes through NULL.
  `policy::visible_vram` appends one `DEVICE_LOCAL | HOST_VISIBLE |
  HOST_COHERENT` type in a device-local heap of its own (512 MiB, capped at
  the host-visible share), backed by our pages imported as the first
  coherent host-visible type. It is the **only guest type index that is not
  the host's**: allocate and import through `GuestDevice::host_type`, and
  turn every host `memoryTypeBits` into guest terms through
  `GuestDevice::guest_bits` (which `memory::guest_type_bits` does). A new
  path that hands the host a guest type index, or the guest a host bit mask,
  without them is a bug that the fake tests in `executor::vram_tests` catch
  only if you add a case.
- **Clients present through dma-buf** (ADR-0004, S5) on a host that exports
  device-local memory: `policy::GuestWsi` shows an NVIDIA driver at venus's
  590.48.1 gate, and swapchains are S1's canonical LINEAR images. Two traps
  that are easy to reintroduce: Mesa's WSI puts `ALIAS` on every swapchain
  image and `EXTENDED_USAGE` on mutable ones (`modifier::IGNORED_FLAGS` must
  keep accepting them), and a swapchain format *without* LINEAR sends the
  WSI down its prime path, whose blit buffer finds no device-local type in
  our pages and spins forever in Mesa's `UNREACHABLE`. Every format the WSI
  can pick belongs in `modifier::SCANOUT_FORMATS`. Check a client with
  `WAYLAND_DEBUG=client`: `zwp_linux_buffer_params_v1#N.add(...)`, not
  `wl_shm#N.create_pool`.
- **What the format queries advertise, `vkCreateImage` must accept**
  (ADR-0004, "X11 applications"). The two are judged by different code
  (`modifier::image_format_properties` against `check_image_create_info` +
  `modifier_create_info`), and the one time they disagreed it killed
  Xwayland: glamor on Zink asked about LINEAR alone, was told yes, then
  created with Mutter's whole list `[LINEAR, DRM_FORMAT_MOD_INVALID]` (Zink
  chains its frontend's list unfiltered) and the context was made fatal. A
  modifier list is the implementation's to pick from: LINEAR is chosen from
  any list that names it; only a list without LINEAR, or an explicit
  non-LINEAR modifier, is refused. `x11_tests::everything_the_format_queries_advertise_a_create_accepts`
  holds query and create together over formats × usage × flags × the four
  ways of naming LINEAR — extend its matrix when you add a scanout format, an
  ignored flag or a new shape. Each harness call is a real ring round trip
  (~2 ms), so sample monotone dimensions instead of enumerating them.
- **X11 clients reach the GPU through Xwayland**, which Mutter starts on the
  first X connection. In a serial-console probe take `XAUTHORITY` from
  Xwayland's own `-auth` argument (`pgrep -a Xwayland`), not from
  gnome-shell's environment, which predates it: without it `glxinfo` says
  `unable to open display :0`, which is exactly what a crashed Xwayland says
  too. The journal line that tells them apart is gnome-shell's
  `X Wayland crashed; attempting to recover`.
- **The desktop's flips are presented by the display's own GPU on Windows**
  (ADR-0004, zero-copy presentation). A renderer-blob flush first asks
  `Renderer3d::begin_shared_scanout` for a **lease**: the image, its
  duplicated NT handle and create info, the guest's release, and a claim on
  the payload (`writes::Owner::Presenter`). It then hands the lease to
  `ScanoutSink::present_shared`, which keeps it until its GPU copy has run.
  The copy path (`read_rect_bgra`) serves anything the sink declines. After a
  shared present, the first copy-path flush reads the whole visible region,
  because the mirror is stale. Both the renderer hook and the sink methods are
  additive defaults, so a new renderer or sink owes nothing. A new *owner* of
  shared payloads needs its own serials and `Progress`: a watermark shared
  with a synchronous owner completes the other's touches early (why
  `Presenter` is not `Scanout`). `ENTANGLED_SCANOUT_PATH=copy` is the A/B
  switch. Grep the log for `presents the renderer's scanout through the
  display's GPU` (once per boot) and `shared=` in the pacing lines.

## Per-device references

- **blk** (EPIC 4): request = header (type/reserved/sector) + data + status
  byte. Types in `virtio_block::RequestType`; status `S_OK/S_IOERR/S_UNSUPP`.
  Unknown type → `S_UNSUPP`, out-of-range → `S_IOERR` (test exists).
  **Thin-provisioning reclaim** (`VIRTIO_BLK_F_DISCARD` bit 13,
  `VIRTIO_BLK_F_WRITE_ZEROES` bit 14) is on by default for every writable disk
  and withheld for read-only ones, so a guest `fstrim` gives host disk space
  back. Three things to know before touching it:
  - The config space is now the spec's **fixed 60-byte layout**, not just
    `capacity`: the discard fields sit at offsets 36–56, so everything in front
    of them is published as zero. Build it in `BlockDevice::config_space`, never
    by special-casing an offset, and keep the published limits equal to the
    constants `DiscardSegment::validate` enforces — a number we advertise and
    then refuse makes a well-behaved driver look malicious.
  - The **segment array** is a second untrusted surface on top of the header:
    `segment_count` checks the array's shape (whole 16-byte segments, at most
    `MAX_DISCARD_SEG`) and `DiscardSegment::validate` checks each range
    (reserved flag bits refused, `unmap` refused on a discard, per-command
    maximum, overflow-safe capacity check, representable byte offset *and* end).
    Protocol misuse answers `S_UNSUPP`, a limit or geometry violation `S_IOERR`.
    Validation runs over the **whole array before anything is punched**, so one
    poisoned segment costs the guest the request and nothing else — the property
    `blk_queue::one_bad_segment_makes_the_whole_request_a_no_op` exists to keep.
  - The host half is `disk_image::{punch_hole, write_zeroes}` (Linux
    `fallocate(PUNCH_HOLE|KEEP_SIZE)`, Windows `FSCTL_SET_ZERO_DATA` on a sparse
    file). `punch_hole` may report `Unsupported` and change nothing — legal for a
    discard hint; `write_zeroes` never can, because zeros are a promise, so it
    falls back to writing them. `RawDisk` logs which mechanism it got **once per
    disk**, not per request. `ENTANGLED_BLK_DISCARD=off` withholds both features,
    which is how the before/after measurement is taken.
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

  **Five things about `usernet::tcp` that are load-bearing and were each a bug
  or nearly one:**
  - **A flow is retired only after the stack has spoken.** `poll` runs
    `iface.poll` → `service_flows` → `iface.poll` → *then* removes the finished
    sockets. `socket.abort()` merely moves smoltcp to `Closed`; the RST it was
    aborted to send is emitted by the next dispatch, so a socket removed before
    that dispatch never sends it — a guest whose host connect was refused got
    silence and waited out its own SYN timeout. Same reordering is what lets the
    ACK for a guest's final FIN go out before its socket disappears.
    The corollary for tests: a refusal can be back **before the first poll**
    (Linux loopback is immediate; the test thread only has to be descheduled
    between SYN and poll), and then that one poll emits SYN-ACK *and* RST and
    retires the flow. A test that runs `GuestPeer::handshake()` — which discards
    what it polls — and then expects the flow to still be open is asserting a
    window the NAT never promised; it failed 8 runs in 200 under CPU load. Watch
    every segment from the first poll, and pin orderings deterministically with
    `GuestPeer::host_connect_fails` on an `Offline` NAT instead of racing a real
    socket.
  - **Every socket carries a keep-alive pair** (`FLOW_KEEPALIVE`,
    `FLOW_IDLE_TIMEOUT`). Nothing in TCP notices a peer that stops existing, and
    the guest is a peer that can: a reboot, a device reset, a paused VM. Without
    it 64 abandoned flows wedge the NAT for the life of the process, and a
    device reset does *not* rebuild the backend.
  - **The flow table is observable** — `UserNetBackend::{flow_count,
    flows_retired, flows_refused_at_limit}`. The leak above was invisible for
    exactly as long as nothing outside the crate could see the number.
  - **There is no MSS clamp and there must not be one.** The NAT *terminates*
    TCP: the guest's connection ends in smoltcp and a separate host socket
    carries the bytes on, so the guest's MSS is negotiated against this
    segment's own 1500-byte MTU and a short host uplink (WSL's 1472, a VPN's
    1400) is the host stack's problem on a connection the guest never sees.
    That is the opposite of the TAP path, where the guest's own segments are
    bridged onto that uplink and the nftables clamp in `scripts/setup-tap.sh`
    is what keeps them from being dropped.
  - **UDP is DHCP and DNS and nothing else.** There is no general UDP NAT, so
    QUIC, NTP and mDNS do not work through this backend; anything else on UDP
    is counted as `dropped_unsupported`. A test says so, and it is the test that
    has to change the day one is added.
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
    (the device is inside its transport before the worker exists). On the
    synchronous-kick buses (WHP) `machine_x86::host_wake` makes the same
    `queue_notify(0)` from a pause-gated thread of the bus's own (2026-09-24;
    before that WHP devices got no waker). **A waker is never a reason to
    complete work the host has not finished**: a device must be correct when
    its waker never fires (unit tests, `ENTANGLED_QUEUE_NOTIFY=sync`) — what
    it holds is served at the next kick. virgl's device-timeline fences may
    still fall back to phase 1 without one (one GL context, in order); a
    Venus `ring_idx` fence may not, and stays pending. Bounds that
    are load-bearing: `MAX_PENDING_FENCES` (64, checked *before* asking the
    renderer for a fence), a 2 s watchdog (`FENCE_TIMEOUT`) so a stalled host
    never becomes a stalled guest, immediate answers for failed fenced
    commands, and a reset that drops everything held. `ENTANGLED_GPU_FENCES=sync`
    forces phase 1 back for a measurement. Since EPIC 20 stage 5b.3 the table
    holds **one FIFO per timeline** (`virtio_gpu::FenceTimeline`): the device's,
    for every fence without `VIRTIO_GPU_FLAG_INFO_RING_IDX` (all of virgl's),
    and one per `(ctx_id, ring_idx)` when the header carries it (the guest
    kernel's per-ring fence contexts; Venus binds one per `VkQueue`). A
    retirement completes only its own timeline's prefix. A renderer names the
    timeline through `Renderer3d::create_fence_on` / `poll_fence_timelines`,
    whose defaults put everything on the device timeline — never change a
    renderer's fence behaviour by editing `create_fence`/`poll_fences` alone.
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
    fence statistics and mirrored to JSON by `entangled run --frame-stats`.
    Each interval is split into **quiet** (the guest asked for nothing),
    **submit** (guest → device) and **service** (the device's own cost); the
    three sum to the interval, which is what makes an attribution an argument.
    Everything that depends on the refresh follows `[display] refresh_hz`
    (`GpuDevice::set_refresh_hz` → `FramePacing::set_refresh`): the slot
    counters, `late` (1.2 periods) and the report window (two seconds of
    slots, never under 120 frames). The report carries `stddev_us` and
    `jitter_us` (mean |Δn − Δn−1|) and the log line its `refresh_hz`. With a
    client on screen `submit` covers the client's traffic too, so it does not
    isolate the compositor. Use it for any GPU before/after — and see the
    host-display skill for the things it has already caught.
  - **The scanout reply is the one message worth hand-optimising.** A present
    flushes the *whole* screen (the guest double-buffers, so DRM widens the
    damage to the plane), which is 7.9 MiB at 1080p, once per frame, across
    the isolation boundary. It therefore has a bulk path — `write_bytes_reply`
    on the helper, `read_frame_header` + `read_payload` on the client — that
    reuses buffers instead of building `Reply::Bytes` → `encode` → `frame` and
    decoding back out. Same bytes on the wire (a unit test pins that); it was
    worth 30 ms → 13 ms of service time per frame. If you add another
    megabyte-scale reply, give it the same treatment and the same parity test.

  Tests: `tests/gpu_3d.rs` (transport-level, any OS), `tests/gpu_fence.rs`
  (deferral, cap, watchdog, reset, renderer death — any OS),
  `tests/gpu_remote.rs` (a real helper process, including `SIGKILL` under
  load — unix), `tests/virgl_host.rs` + `virgl_fence_host.rs` +
  `virgl_scanout_host.rs` (real GL, one binary each because virglrenderer is a
  process singleton, all self-skipping), `boot-tests/virgl_gnome.rs` (GNOME
  live on virgl, `--ignored`), fuzz targets `gpu_3d_commands` (now including
  the fence surface) and `gpu_remote_protocol`.
- **snd** (EPIC 21, GAME-2102): wire format in `virtio_sound::protocol` (mirrors
  `linux/virtio_snd.h`, lengths asserted at compile time), bounds and the
  lifecycle state machine in `virtio_sound::stream`, the device and its two pump
  threads in `virtio_sound::device`. Four queues, as the spec mandates: control,
  event, TX, RX — and since phase 2 all four are live. Six things to know before
  touching it:
  - **Completion is the pacing, in both directions.** The driver puts one
    message per period on a queue and treats the used ring as the hardware
    pointer, so a playback message may only be retired once the host sink has
    actually *consumed* its audio, and a capture buffer only once the audio to
    fill it **exists**. Retire on copy, or answer a capture buffer when its
    request arrives, and the guest believes an hour moved in a microsecond.
    This is why **pump threads** own the TX and RX queues (`Arc<Mutex<Inner>>`,
    virtio-net's receive worker is the pattern) and take the pause gate before
    they touch guest memory. Two threads and not one: each blocks for about a
    period inside its endpoint, and sharing would make each direction wait on
    the other's hardware. No `HostWaker`: unlike a GPU fence, nothing foreign
    has to tell us the audio finished — these threads measured it.
  - **RX inverts the ownership, and two rules make that safe.** On TX the guest
    hands over bytes; on RX it hands over *room* and the device writes into
    guest memory. (1) The room is measured **once**, from the guest's own
    device-writable descriptors (`CaptureBuffer::room` = the writable total
    minus the status word), validated as whole frames within one period, and
    recorded — every later write is bounded by that number *and*, segment by
    segment, by the length the chain walk reported. (2) A buffer is filled and
    retired only when `consumed` has passed its `consume_at`, so it never
    carries bytes that do not exist; a STOP or RELEASE hands its buffers back
    with a **zero-length payload** rather than with whatever the ring holds.
    A capture buffer also reserves its room (`Stream::promised`) the moment it
    is posted, which is what admission control bounds — `ring.len()` would be
    the wrong number, because it is empty when the reservation is made.
  - **A software endpoint must keep time.** `NullSink`/`RecordingSink` and
    `SilentSource`/`ToneSource` sleep through `backend::Pacer` rather than
    moving audio instantly; an endpoint with zero latency is not a sound card.
    `unpaced()` exists for tests and fuzzing only.
  - **The advertised set is short on purpose, and identical either way**: one
    output stream and one input stream, `S16`, 44100/48000 Hz, 1–2 channels, no
    PCM features. A guest's own ALSA converts anything else, and every extra
    format is more host code on an untrusted path — widening only the capture
    side would be worse, not better. Requests are checked against
    `SUPPORTED_FORMATS`/`SUPPORTED_RATES`, never against what the guest claims
    we said, **and** against `stream::direction_of(id)` matching the queue the
    message arrived on.
  - **Status mapping is load-bearing**: `BAD_MSG` for protocol misuse (an id
    outside the config space, an id named on the wrong queue, a command in the
    wrong state, a payload or a grant of room that contradicts the negotiated
    geometry), `NOT_SUPP` for a well-formed ask we never offered (format, rate,
    channel count, buffer size, `JACK_REMAP`), `IO_ERR` only for a genuine
    host-side or ring-full condition. A driver that gets the two confused debugs
    the wrong half of its stack.
  - **The host endpoints are behind `AudioSink`/`AudioSource`, built by a
    `SinkFactory`/`SourceFactory` *on their own pump thread*** — WASAPI's COM
    objects and ALSA's handle both belong to one thread, and a device reset has
    to be able to get its audio back. Linux is `libasound` **`dlopen`ed, never
    linked** (LGPL; `cargo deny` gates the graph — ADR-0004's virglrenderer
    arrangement, reasoning in `virtio_sound::alsa`'s module docs); capture added
    exactly one symbol, `snd_pcm_readi`, and changed nothing else about that.
    Windows is WASAPI shared mode with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
    SRC_DEFAULT_QUALITY` for render *and* capture, so the audio engine
    resamples and we ship no converter; the capture client is packet-oriented,
    hence `WasapiSource`'s leftover buffer (`ReleaseBuffer` must consume a whole
    packet or none).
  Underruns and overruns are counted and logged at most once a second, never
  allowed to wedge the device — the playback pump writes silence and carries
  on, and the capture pump only reads what the guest has made room for, so a
  guest that stops posting starves the microphone (counted as an overrun after
  `CAPTURE_STARVE_GRACE`) rather than making the host buffer without bound. A
  missing or failing source degrades to `SilentSource`; `open_source` never
  fails a VM start, because a machine with speakers and no microphone is
  completely ordinary.
  The three obligations (CLAUDE.md, ADR-0005/0006) are all present: `reset()`
  clears both streams and joins both pumps, both pumps take `Quiesce`, and
  `save_device`/`load_device` carry the two streams' state and params through
  the same `validate_params` a guest's own `SET_PARAMS` goes through. Note the
  deliberate quirk in `queue_positions`: TX and RX report `next_avail` **rewound
  by the un-retired pending count**, so messages the device is holding are
  re-delivered after a restore rather than lost. That is the opposite of what
  virtio-gpu wants from the same field and for the opposite reason — a GPU
  command in flight has already had its side effect, an unconsumed period has
  had none.
  Tests: `tests/snd_queue.rs` (mmio — a 440 Hz tone played and asserted
  byte-for-byte with no underruns and provably paced completion, a 1 kHz tone
  *recorded* and asserted the same way with zero overruns, both directions at
  once, and the malicious-guest suite for each queue), `tests/snd_pci.rs` (the
  same untouched device over pci), fuzz targets `snd_control` (parsers, bounds,
  lifecycle, both queue directions) and `snd_device` (arbitrary chains through a
  live transport whose streams the harness first drives to Running, so RX
  reaches the fill path rather than only the refusal path).
- **input** (EPIC 9): event model in `virtio_input` (`ev`, `abs`, `btn`,
  `InputEvent`). Absolute pointer: window coords →
  `InputEvent::abs_from_window` (0..=32767). Every batch ends with
  `InputEvent::SYN_REPORT`. On focus loss release all pressed keys (MVP-906).

  **gamepad** (GAME-2104, `[gamepad] enabled`): a third `Profile`, and the
  whole deliverable is its config space. Three consumers have to accept it and
  none of them reads its *name* — `joydev` (so `/dev/input/js*` exists), udev's
  `input_id` (so a desktop user gets an ACL on the node) and SDL's capability
  fallback (so Steam maps it with no controller-database entry) — and the one
  layout all three agree on is the kernel's own `xpad`. So the eleven
  `BTN_GAMEPAD` codes, `ABS_X/Y/RX/RY` at `-32768..32767` fuzz 16 flat 128,
  `ABS_Z/RZ` at `0..255`, `ABS_HAT0X/Y` at `-1..1`, copied code-for-code. Four
  things that look like details and are not: `BTN_C`/`BTN_Z` are *omitted*
  (advertising them makes it a six-face-button pad to SDL), `BTN_NORTH` is Y
  and `BTN_WEST` is X (not clockwise), the ids stay `BUS_VIRTUAL` + our own
  vendor rather than Microsoft's `045e`, and `flat` is the only deadzone in the
  whole path — the host rescales and never filters.
  Host capture is `virtio_input::gamepad`: sources report a pad's **complete
  state** and the pump diffs it, which is why hotplug needs no code — an unplug
  is `PadState::NEUTRAL`, so held buttons release and sticks re-centre, and
  every later tick then produces exactly zero events. Linux is `/dev/input/
  event*` by hand (four ioctls and a `poll`, in `libc` — no `gilrs`), Windows
  is XInput. The pump is a host thread that writes guest memory, so it belongs
  to one activation and takes the pause gate (ADR-0005), like virtio-net's
  receive worker.
  **`CONFIG_INPUT_JOYDEV` is a separate kernel symbol from
  `CONFIG_INPUT_EVDEV`** and is a *module* in a stock Debian kernel. Without it
  the pad is a perfectly good `event*` device with no `js*` at all — which is
  what the boot acceptance failed on first. It is in
  `guest/bootstrap-kernel/entangled.config` now; `tests/boot/tests/gamepad.rs`
  self-skips rather than run on the Debian-installer fallback, because a
  failure there says nothing about the descriptor.
  **Who gets `js0`, and the rule that decides it.** `joydev`'s id table claims
  *any* `EV_ABS` device with `ABS_X`, so it claimed the tablet too and the pad
  came second. The only escape is `joydev_dev_is_absolute_mouse()`, which is
  three *exact* bitmap comparisons (drivers/input/joydev.c, unchanged v6.12 to
  master): event types exactly `{SYN,KEY,ABS}` **or** `{SYN,KEY,ABS,MSC}`
  **or** `{SYN,KEY,ABS,MSC,REL}`; absolute axes exactly `{ABS_X,ABS_Y}`; keys
  exactly `{BTN_LEFT,BTN_RIGHT,BTN_MIDDLE}`. Note the shape of the first one:
  **`EV_REL` is admissible only in company with `EV_MSC`**, so a pointer with a
  scroll wheel needs both or neither. The tablet therefore advertises three
  buttons and one `MSC_SCAN` bit it never sends, and the host's Back/Forward
  mouse buttons went to the *keyboard* as `KEY_BACK`/`KEY_FORWARD` — codes it
  already advertised, and the ones browsers already bind. Measured in the
  guest, all three states: with the old capabilities the tablet took `js0`;
  with three buttons but no `EV_MSC` it *still* took `js0` (so the obvious
  half-fix is a dead end); with both changes it has no `js*` at all.
  `config::joydev_would_bind` models the rule against our own bitmaps and is
  unit-tested per profile — use it rather than rediscovering this from a boot.
  **Two players** (`[gamepad] players = 1..=4`): one virtio-input device is one
  evdev device, so there is no way to put two pads on one device — `players =
  N` costs N of the eight slots on either bus, and the arithmetic is written
  out on `control_api::GamepadSection`. The two devices are the same name and
  the same `input_id` (as two identical controllers are) and differ only in
  `VIRTIO_INPUT_CFG_ID_SERIAL`, which `virtio_input.c` puts in `idev->uniq`.
  Which *host* controller is which player is decided in one portable place,
  `gamepad::PadRoster`, shared by both backends: fill in order, never steal,
  never promote — unplugging player 1 leaves player 2's pad where it is, and
  the freed slot is refilled first.
  **Rumble does not exist and cannot, yet.** Linux's `virtio_input.c` never
  queries `EV_BITS` for `EV_FF` and never calls `input_ff_create()`, so `EV_FF`
  is never in the guest device's `evbit`: `EVIOCSFF` fails before a game gets
  an effect id, and an `EV_FF` write is dropped by the input core before it
  could reach the status queue. And the spec has nowhere to upload a
  `struct ff_effect` — the status queue carries an 8-byte type/code/value
  triple. Both halves are asserted against a real kernel in
  `tests/boot/tests/gamepad.rs`. Before writing a host `RumbleSink`, check
  those two facts still hold; until they change it would be unreachable code.
  **The status queue is the device's only inbound path** and it is entirely
  guest-shaped: `StatusEvent::classify` is total, every kind is counted
  (`status_events`, `status_ff`, `status_rejected`), an unreadable or
  interleaved chain is refused *and still acked*, and nothing that arrives
  indexes anything host-side. Fuzzed by `fuzz/fuzz_targets/input_device.rs`.

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

## Pause, reset and suspend: what every device owes the machine (ADR-0005, ADR-0006)

A VM can be **frozen**, **rebooted in place** and **written to a file**, so a
device is not finished when it works — it has to survive all three. Three
obligations:

1. **`reset()` returns the device to power-on**, infallibly. This already existed
   as the driver-facing device reset (a write of 0 to `device_status`); a machine
   reset calls the same path through `TransportState::power_on_reset`, which adds
   what a device reset deliberately *keeps*: `config_generation`, and on
   virtio-pci the MSI-X table and message-control register. A virtio device reset
   is not a PCI function reset; a reboot is both.
2. **A host thread of your own that touches guest memory must take the pause
   gate first.** `DeviceResources::quiesce` is an `Arc<Quiesce>`;
   `let Some(_pass) = quiesce.wait_while_paused(|| !stop.load(..)) else { return }`
   at the top of the loop, *before* any device lock — and **hold the pass for as
   long as the work lasts**. Closing the gate only stops work that has not
   started; a pause is not acknowledged until every pass is dropped, which is
   what makes "paused" true of guest memory and not only of the guest. Two
   devices need it today: virtio-net (its receive worker writes arriving frames
   straight into the RX ring on its own schedule) and virtio-snd (its two pump
   threads retire playback messages on the host's audio clock and *write*
   captured audio into the guest's own buffers on the same one). A device whose
   work all happens inside `notify()` needs nothing: it is already on a parked
   vCPU thread, or behind a queue worker that took the gate for it.

Three rules that are easy to get wrong:

- **Take the gate outside the lock.** A machine reset runs *while the VM is
  quiesced* and needs the device locks. Parking inside one deadlocks the reset
  that is trying to shut you down.
- **Bring your own liveness predicate, and `wake()` when you stop.** virtio-net's
  `reset()` joins its receive worker; a gate that only opened on resume would
  deadlock exactly that. `stop.store(true); quiesce.wake(); thread.join()`.
- **Host wiring survives a reset.** `notify_offloaded`, the ioeventfd
  registrations, the pause gate itself: all of it describes the *host*, not the
  guest, and the same worker keeps serving the device across a reboot. What does
  not survive is anything the guest programmed.

On virtio-pci the bus additionally restores each function's configuration space
from a power-on snapshot taken at attach, then **re-bases the notify
ioeventfds** around the restored BARs (`reconcile_notify`). Miss that and the
next boot's kicks land at an address nothing is listening to — a device that
enumerates, negotiates and then never completes a request. The *restore* path
goes through the same `reconcile_notify` for the same reason.

### 3. `queue_positions`, and a `save_device`/`load_device` pair if you hold
anything else (ADR-0006)

`TransportState::save`/`load` carry everything the transport owns — features,
status, queue geometry, the ISR, `config_generation`, the MSI-X table. What it
cannot see is inside your device:

```rust
fn queue_positions(&self) -> Vec<virtio_core::QueuePosition>   // next_avail / next_used, in queue order
fn save_device(&self) -> Vec<u8>                                // anything else, your encoding
fn load_device(&mut self, bytes: &[u8]) -> Result<(), DeviceError>
```

- **A device that holds queues must implement `queue_positions`.** The positions
  cannot be recomputed from guest memory for a device that holds a descriptor
  chain across a host fence (virtio-gpu does), and a restored device that forgot
  where it was hands the same buffers out twice. Empty when not activated.
- On restore the transport applies the positions to the rebuilt queues **before**
  `activate()`, so a device that starts serving on activation does not first
  re-serve what it already completed.
- `load_device`'s bytes come out of a **file**: treat them exactly like a guest
  command. Bounds-check every length before allocating, and return an error
  rather than half-applying. `virtio_gpu::save` is the worked example, in the
  same explicit-`from_le_bytes` style as `virtio_gpu::protocol`.
- **Save what the guest cannot rebuild, not what it can.** virtio-gpu records its
  2D resources' identity, geometry and *guest backing list* — not their pixels,
  which are a copy of guest pages the snapshot already carries, and which the
  restore re-derives by re-running the transfer. Eight megabytes per resource
  saved, several times over.
- **Say so when something cannot come back.** virtio-net's NAT flows,
  virtio-snd's host sink and capture source, and virtio-gpu's 3D contexts and
  blob resources are gone by construction. The network and the audio are left to the guest to
  notice (what a laptop suspend does to them); the GPU raises
  `DEVICE_NEEDS_RESET` through the same path GPU-012 uses for a crashed
  renderer. None of them silently pretends.
- **A device's own state has its own version, and so does the section around
  it.** Adding a field is a bump: `virtio` went to 2 when `SHM_SEL` and the
  host's shared-memory placement joined it, and virtio-gpu's blob went to 2 when
  the scanout source became three-way. Defaulting a missing field would restore
  a guest whose driver had selected a region into one that had not, so the old
  snapshot is refused by number instead — `SectionVersion` names the section and
  both versions.
- **Guest-visible state the *host* chose still needs recording.** A
  shared-memory region's base is placed by the machine, not the guest — but the
  guest read it out of the registers and its mappings point at it. It is saved
  not to be restored but so a machine that placed the window elsewhere is a
  refusal. The same reasoning applies to anything else the host hands the guest
  as an address.

There is a fourth obligation that is not the device's but is worth knowing about
when a resumed guest misbehaves: on **KVM** the 8259s, IOAPIC and 8254 are in
the kernel, and the snapshot carries them through `vmm_core::hv::HostIrqChip`.
A restore that skipped them comes back with every IOAPIC pin masked — and MSI-X
devices keep working, so the VM looks half-alive.

## Publishing the renderer (ADR-0004, 2026-09-16)

A Venus-capable virglrenderer is now a **published, pinned artifact**, the third
after the UEFI firmware and the bootstrap kernel:

* `entangled fetch virglrenderer` downloads `libvirglrenderer.so.1` and
  `virgl_render_server` into `<cache>/virglrenderer/<tag>/`, digest-checked
  against `guest/virglrenderer/pinned.toml`.
* `.github/workflows/virglrenderer.yml` builds and publishes them, on
  **ubuntu-22.04** — the same runner as the Linux engine, so both binaries this
  project ships for Linux carry the same floor, GLIBC_2.34. Do not "modernise"
  that runner: a newer one silently narrows the hosts that can load the renderer
  to fewer than the hosts that can run the engine.
* `apps/entangled/src/virgl_lib.rs` finds them (`ENTANGLED_VIRGL_DIR`, then the
  verified cache) and `doctor` reports the result as a `3D:` line.

Two rules that are easy to break and expensive to debug:

**Both files or nothing.** Venus exists only behind the render server, so a
directory holding just the library is not a renderer and `pair_in` says so. A
library published alone would serve classic virgl and refuse every Venus command
in band — green logs, no Venus, no explanation.

**Never point `ENTANGLED_VIRGL_LIB` at a fetched library from code.** That
variable is a *person's* instruction and a path that will not open is a hard
error. The one `entangled run` sets is `ENTANGLED_VIRGL_LIB_DEFAULT`, which
falls through to the system library when it cannot be loaded — the case being a
host with no `libvulkan.so.1`, which a Venus build hard-requires and the
distribution's 0.9.x does not.

The loader also exports `RENDER_SERVER_EXEC_PATH` itself, derived from the
library it opened, because virglrenderer compiles that path in as an absolute
one under its build prefix. Without it a downloaded artifact loses Venus quietly.
