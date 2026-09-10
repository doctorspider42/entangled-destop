//! The shared-memory BAR as a *driver* sees it, and as a *firmware* moves it
//! (EPIC 20, VEN-2001 phase 2).
//!
//! `tests/boot/tests/pci_shm.rs` proves the whole chain with a real Linux
//! kernel on KVM. This proves the parts of it that a boot test cannot reach and
//! that no hypervisor is needed for — which is the point, because it therefore
//! runs on **both** hosts, in CI, in a second:
//!
//! * the BAR-sizing protocol on a 64-bit pair, which is how both EDK2 and Linux
//!   learn the window's size before they place it;
//! * the `virtio_pci_cap64` shared-memory capability, walked out of the
//!   capability list the way a driver walks it;
//! * the mapping following a BAR the guest moves — the thing EDK2's
//!   `PciBusDxe` does to every BAR on the bus during enumeration;
//! * the mapping going away when the guest turns memory decoding off, resets
//!   the machine, or parks the BAR somewhere the machine refuses to follow.
//!
//! The hypervisor half is `vmm_core::shm::UnmappedGpaMapper`: real host pages,
//! no guest behind them. Everything asserted here is bookkeeping that must be
//! right *before* a hypervisor call is made, so testing it without one is not a
//! compromise — it is the layer being tested.

use std::sync::Arc;

use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::pci;
use machine_x86::shm::ShmSupport;
use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
use virtio_core::pci as vpci;
use vmm_core::shm::{SharedWindow, UnmappedGpaMapper};

const MIB: u64 = 1 << 20;
/// The guest this bus belongs to: below the 32-bit hole, so its 64-bit
/// aperture starts at exactly 4 GiB.
const GUEST_BYTES: u64 = 2048 * MIB;
/// What the fake device declares — the same size virtio-gpu's loopback Venus
/// renderer declares, so the BAR is the one a real VM gets.
const WINDOW: u64 = 256 * MIB;

/// A device whose only interesting property is that it has a shared-memory
/// region. Deliberately not virtio-gpu: this test is about the bus.
struct WindowedDevice {
    regions: Vec<virtio_core::ShmRegion>,
    backing: Option<Arc<dyn virtio_core::ShmBacking>>,
}

impl WindowedDevice {
    fn new(regions: Vec<virtio_core::ShmRegion>) -> Self {
        Self {
            regions,
            backing: None,
        }
    }
}

impl virtio_core::VirtioDevice for WindowedDevice {
    fn device_type(&self) -> virtio_core::DeviceType {
        virtio_core::DeviceType::Gpu
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[256]
    }
    fn device_features(&self) -> u64 {
        virtio_core::VIRTIO_F_VERSION_1
    }
    fn ack_features(&mut self, _negotiated: u64) -> bool {
        true
    }
    fn read_config(&self, _offset: u64, _data: &mut [u8]) {}
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(
        &mut self,
        _resources: virtio_core::DeviceResources,
    ) -> Result<(), virtio_core::DeviceError> {
        Ok(())
    }
    fn notify(&mut self, _queue_index: u16) -> Result<(), virtio_core::DeviceError> {
        Ok(())
    }
    fn reset(&mut self) {}
    fn shm_regions(&self) -> Vec<virtio_core::ShmRegion> {
        self.regions.clone()
    }
    fn set_shm_backing(&mut self, _id: u8, backing: Arc<dyn virtio_core::ShmBacking>) {
        self.backing = Some(backing);
    }
}

fn memory() -> Arc<virtio_core::GuestMem> {
    Arc::new(virtio_core::testing::guest_memory(4 * MIB))
}

/// An interrupt sink that counts and does nothing else — the userspace irqchip
/// needs one, and nothing here looks at an interrupt.
#[derive(Default)]
struct Silent;

impl vmm_core::InterruptDelivery for Silent {
    fn request(&self, _r: &vmm_core::InterruptRequest) -> Result<(), vmm_core::hv::HvError> {
        Ok(())
    }
}

fn chip() -> Arc<UserspaceIrqChip> {
    UserspaceIrqChip::new(Arc::new(Silent), 1).expect("userspace irqchip")
}

/// A bus with one windowed device, on the host-neutral (userspace-irqchip)
/// path so this file compiles and runs on Windows too.
fn bus_with_window(regions: Vec<virtio_core::ShmRegion>) -> VirtioPciBus {
    let chip = chip();
    let allocate = |len: u64, host_mapped: bool| {
        if host_mapped {
            SharedWindow::new_host_mapped(len, Arc::new(UnmappedGpaMapper)).map(Arc::new)
        } else {
            SharedWindow::new(len, Arc::new(UnmappedGpaMapper)).map(Arc::new)
        }
    };
    let devices: Vec<Box<dyn virtio_core::VirtioDevice>> =
        vec![Box::new(WindowedDevice::new(regions))];
    VirtioPciBus::attach_userspace_with_shm(
        memory(),
        devices,
        chip.as_ref(),
        PciInterruptMode::IntxOnly,
        Some(ShmSupport {
            mem_bytes: GUEST_BYTES,
            allocate: &allocate,
        }),
    )
    .expect("bus")
}

/// A bus with the same device but no way to back its window — the machine that
/// every caller had before this epic.
fn bus_without_support() -> VirtioPciBus {
    let chip = chip();
    let devices: Vec<Box<dyn virtio_core::VirtioDevice>> =
        vec![Box::new(WindowedDevice::new(vec![
            virtio_core::ShmRegion {
                id: 1,
                len: WINDOW,
                host_mapped: false,
            },
        ]))];
    VirtioPciBus::attach_userspace(memory(), devices, chip.as_ref(), PciInterruptMode::IntxOnly)
        .expect("bus")
}

// ---- configuration-space access, the way a guest makes one ----------------

fn address(device: u8, register: u8) -> u32 {
    0x8000_0000 | (u32::from(device) << 11) | u32::from(register & 0xfc)
}

fn read_dword(bus: &VirtioPciBus, device: u8, register: u8) -> u32 {
    bus.io_write(
        pci::CONFIG_ADDRESS_PORT,
        &address(device, register).to_le_bytes(),
    );
    let mut data = [0u8; 4];
    bus.io_read(pci::CONFIG_DATA_PORT, &mut data);
    u32::from_le_bytes(data)
}

fn write_dword(bus: &VirtioPciBus, device: u8, register: u8, value: u32) {
    bus.io_write(
        pci::CONFIG_ADDRESS_PORT,
        &address(device, register).to_le_bytes(),
    );
    bus.io_write(pci::CONFIG_DATA_PORT, &value.to_le_bytes());
}

/// Programs the 64-bit BAR pair at index 2 and turns memory decoding on, in the
/// order a firmware does it: address first, enable second.
fn place_bar(bus: &VirtioPciBus, device: u8, base: u64) {
    let low = pci::reg::BAR0 + 2 * 4;
    write_dword(bus, device, low, base as u32);
    write_dword(bus, device, low + 4, (base >> 32) as u32);
    let command = read_dword(bus, device, pci::reg::COMMAND);
    write_dword(bus, device, pci::reg::COMMAND, command | 0x2);
}

fn set_memory_enable(bus: &VirtioPciBus, device: u8, on: bool) {
    let command = read_dword(bus, device, pci::reg::COMMAND);
    let value = if on { command | 0x2 } else { command & !0x2 };
    write_dword(bus, device, pci::reg::COMMAND, value);
}

/// Walks the capability list the way a driver does and returns every
/// `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` record as raw bytes.
fn shm_capabilities(bus: &VirtioPciBus, device: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = (read_dword(bus, device, pci::reg::CAP_POINTER) & 0xff) as u8;
    let mut guard = 0;
    while at >= pci::reg::FIRST_CAPABILITY && guard < 64 {
        guard += 1;
        let head = read_dword(bus, device, at);
        let id = (head & 0xff) as u8;
        let next = ((head >> 8) & 0xff) as u8;
        let len = ((head >> 16) & 0xff) as usize;
        let cfg_type = ((head >> 24) & 0xff) as u8;
        if id == vpci::PCI_CAP_ID_VNDR && cfg_type == vpci::VIRTIO_PCI_CAP_SHARED_MEMORY_CFG {
            let mut record = Vec::new();
            for offset in (0..len).step_by(4) {
                record.extend_from_slice(&read_dword(bus, device, at + offset as u8).to_le_bytes());
            }
            record.truncate(len);
            out.push(record);
        }
        at = next;
    }
    out
}

// ---- the tests -------------------------------------------------------------

/// The size a firmware and a driver read out of the pair before they place it:
/// write all-ones to both halves, read back `!(size - 1)` with the flag bits
/// restored in the low register.
#[test]
fn the_sizing_protocol_reports_the_window_on_a_64_bit_pair() {
    let bus = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let low = pci::reg::BAR0 + 2 * 4;
    let base = read_dword(&bus, 1, low);
    let base_high = read_dword(&bus, 1, low + 4);

    // Bits 2:1 = 10 (64-bit), bit 3 = 1 (prefetchable), bit 0 = 0 (memory).
    assert_eq!(
        base & 0xf,
        0b1100,
        "BAR 2 must be 64-bit prefetchable memory"
    );
    let placed = (u64::from(base & !0xf)) | (u64::from(base_high) << 32);
    assert_eq!(
        placed,
        machine_x86::layout::pci_mmio64_base(GUEST_BYTES),
        "the host's initial assignment must be the aperture base"
    );

    write_dword(&bus, 1, low, u32::MAX);
    write_dword(&bus, 1, low + 4, u32::MAX);
    let size_low = read_dword(&bus, 1, low);
    let size_high = read_dword(&bus, 1, low + 4);
    let mask = (u64::from(size_low & !0xf)) | (u64::from(size_high) << 32);
    assert_eq!(
        (!mask).wrapping_add(1),
        WINDOW,
        "the sizing protocol reported {mask:#x}, which is not a {WINDOW}-byte window"
    );
    assert_eq!(
        size_low & 0xf,
        0b1100,
        "sizing must not eat the flag bits, or the guest forgets it is 64-bit"
    );
}

/// The capability a driver finds the region through: `cfg_type` 8, BAR 2, the
/// shmid it asked for, and offset/length split across two field pairs.
#[test]
fn the_shared_memory_capability_names_bar_2_and_the_region() {
    let bus = bus_with_window(vec![
        virtio_core::ShmRegion {
            id: 1,
            len: WINDOW,
            host_mapped: false,
        },
        virtio_core::ShmRegion {
            id: 7,
            len: 8192,
            host_mapped: false,
        },
    ]);
    let records = shm_capabilities(&bus, 1);
    assert_eq!(records.len(), 2, "one capability per declared region");

    let decode = |record: &[u8]| {
        let offset = u64::from(u32::from_le_bytes(record[8..12].try_into().unwrap()))
            | (u64::from(u32::from_le_bytes(record[16..20].try_into().unwrap())) << 32);
        let length = u64::from(u32::from_le_bytes(record[12..16].try_into().unwrap()))
            | (u64::from(u32::from_le_bytes(record[20..24].try_into().unwrap())) << 32);
        (record[4], record[5], offset, length)
    };

    let (bar, id, offset, length) = decode(&records[0]);
    assert_eq!(bar, vpci::VIRTIO_PCI_SHM_BAR_INDEX);
    assert_eq!(id, 1);
    assert_eq!(offset, 0);
    assert_eq!(length, WINDOW);

    let (bar, id, offset, length) = decode(&records[1]);
    assert_eq!(bar, vpci::VIRTIO_PCI_SHM_BAR_INDEX);
    assert_eq!(id, 7);
    assert_eq!(offset, WINDOW, "the second region follows the first");
    assert_eq!(length, 8192);
    assert_eq!(records[1].len(), usize::from(vpci::VIRTIO_PCI_CAP64_LEN));
}

/// A device with no way to be backed must produce the bus it always produced:
/// no BAR 2, no shared-memory capability, nothing for a driver to find.
#[test]
fn a_machine_that_cannot_back_a_window_publishes_nothing() {
    let bus = bus_without_support();
    assert!(bus.shm_window(0).is_none());
    assert!(shm_capabilities(&bus, 1).is_empty());
    let low = pci::reg::BAR0 + 2 * 4;
    assert_eq!(read_dword(&bus, 1, low), 0, "BAR 2 must not exist");
    write_dword(&bus, 1, low, u32::MAX);
    assert_eq!(
        read_dword(&bus, 1, low),
        0,
        "a BAR that does not exist reads back zero from the sizing protocol"
    );
}

/// The BAR-rebase path, which is the reason this window needed machinery at
/// all: EDK2's `PciBusDxe` reassigns every BAR during enumeration, and the
/// mapping has to be where the guest has just put it.
#[test]
fn the_window_follows_a_firmware_bar_reassignment() {
    let bus = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let window = bus.shm_window(0).expect("backed").clone();
    let aperture = machine_x86::layout::pci_mmio64_base(GUEST_BYTES);

    // Nothing decodes until the driver says so — a fresh function's command
    // register is zero, and the window must not be mapped before that.
    assert_eq!(
        window.placed_at(),
        None,
        "a window before pci_enable_device"
    );

    place_bar(&bus, 1, aperture);
    assert_eq!(window.placed_at(), Some(aperture));

    // The firmware moves it, memory decoding still on.
    let moved = aperture + 512 * MIB;
    let low = pci::reg::BAR0 + 2 * 4;
    write_dword(&bus, 1, low, moved as u32);
    write_dword(&bus, 1, low + 4, (moved >> 32) as u32);
    assert_eq!(
        window.placed_at(),
        Some(moved),
        "the window did not follow the BAR"
    );

    // A driver unbinding turns memory decoding off; the window must go with it.
    set_memory_enable(&bus, 1, false);
    assert_eq!(window.placed_at(), None);
    set_memory_enable(&bus, 1, true);
    assert_eq!(window.placed_at(), Some(moved), "and comes back");

    // A machine reset restores the power-on registers, decoding included.
    bus.reset();
    assert_eq!(
        window.placed_at(),
        None,
        "a reboot must not leave host memory mapped in a guest that has not \
         enumerated the bus yet (ADR-0005)"
    );
}

/// The untrusted-guest case. A BAR is guest-writable, so a guest can point the
/// window at its own RAM, at the LAPIC, or at nothing in particular — and the
/// machine must refuse to map it there rather than shadow whatever is
/// underneath.
#[test]
fn a_window_the_guest_moves_out_of_the_aperture_is_unmapped() {
    let bus = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let window = bus.shm_window(0).expect("backed").clone();
    let aperture = machine_x86::layout::pci_mmio64_base(GUEST_BYTES);
    place_bar(&bus, 1, aperture);
    assert_eq!(window.placed_at(), Some(aperture));

    for evil in [
        0x0000_0000u64,                                   // guest RAM at zero
        0x0010_0000,                                      // where the kernel lives
        u64::from(machine_x86::layout::LAPIC_ADDR),       // the local APIC
        u64::from(machine_x86::layout::IOAPIC_ADDR),      // the IOAPIC
        machine_x86::layout::PFLASH_BASE,                 // the UEFI variable store
        machine_x86::layout::PCI_MMIO_BASE,               // another device's BAR
        machine_x86::layout::VIRTIO_MMIO_BASE,            // the mmio window
        machine_x86::layout::pci_mmio64_end(GUEST_BYTES), // one window past the top
        0xffff_ffff_f000_0000,                            // wraps when the size is added
    ] {
        let low = pci::reg::BAR0 + 2 * 4;
        write_dword(&bus, 1, low, evil as u32);
        write_dword(&bus, 1, low + 4, (evil >> 32) as u32);
        assert_eq!(
            window.placed_at(),
            None,
            "the machine mapped host memory at {evil:#x}"
        );
    }

    // And it recovers: a legitimate address after a refused one still works, so
    // the refusal is a decision and not a latch.
    let low = pci::reg::BAR0 + 2 * 4;
    write_dword(&bus, 1, low, aperture as u32);
    write_dword(&bus, 1, low + 4, (aperture >> 32) as u32);
    assert_eq!(window.placed_at(), Some(aperture));
}

/// The device really does get host memory it can read and write, and it is the
/// same memory the window maps.
#[test]
fn the_device_and_the_window_share_the_same_pages() {
    let bus = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let window = bus.shm_window(0).expect("backed");
    let backing = window.backing_for(1).expect("region 1");
    assert_eq!(backing.len(), WINDOW);

    backing.write(0, b"host").expect("in bounds");
    let mut buf = [0u8; 4];
    backing.read(0, &mut buf).expect("in bounds");
    assert_eq!(&buf, b"host");

    // Fresh pages are zero, which is what the guest is entitled to see.
    let mut tail = [0xffu8; 8];
    backing.read(WINDOW - 8, &mut tail).expect("in bounds");
    assert_eq!(tail, [0u8; 8]);

    assert!(backing.read(WINDOW - 4, &mut buf).is_ok());
    assert!(backing.read(WINDOW - 3, &mut buf).is_err(), "no overrun");
    assert!(backing.write(u64::MAX, b"x").is_err(), "no wrap");
}

// ---- suspend and restore (ADR-0006) ---------------------------------------

/// A snapshot records **where the host put the window**, and a restore onto a
/// machine that would put it somewhere else is refused by name.
///
/// This is the one thing a shared-memory window owes the snapshot format, and
/// the reason is not the window: it is the *guest*. A driver reads the region's
/// address once, hands it to its allocator, and every blob mapping it makes
/// afterwards is an offset from that number. Restore the machine with the
/// window a gigabyte higher and every one of those mappings points at nothing —
/// silently, in the guest, some time later. A typed refusal at restore time is
/// the only honest answer, and `virtio_core::StateError::ShmBase` is it.
#[test]
fn a_snapshot_records_the_window_and_a_moved_one_is_refused() {
    let bus = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let window = bus.shm_window(0).expect("backed").clone();
    let aperture = machine_x86::layout::pci_mmio64_base(GUEST_BYTES);
    place_bar(&bus, 1, aperture);
    assert_eq!(window.placed_at(), Some(aperture));

    let saved = bus.save_state();
    let config = bus
        .save_config()
        .expect("a pci bus has configuration space");
    assert_eq!(
        saved[0].state.shm_bases,
        vec![(1, aperture)],
        "the snapshot must say where the window was"
    );

    // Restoring onto the machine that produced it puts the window back at the
    // same address and the transport accepts it.
    let same = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    same.load_state(&config, &saved)
        .expect("same machine, same window");
    assert_eq!(
        same.shm_window(0).expect("backed").placed_at(),
        Some(aperture),
        "a restore must put the window back where the guest left it"
    );

    // A machine whose window lands elsewhere must refuse rather than restore a
    // guest whose mappings point at the old address. Forged by hand, because a
    // different guest memory size is exactly what causes it and building a
    // whole second machine here would test less, not more.
    let mut moved = saved.clone();
    moved[0].state.shm_bases = vec![(1, aperture + 0x4000_0000)];
    let elsewhere = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let error = elsewhere
        .load_state(&config, &moved)
        .expect_err("a window that moved must be refused");
    let text = error.to_string();
    assert!(
        text.contains("shared-memory") || text.contains("shm"),
        "the refusal must name the window: {text}"
    );
}

/// A window the guest had unmapped when the snapshot was taken records
/// nothing, so a restore of that VM does not insist on an address the guest
/// was not using.
#[test]
fn an_unmapped_window_records_no_placement() {
    let bus = bus_with_window(vec![virtio_core::ShmRegion {
        id: 1,
        len: WINDOW,
        host_mapped: false,
    }]);
    let aperture = machine_x86::layout::pci_mmio64_base(GUEST_BYTES);
    place_bar(&bus, 1, aperture);
    assert_eq!(bus.save_state()[0].state.shm_bases, vec![(1, aperture)]);

    set_memory_enable(&bus, 1, false);
    assert_eq!(
        bus.save_state()[0].state.shm_bases,
        Vec::new(),
        "a window that is not mapped must not be recorded as if it were"
    );
}

// ---- two windows, and a firmware that swaps them ---------------------------

/// A [`vmm_core::shm::GpaMapper`] that models the one thing
/// [`UnmappedGpaMapper`] cannot: **both hypervisors refuse an overlapping
/// range.** KVM rejects a memory slot that overlaps a live one and WHP fails
/// the `WHvMapGpaRange`, so a sweep that asks for A's new address while B is
/// still sitting on it gets an error, not a second mapping.
///
/// Shared by every window on the bus, exactly as one hypervisor is.
#[derive(Default)]
struct ExclusiveMapper {
    live: std::sync::Mutex<Vec<(u64, u64)>>,
}

impl vmm_core::shm::GpaMapper for ExclusiveMapper {
    fn map_range(
        &self,
        gpa: u64,
        range: vmm_core::shm::HostRange,
    ) -> Result<(), vmm_core::hv::HvError> {
        let len = range.len();
        let mut live = self.live.lock().expect("mapper lock");
        if live.iter().any(|&(at, l)| gpa < at + l && at < gpa + len) {
            return Err(vmm_core::hv::HvError::Registers(format!(
                "a live mapping already overlaps {gpa:#x}+{len}"
            )));
        }
        live.push((gpa, len));
        Ok(())
    }

    fn unmap_range(&self, gpa: u64, len: u64) -> Result<(), vmm_core::hv::HvError> {
        self.live
            .lock()
            .expect("mapper lock")
            .retain(|&(at, l)| (at, l) != (gpa, len));
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "exclusive"
    }
}

/// Two windowed functions on one bus, sharing one overlap-refusing mapper.
fn bus_with_two_windows() -> VirtioPciBus {
    let chip = chip();
    let mapper: Arc<dyn vmm_core::shm::GpaMapper> = Arc::new(ExclusiveMapper::default());
    let allocate =
        |len: u64, _host_mapped: bool| SharedWindow::new(len, Arc::clone(&mapper)).map(Arc::new);
    let devices: Vec<Box<dyn virtio_core::VirtioDevice>> = vec![
        Box::new(WindowedDevice::new(vec![virtio_core::ShmRegion {
            id: 1,
            len: WINDOW,
            host_mapped: false,
        }])),
        Box::new(WindowedDevice::new(vec![virtio_core::ShmRegion {
            id: 1,
            len: WINDOW,
            host_mapped: false,
        }])),
    ];
    VirtioPciBus::attach_userspace_with_shm(
        memory(),
        devices,
        chip.as_ref(),
        PciInterruptMode::IntxOnly,
        Some(ShmSupport {
            mem_bytes: GUEST_BYTES,
            allocate: &allocate,
        }),
    )
    .expect("bus")
}

/// The reason `reconcile_shm` releases every window before it claims any.
///
/// `PciBusDxe` hands out addresses in reverse device order, so a permutation —
/// here the simplest one, a swap — is not a hypothetical. Programmed one BAR
/// write at a time, the way a firmware does it, the first write asks the host
/// to map A where B still is; the hypervisor refuses, and A is left unmapped
/// with nothing but the *next* write to rescue it. That next write does rescue
/// it only if the sweep releases B before it retries A.
///
/// A one-pass sweep leaves A unmapped for good here, and the guest's `mmap` of
/// its region reads nothing — with no error anywhere, which is what makes this
/// worth a test rather than a comment.
#[test]
fn two_windows_that_swap_addresses_both_end_up_mapped() {
    let bus = bus_with_two_windows();
    let base = machine_x86::layout::pci_mmio64_base(GUEST_BYTES);
    let (x, y) = (base, base + WINDOW);

    place_bar(&bus, 1, x);
    place_bar(&bus, 2, y);
    assert_eq!(bus.shm_window(0).expect("window a").placed_at(), Some(x));
    assert_eq!(bus.shm_window(1).expect("window b").placed_at(), Some(y));

    // The swap, one function at a time. After the first write the two BARs
    // genuinely name the same address, so A cannot be mapped yet — no sweep
    // could fix that, and pretending otherwise would be the bug.
    place_bar(&bus, 1, y);
    assert_eq!(
        bus.shm_window(0).expect("window a").placed_at(),
        None,
        "A must not be mapped on top of B"
    );

    place_bar(&bus, 2, x);
    assert_eq!(
        bus.shm_window(0).expect("window a").placed_at(),
        Some(y),
        "the sweep released B before retrying A, so A must have landed"
    );
    assert_eq!(bus.shm_window(1).expect("window b").placed_at(), Some(x));
}
