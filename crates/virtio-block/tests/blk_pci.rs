//! End-to-end virtio-blk over the **virtio-pci** transport (EPIC 19).
//!
//! `blk_queue.rs` is the same device over virtio-mmio, and the point of having
//! both is the claim that devices are transport-agnostic: `virtio-block` was not
//! touched to add PCI, so if the round trips here match the ones there, the
//! transport really is the only thing that changed.
//!
//! Every test brings the device up the way Linux's `virtio_pci_modern` driver
//! does — through the common-configuration structure in the device's BAR, with
//! the same access widths — lays out descriptor chains by hand in a
//! `GuestMemoryMmap`, kicks the queue's own notification address and inspects the
//! used ring, the status byte and the backing file.
//!
//! Two groups, mirroring the mmio suite:
//!
//! * well-behaved driver: `IN`/`OUT` round trip, multi-descriptor reads, `FLUSH`,
//!   reset and re-initialisation, plus the PCI-specific parts — ISR
//!   read-to-clear, the capability records actually describing where the
//!   registers turned out to be, and two devices side by side;
//! * malicious guest: looped chains, out-of-range sectors, buffers outside guest
//!   RAM, oversized requests, garbage queue addresses, notifications for absent
//!   queues, and register accesses at widths and offsets no driver would use.
//!   None of them may panic, and none may take the device down.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use virtio_block::device::{VIRTIO_BLK_F_DISCARD, VIRTIO_BLK_F_WRITE_ZEROES};
use virtio_block::{
    BlockDevice, DISCARD_SECTOR_ALIGNMENT, MAX_DISCARD_SECTORS, MAX_DISCARD_SEG,
    MAX_WRITE_ZEROES_SECTORS, SECTOR_SIZE, S_IOERR, S_OK, S_UNSUPP, WRITE_ZEROES_FLAG_UNMAP,
    WRITE_ZEROES_MAY_UNMAP,
};
use virtio_core::chain::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::msix;
use virtio_core::pci::{self, common};
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine, TestMsiSink};
use virtio_core::{status, GuestMem, PciTransport, VIRTIO_F_VERSION_1};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20; // 1 MiB of guest RAM
const RING_BASE: u64 = 0x1000;
const RING_SIZE: u16 = 16;
/// Scratch guest buffers live well clear of the ring.
const BUF_BASE: u64 = 0x8000;
const DISK_SECTORS: u64 = 32;

/// virtio-blk request types, as the guest writes them.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

/// The MSI-X message a guest on x86 programs: the local APIC's default physical
/// address, and a data word whose low byte is the interrupt vector.
const MSI_ADDRESS: u64 = 0xfee0_0000;
const MSI_DATA_CONFIG: u32 = 0x4030;
const MSI_DATA_QUEUE: u32 = 0x4031;

static NEXT_IMAGE: AtomicUsize = AtomicUsize::new(0);

fn temp_image(sectors: u64) -> PathBuf {
    let dir = std::env::temp_dir().join("entangled-blk-pci-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let id = NEXT_IMAGE.fetch_add(1, Ordering::AcqRel);
    let path = dir.join(format!("img-{}-{id}.raw", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let file = File::options()
        .create_new(true)
        .write(true)
        .open(&path)
        .expect("create image");
    file.set_len(sectors * SECTOR_SIZE).expect("size image");
    path
}

/// A device wired behind a PCI transport, plus the guest-side ring.
struct Harness {
    mem: Arc<GuestMem>,
    ring: SplitRing,
    transport: PciTransport,
    irq: Arc<TestIrqLine>,
    /// The host MSI mechanism, for a function that publishes MSI-X. `None` for an
    /// INTx-only one, which is what most tests here use.
    sink: Option<Arc<TestMsiSink>>,
    path: PathBuf,
}

impl Harness {
    fn new(writable: bool) -> Self {
        Self::with_sectors(writable, DISK_SECTORS)
    }

    fn with_sectors(writable: bool, sectors: u64) -> Self {
        let path = temp_image(sectors);
        let device = BlockDevice::open(&path, writable).expect("open image");
        Self::around(device, path)
    }

    /// The same device on a function that publishes MSI-X, brought up with one
    /// vector for the request queue and one for configuration changes — which is
    /// exactly what Linux's `vp_find_vqs_msix` asks for.
    fn new_msix(writable: bool) -> Self {
        let path = temp_image(DISK_SECTORS);
        let device = BlockDevice::open(&path, writable).expect("open image");
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let sink = Arc::new(TestMsiSink::default());
        let transport = PciTransport::with_msix(
            0,
            Box::new(device),
            Arc::clone(&mem),
            irq.clone(),
            sink.clone(),
        )
        .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            ring: SplitRing::layout(RING_BASE, RING_SIZE),
            transport,
            irq,
            sink: Some(sink),
            path,
        };
        harness.bring_up();
        harness.enable_msix();
        harness
    }

    fn around(device: BlockDevice, path: PathBuf) -> Self {
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = PciTransport::new(0, Box::new(device), Arc::clone(&mem), irq.clone())
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            ring: SplitRing::layout(RING_BASE, RING_SIZE),
            transport,
            irq,
            sink: None,
            path,
        };
        harness.bring_up();
        harness
    }

    // ---------------------------------------------------------------- MSI-X

    /// BAR offset of table entry `vector`'s dword `dword`.
    fn table_at(vector: u64, dword: u64) -> u64 {
        pci::MSIX_TABLE_OFFSET + vector * msix::MSIX_ENTRY_SIZE + dword * 4
    }

    /// Programs one table entry through the BAR and leaves it unmasked, the way a
    /// driver's four `writel`s into the ioremapped table do.
    fn program_vector(&mut self, vector: u64, address: u64, data: u32) {
        self.write(Self::table_at(vector, 0), 4, address & 0xffff_ffff);
        self.write(Self::table_at(vector, 1), 4, address >> 32);
        self.write(Self::table_at(vector, 2), 4, u64::from(data));
        self.write(Self::table_at(vector, 3), 4, 0);
    }

    /// Masks or unmasks one vector, as `pci_msix_mask_irq` does.
    fn set_vector_mask(&mut self, vector: u64, masked: bool) {
        self.write(Self::table_at(vector, 3), 4, u64::from(masked));
    }

    /// What the machine's configuration space does when the guest writes the
    /// MSI-X message control register: publish the dword, then tell the transport
    /// it changed (`VirtioPciBus::io_write`).
    fn write_msix_control(&mut self, control: u16) {
        let handle = self
            .transport
            .msix_control_handle()
            .expect("this function publishes MSI-X");
        handle.store(
            u32::from(control) << (msix::MSIX_CONTROL_OFFSET as u32 * 8),
            Ordering::Release,
        );
        self.transport.msix_control_changed();
    }

    /// Assigns vector 0 to configuration changes and vector 1 to the request
    /// queue, programs both, and enables MSI-X.
    fn enable_msix(&mut self) {
        assert_eq!(self.transport.msix_table_size(), 2, "one queue plus config");
        self.program_vector(0, MSI_ADDRESS, MSI_DATA_CONFIG);
        self.program_vector(1, MSI_ADDRESS, MSI_DATA_QUEUE);
        self.write(common::CONFIG_MSIX_VECTOR, 2, 0);
        self.write(common::QUEUE_SELECT, 2, 0);
        self.write(common::QUEUE_MSIX_VECTOR, 2, 1);
        assert_eq!(self.read(common::CONFIG_MSIX_VECTOR, 2), 0);
        assert_eq!(self.read(common::QUEUE_MSIX_VECTOR, 2), 1);
        self.write_msix_control(msix::MSIX_CTRL_ENABLE);
        assert!(self.transport.msix_enabled());
    }

    /// Messages delivered so far, by their `data` field — which is what tells a
    /// driver which vector fired.
    fn msi_data(&self) -> Vec<u32> {
        self.sink.as_ref().map(|s| s.data()).unwrap_or_default()
    }

    /// The pending-bit array as the guest reads it.
    fn pba(&mut self) -> u64 {
        self.read(pci::MSIX_PBA_OFFSET, 8)
    }

    // ------------------------------------------------------------ registers

    /// A read of `len` bytes at `offset` inside the BAR, as a little-endian
    /// value — the shape every `vp_ioread*` takes.
    fn read(&mut self, offset: u64, len: usize) -> u64 {
        let mut data = vec![0u8; len];
        self.transport.read_bar(offset, &mut data);
        let mut bytes = [0u8; 8];
        bytes[..len].copy_from_slice(&data);
        u64::from_le_bytes(bytes)
    }

    fn write(&mut self, offset: u64, len: usize, value: u64) {
        self.transport
            .write_bar(offset, &value.to_le_bytes()[..len]);
    }

    fn status(&mut self) -> u8 {
        self.read(common::DEVICE_STATUS, 1) as u8
    }

    fn set_status(&mut self, value: u32) {
        self.write(common::DEVICE_STATUS, 1, u64::from(value));
    }

    fn device_features(&mut self) -> u64 {
        self.write(common::DEVICE_FEATURE_SELECT, 4, 0);
        let low = self.read(common::DEVICE_FEATURE, 4);
        self.write(common::DEVICE_FEATURE_SELECT, 4, 1);
        let high = self.read(common::DEVICE_FEATURE, 4);
        low | (high << 32)
    }

    /// Full driver bring-up, in `virtio_pci_modern`'s order and widths: reset,
    /// ACKNOWLEDGE, DRIVER, negotiate, program the ring, enable the queue,
    /// DRIVER_OK.
    fn bring_up(&mut self) {
        self.set_status(0);
        self.set_status(status::ACKNOWLEDGE);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER);

        let offered = self.device_features();
        // Accept everything the device offered, which is what the real driver
        // does for the features it knows.
        self.write(common::DRIVER_FEATURE_SELECT, 4, 0);
        self.write(common::DRIVER_FEATURE, 4, offered & 0xffff_ffff);
        self.write(common::DRIVER_FEATURE_SELECT, 4, 1);
        self.write(common::DRIVER_FEATURE, 4, offered >> 32);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);
        assert_ne!(
            u32::from(self.status()) & status::FEATURES_OK,
            0,
            "device must accept the features it offered"
        );

        let ring = self.ring;
        assert_eq!(
            self.read(common::NUM_QUEUES, 2),
            1,
            "virtio-blk has one queue"
        );
        self.write(common::QUEUE_SELECT, 2, 0);
        assert!(self.read(common::QUEUE_SIZE, 2) >= u64::from(RING_SIZE));
        self.write(common::QUEUE_SIZE, 2, u64::from(RING_SIZE));
        // Linux writes the 64-bit ring addresses as two 32-bit halves.
        for (field, addr) in [
            (common::QUEUE_DESC, ring.desc_table()),
            (common::QUEUE_DRIVER, ring.driver_area()),
            (common::QUEUE_DEVICE, ring.device_area()),
        ] {
            self.write(field, 4, addr & 0xffff_ffff);
            self.write(field + 4, 4, addr >> 32);
        }
        self.write(common::QUEUE_ENABLE, 2, 1);
        self.set_status(
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated(), "device must be live");
    }

    fn capacity_from_config(&mut self) -> u64 {
        let mut raw = [0u8; 8];
        self.transport.read_bar(pci::DEVICE_CFG_OFFSET, &mut raw);
        u64::from_le_bytes(raw)
    }

    /// A 32-bit device-config field, read at its natural width the way
    /// `virtio_cread` does.
    fn device_config32(&mut self, offset: u64) -> u32 {
        let mut raw = [0u8; 4];
        self.transport
            .read_bar(pci::DEVICE_CFG_OFFSET + offset, &mut raw);
        u32::from_le_bytes(raw)
    }

    fn device_config8(&mut self, offset: u64) -> u8 {
        let mut raw = [0u8; 1];
        self.transport
            .read_bar(pci::DEVICE_CFG_OFFSET + offset, &mut raw);
        raw[0]
    }

    /// A kick, the way the driver does it: a 16-bit write of the queue index to
    /// that queue's own notification address.
    fn notify(&mut self) {
        self.transport
            .write_bar(pci::queue_notify_offset(0), &0u16.to_le_bytes());
    }

    /// Reads (and thereby acknowledges) the ISR byte.
    fn take_isr(&mut self) -> u8 {
        self.read(pci::ISR_CFG_OFFSET, 1) as u8
    }

    // ------------------------------------------------------- guest memory

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        self.mem
            .write_slice(bytes, GuestAddress(addr))
            .expect("test write inside guest memory");
    }

    fn read_mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.mem
            .read_slice(&mut buf, GuestAddress(addr))
            .expect("test read inside guest memory");
        buf
    }

    fn write_header(&self, addr: u64, kind: u32, sector: u64) {
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&kind.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        self.write_mem(addr, &header);
    }

    fn submit(&self, descs: &[(u64, u32, u16)]) {
        let last = descs.len().saturating_sub(1);
        for (i, &(addr, len, flags)) in descs.iter().enumerate() {
            let index = u16::try_from(i).expect("test chains stay small");
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | VIRTQ_DESC_F_NEXT, index + 1)
            };
            self.ring
                .write_desc(&self.mem, index, addr, len, flags, next);
        }
        self.ring.publish(&self.mem, 0);
    }

    fn last_used(&self) -> (u32, u32) {
        let idx = self.ring.used_idx(&self.mem);
        assert!(idx > 0, "device did not add anything to the used ring");
        self.ring.used_elem(&self.mem, (idx - 1) % RING_SIZE)
    }

    // ---------------------------------------------------------- operations

    fn run_request(
        &mut self,
        kind: u32,
        sector: u64,
        data: &[(u64, u32, bool)],
        status_addr: u64,
    ) -> (u8, u32) {
        let header_addr = BUF_BASE;
        self.write_header(header_addr, kind, sector);
        // Poison the status byte so a device that never writes it is caught.
        self.write_mem(status_addr, &[0xff]);

        let mut descs = vec![(header_addr, 16u32, 0u16)];
        for &(addr, len, writable) in data {
            descs.push((addr, len, if writable { VIRTQ_DESC_F_WRITE } else { 0 }));
        }
        descs.push((status_addr, 1, VIRTQ_DESC_F_WRITE));
        self.submit(&descs);
        self.notify();

        let (head, len) = self.last_used();
        assert_eq!(head, 0, "used ring must report the chain head");
        (self.read_mem(status_addr, 1)[0], len)
    }

    fn write_sectors(&mut self, sector: u64, payload: &[u8]) -> u8 {
        let data_addr = BUF_BASE + 0x1000;
        let status_addr = BUF_BASE + 0x800;
        self.write_mem(data_addr, payload);
        let len = u32::try_from(payload.len()).expect("test payload fits");
        self.run_request(T_OUT, sector, &[(data_addr, len, false)], status_addr)
            .0
    }

    fn read_sectors(&mut self, sector: u64, len: usize) -> (u8, Vec<u8>) {
        let data_addr = BUF_BASE + 0x2000;
        let status_addr = BUF_BASE + 0x800;
        self.write_mem(data_addr, &vec![0u8; len]);
        let (status, _) = self.run_request(
            T_IN,
            sector,
            &[(data_addr, u32::try_from(len).expect("fits"), true)],
            status_addr,
        );
        (status, self.read_mem(data_addr, len))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ===================================================== well-behaved driver

#[test]
fn the_pci_identity_is_what_a_modern_driver_binds_on() {
    let h = Harness::new(true);
    // `vp_modern_probe` derives the virtio device id as `device - 0x1040`, so
    // virtio-blk (type 2) must be 0x1042, and the revision must be ≥ 1 or the
    // device looks transitional.
    assert_eq!(h.transport.device_id(), 0x1042);
    assert_eq!(
        h.transport.device_id() - pci::VIRTIO_PCI_DEVICE_ID_BASE,
        u16::from(virtio_core::DeviceType::Block as u8)
    );
    // Revision 0 is what marks a transitional (legacy-capable) device.
    const _: () = assert!(pci::VIRTIO_PCI_REVISION >= 1);
    assert_eq!(pci::VIRTIO_PCI_VENDOR_ID, 0x1af4);
    // Mass storage, so `lspci` says something true about it.
    assert_eq!(h.transport.class_code() >> 24, 0x01);
}

/// The capability records are the driver's only map of the BAR. If a record said
/// the common configuration lived somewhere it does not, bring-up would fail in a
/// way that looks like a broken device — so check the map against the territory.
#[test]
fn the_capability_records_point_at_the_registers_that_answer() {
    let mut h = Harness::new(true);
    for record in pci::capability_records() {
        let cfg_type = record[3];
        let offset = u64::from(u32::from_le_bytes([
            record[8], record[9], record[10], record[11],
        ]));
        match cfg_type {
            pci::VIRTIO_PCI_CAP_COMMON_CFG => {
                // num_queues is a register only the common config has.
                h.write(offset + common::QUEUE_SELECT, 2, 0);
                assert_eq!(h.read(offset + common::NUM_QUEUES, 2), 1);
            }
            pci::VIRTIO_PCI_CAP_DEVICE_CFG => {
                let mut raw = [0u8; 8];
                h.transport.read_bar(offset, &mut raw);
                assert_eq!(u64::from_le_bytes(raw), DISK_SECTORS, "blk capacity");
            }
            pci::VIRTIO_PCI_CAP_ISR_CFG => {
                // Something is pending after bring-up only if the device
                // signalled; either way this must not panic and must read clean.
                let _ = h.read(offset, 1);
                assert_eq!(h.read(offset, 1), 0, "the ISR is read-to-clear");
            }
            pci::VIRTIO_PCI_CAP_NOTIFY_CFG => {
                // The driver computes queue n's kick address as
                // `offset + n * notify_off_multiplier`; check that against the
                // address this transport actually decodes.
                let multiplier =
                    u32::from_le_bytes([record[16], record[17], record[18], record[19]]);
                for queue in 0..4u16 {
                    assert_eq!(
                        offset + u64::from(queue) * u64::from(multiplier),
                        pci::queue_notify_offset(queue),
                        "queue {queue}"
                    );
                }
            }
            other => panic!("unexpected capability type {other}"),
        }
    }
}

#[test]
fn advertises_capacity_and_features() {
    let mut h = Harness::new(true);
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);
    assert_ne!(h.device_features() & VIRTIO_F_VERSION_1, 0);
}

/// A read-only device — the shape an installer ISO is attached in
/// (`examples/ubuntu-uefi.toml`, `writable = false`) — must refuse writes over
/// the wire, not merely decline to advertise that it accepts them.
///
/// Both halves matter and neither implies the other:
///
/// * `VIRTIO_BLK_F_RO` is *advice*. A well-behaved driver reads it and mounts
///   read-only; nothing stops a broken or hostile one from sending `T_OUT`
///   anyway, and a firmware doing its own partition-table fixups is not a
///   hypothetical hostile driver.
/// * so the request itself is failed in band with `S_IOERR`, before the backend
///   is asked, and the file on disk is unchanged. Verified against the *bytes*,
///   because "the status byte said no" and "nothing was written" are different
///   claims — this is media whose SHA-256 we checked against a signed manifest,
///   and silently modifying it would invalidate that.
///
/// Reads keep working throughout, which is the whole point of attaching it.
#[test]
fn a_read_only_device_advertises_and_enforces_read_only() {
    let mut h = Harness::new(false);
    assert_ne!(
        h.device_features() & virtio_block::VIRTIO_BLK_F_RO,
        0,
        "a read-only image must offer VIRTIO_BLK_F_RO"
    );

    let before = std::fs::read(&h.path).expect("read the backing image");
    let payload = vec![0xa5u8; 512];
    assert_eq!(
        h.write_sectors(4, &payload),
        S_IOERR,
        "a write to a read-only device must fail in band"
    );
    // Sector 0 too: a partition-table rewrite is the write that would actually
    // happen, and it must fail exactly the same way.
    assert_eq!(h.write_sectors(0, &payload), S_IOERR);
    // FLUSH is not an error — it is a no-op on media that cannot be dirty — so a
    // driver that flushes on unmount does not see a spurious failure.
    let status_addr = BUF_BASE + 0x800;
    assert_eq!(h.run_request(T_FLUSH, 0, &[], status_addr).0, S_OK);

    assert_eq!(
        std::fs::read(&h.path).expect("re-read the backing image"),
        before,
        "the backing image must be byte-for-byte unchanged"
    );

    // And it is still a usable disk.
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);
    let (status, data) = h.read_sectors(4, 512);
    assert_eq!(status, S_OK, "reads must still work");
    assert_eq!(data, vec![0u8; 512]);
}

#[test]
fn write_then_read_round_trip() {
    let mut h = Harness::new(true);
    let payload: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();

    assert_eq!(h.write_sectors(4, &payload), S_OK);
    let (status, back) = h.read_sectors(4, payload.len());
    assert_eq!(status, S_OK);
    assert_eq!(back, payload);

    // A neighbouring sector was not touched.
    let (status, untouched) = h.read_sectors(6, 512);
    assert_eq!(status, S_OK);
    assert_eq!(untouched, vec![0u8; 512]);

    // Used buffers raised the line with the queue bit set, and reading the ISR
    // acknowledges it.
    assert!(h.irq.count() > 0, "device must signal used buffers");
    assert_eq!(h.take_isr() & pci::ISR_QUEUE, pci::ISR_QUEUE);
    assert_eq!(h.take_isr(), 0, "a second read finds nothing pending");
}

#[test]
fn read_spanning_multiple_data_descriptors() {
    let mut h = Harness::new(true);
    let payload: Vec<u8> = (0..1536u32).map(|i| (i % 97) as u8).collect();
    assert_eq!(h.write_sectors(0, &payload), S_OK);

    let (a, b, c) = (BUF_BASE + 0x2000, BUF_BASE + 0x3000, BUF_BASE + 0x4000);
    let (status, used_len) = h.run_request(
        T_IN,
        0,
        &[(a, 512, true), (b, 512, true), (c, 512, true)],
        BUF_BASE + 0x800,
    );
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1536 + 1, "data plus the status byte");

    let mut back = h.read_mem(a, 512);
    back.extend(h.read_mem(b, 512));
    back.extend(h.read_mem(c, 512));
    assert_eq!(back, payload);
}

#[test]
fn flush_reaches_the_disk() {
    let mut h = Harness::new(true);
    assert_eq!(h.write_sectors(1, &[0x11u8; 512]), S_OK);
    let (status, used_len) = h.run_request(T_FLUSH, 0, &[], BUF_BASE + 0x800);
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1, "flush only writes the status byte");

    let on_disk = std::fs::read(&h.path).expect("read image");
    assert_eq!(&on_disk[512..1024], &[0x11u8; 512]);
}

#[test]
fn written_data_survives_a_device_reset() {
    let mut h = Harness::new(true);
    let payload = [0x42u8; 512];
    assert_eq!(h.write_sectors(2, &payload), S_OK);

    // Driver-initiated reset: device_status = 0.
    h.set_status(0);
    assert_eq!(h.status(), 0);
    assert!(!h.transport.is_activated());
    assert_eq!(h.take_isr(), 0);
    // Queue registers are pristine again.
    h.write(common::QUEUE_SELECT, 2, 0);
    assert_eq!(h.read(common::QUEUE_ENABLE, 2), 0);
    assert_eq!(h.read(common::QUEUE_DESC, 8), 0);

    // A notify while down must be ignored, not crash.
    h.notify();

    h.ring.rewind(&h.mem);
    h.bring_up();

    let (status, back) = h.read_sectors(2, 512);
    assert_eq!(status, S_OK);
    assert_eq!(back, payload);
}

#[test]
fn two_devices_coexist_independently() {
    let mut vda = Harness::new(true);
    assert_eq!(vda.write_sectors(0, &[0xaau8; 512]), S_OK);

    let vdb_path = temp_image(DISK_SECTORS);
    std::fs::write(
        &vdb_path,
        vec![0xbbu8; (DISK_SECTORS * SECTOR_SIZE) as usize],
    )
    .expect("prefill vdb");
    let vdb_device = BlockDevice::open(&vdb_path, false).expect("open vdb");
    let mut vdb = Harness::around(vdb_device, vdb_path);

    let (status, a) = vda.read_sectors(0, 512);
    assert_eq!((status, a), (S_OK, vec![0xaau8; 512]));
    let (status, b) = vdb.read_sectors(0, 512);
    assert_eq!((status, b), (S_OK, vec![0xbbu8; 512]));

    // Writing the read-only one fails and does not disturb the other.
    assert_eq!(vdb.write_sectors(0, &[0u8; 512]), S_IOERR);
    let (status, a) = vda.read_sectors(0, 512);
    assert_eq!((status, a), (S_OK, vec![0xaau8; 512]));
}

#[test]
fn many_requests_in_one_notification() {
    let mut h = Harness::new(true);
    let count = 8u16;
    for i in 0..count {
        let header = BUF_BASE + u64::from(i) * 0x100;
        let status_addr = BUF_BASE + 0x4000 + u64::from(i) * 0x10;
        h.write_header(header, T_FLUSH, 0);
        h.write_mem(status_addr, &[0xff]);
        let base = i * 2;
        h.ring
            .write_desc(&h.mem, base, header, 16, VIRTQ_DESC_F_NEXT, base + 1);
        h.ring
            .write_desc(&h.mem, base + 1, status_addr, 1, VIRTQ_DESC_F_WRITE, 0);
        h.ring.publish(&h.mem, base);
    }
    h.notify();

    assert_eq!(h.ring.used_idx(&h.mem), count);
    for i in 0..count {
        let status_addr = BUF_BASE + 0x4000 + u64::from(i) * 0x10;
        assert_eq!(h.read_mem(status_addr, 1)[0], S_OK, "request {i}");
    }
    // One interrupt for the whole batch, not one per chain.
    assert_eq!(h.irq.count(), 1);
}

// ========================================================= malicious guest

#[test]
fn unknown_request_types_and_bad_sectors_fail_in_band() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;
    for bogus in [3u32, 7, 0xffff_ffff] {
        let (status, used_len) = h.run_request(bogus, 0, &[], status_addr);
        assert_eq!(status, S_UNSUPP, "type {bogus} must be unsupported");
        assert_eq!(used_len, 1);
    }
    for sector in [DISK_SECTORS, u64::MAX] {
        let (status, _) = h.read_sectors(sector, 512);
        assert_eq!(status, S_IOERR, "read at sector {sector} must fail");
    }
    // The transport is untroubled by any of it.
    assert_eq!(u32::from(h.status()) & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn buffers_outside_guest_memory_and_oversized_requests_are_io_errors() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;

    let (status, _) = h.run_request(T_IN, 0, &[(MEM_SIZE + 0x1000, 512, true)], status_addr);
    assert_eq!(status, S_IOERR);
    let (status, _) = h.run_request(T_OUT, 0, &[(0xdead_0000, 512, false)], status_addr);
    assert_eq!(status, S_IOERR);
    // A single descriptor claiming 4 GiB - 1 of payload.
    let (status, _) = h.run_request(T_IN, 0, &[(BUF_BASE + 0x2000, u32::MAX, true)], status_addr);
    assert_eq!(status, S_IOERR);

    // The disk is untouched and the device still healthy.
    let (status, data) = h.read_sectors(0, 512);
    assert_eq!((status, data), (S_OK, vec![0u8; 512]));
}

#[test]
fn looped_descriptor_chains_are_dropped_without_hanging() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_IN, 0);
    // Descriptor 0 chains to itself forever.
    h.ring
        .write_desc(&h.mem, 0, BUF_BASE, 16, VIRTQ_DESC_F_NEXT, 0);
    h.ring.publish(&h.mem, 0);
    h.notify();

    let (head, len) = h.last_used();
    assert_eq!(
        (head, len),
        (0, 0),
        "unusable chain returns with zero length"
    );
    assert_eq!(u32::from(h.status()) & status::DEVICE_NEEDS_RESET, 0);

    // The device still works afterwards.
    let (status, _) = h.read_sectors(0, 512);
    assert_eq!(status, S_OK);
}

/// The queues are validated by the same `QueueConfig::build` the mmio transport
/// uses, so garbage addresses must be refused at activation rather than becoming
/// host reads and writes at guest-chosen addresses.
#[test]
fn queue_enable_with_garbage_addresses_refuses_to_activate() {
    for (desc, driver, device) in [
        (0u64, 0x2000u64, 0x3000u64),        // unset descriptor table
        (0x1000, 0, 0x3000),                 // unset driver area
        (0x1000, 0x2000, MEM_SIZE + 0x1000), // used ring past guest RAM
        (0x1001, 0x2000, 0x3000),            // misaligned descriptor table
        (u64::MAX, u64::MAX, u64::MAX),      // nonsense
    ] {
        let path = temp_image(DISK_SECTORS);
        let blk = BlockDevice::open(&path, true).expect("open image");
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let mut t = PciTransport::new(0, Box::new(blk), mem, irq).expect("transport");

        let set_status = |t: &mut PciTransport, v: u32| {
            t.write_bar(common::DEVICE_STATUS, &[v as u8]);
        };
        set_status(&mut t, status::ACKNOWLEDGE);
        set_status(&mut t, status::ACKNOWLEDGE | status::DRIVER);
        t.write_bar(common::DRIVER_FEATURE_SELECT, &1u32.to_le_bytes());
        t.write_bar(
            common::DRIVER_FEATURE,
            &((VIRTIO_F_VERSION_1 >> 32) as u32).to_le_bytes(),
        );
        set_status(
            &mut t,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        t.write_bar(common::QUEUE_SELECT, &0u16.to_le_bytes());
        for (field, addr) in [
            (common::QUEUE_DESC, desc),
            (common::QUEUE_DRIVER, driver),
            (common::QUEUE_DEVICE, device),
        ] {
            t.write_bar(field, &addr.to_le_bytes());
        }
        t.write_bar(common::QUEUE_ENABLE, &1u16.to_le_bytes());
        set_status(
            &mut t,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );

        assert!(
            !t.is_activated(),
            "desc {desc:#x} driver {driver:#x} device {device:#x} must not activate"
        );
        assert_ne!(
            t.status() & status::DEVICE_NEEDS_RESET,
            0,
            "the driver must be told the device needs a reset"
        );
        // A kick against the un-activated device is inert, not a panic.
        t.write_bar(pci::queue_notify_offset(0), &0u16.to_le_bytes());
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn notifications_for_queues_the_device_does_not_have_are_dropped() {
    let mut h = Harness::new(true);
    // Publish a real chain, then kick every *other* slot in the notify region.
    h.write_header(BUF_BASE, T_FLUSH, 0);
    h.submit(&[(BUF_BASE, 16, 0), (BUF_BASE + 0x800, 1, VIRTQ_DESC_F_WRITE)]);
    for queue in [1u16, 2, 63, 1023] {
        h.transport
            .write_bar(pci::queue_notify_offset(queue), &queue.to_le_bytes());
    }
    // A misaligned write inside the region, and one past its end.
    h.transport.write_bar(pci::NOTIFY_CFG_OFFSET + 1, &[0, 0]);
    h.transport
        .write_bar(pci::NOTIFY_CFG_OFFSET + pci::NOTIFY_CFG_LEN, &[0, 0]);
    assert_eq!(
        h.ring.used_idx(&h.mem),
        0,
        "none of those may have run the device"
    );
    assert_eq!(u32::from(h.status()) & status::DEVICE_NEEDS_RESET, 0);

    // The real address still works.
    h.notify();
    assert_eq!(h.ring.used_idx(&h.mem), 1);
}

/// Config-space accesses beyond the device's own config structure, and register
/// accesses at widths no driver uses, must be inert rather than corrupting the
/// bring-up state.
#[test]
fn out_of_bounds_and_odd_width_register_accesses_are_inert() {
    let mut h = Harness::new(true);
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);

    // virtio-blk's config space is read-only; writes to it, in bounds or not,
    // change nothing.
    h.transport.write_bar(pci::DEVICE_CFG_OFFSET, &[0u8; 8]);
    h.transport
        .write_bar(pci::DEVICE_CFG_OFFSET + 0x900, &[0xffu8; 8]);
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);

    // Reads far past the config structure are zeroes, not a panic.
    let mut far = [0xffu8; 8];
    h.transport
        .read_bar(pci::DEVICE_CFG_OFFSET + 0x800, &mut far);
    assert_eq!(far, [0u8; 8]);

    // Writes at the wrong width, and unaligned ones, do not reach a field: the
    // device stays live with the same queue programmed.
    for (offset, len) in [
        (common::DEVICE_STATUS, 4),
        (common::QUEUE_SELECT, 1),
        (common::QUEUE_ENABLE, 4),
        (common::DEVICE_STATUS + 1, 1),
        (common::QUEUE_DESC + 2, 4),
    ] {
        h.write(offset, len, 0);
    }
    assert!(h.transport.is_activated(), "device must still be live");
    assert_eq!(h.read(common::QUEUE_DESC, 8), h.ring.desc_table());

    // Reads at every offset of the whole BAR: no width, no offset, may panic.
    for offset in (0..pci::VIRTIO_PCI_BAR_SIZE).step_by(0x111) {
        for len in [1usize, 2, 4, 8] {
            let mut data = vec![0xffu8; len];
            h.transport.read_bar(offset, &mut data);
        }
    }
    // …and neither may a read right at the end of the BAR, or past it.
    for offset in [
        pci::VIRTIO_PCI_BAR_SIZE - 1,
        pci::VIRTIO_PCI_BAR_SIZE,
        u64::MAX - 4,
    ] {
        let mut data = [0xffu8; 4];
        h.transport.read_bar(offset, &mut data);
        h.transport.write_bar(offset, &[0xff; 4]);
    }
    assert!(h.transport.is_activated());

    // And after all of that the device still serves a real request.
    let (status, _) = h.read_sectors(0, 512);
    assert_eq!(status, S_OK);
}

// ============================================================== MSI-X (EPIC 19)
//
// The same untouched `BlockDevice`, on a function that publishes MSI-X. If these
// round trips match the INTx ones above, then the interrupt mechanism really is
// the only thing that changed — which is the same claim the two transports make
// about each other, one level down.

/// The end-to-end claim: a real disk read completes, and the completion arrives
/// as the **MSI the driver programmed** rather than as an INTx edge.
#[test]
fn a_read_over_msix_completes_and_delivers_the_queues_own_vector() {
    let mut h = Harness::new_msix(true);
    let payload: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(h.write_sectors(1, &payload), S_OK);
    let before = h.msi_data().len();

    let (status, data) = h.read_sectors(1, 512);
    assert_eq!(status, S_OK);
    assert_eq!(data, payload, "the data must be the data");

    // Exactly one message, on the request queue's vector and nowhere else.
    assert_eq!(
        &h.msi_data()[before..],
        &[MSI_DATA_QUEUE],
        "the completion must arrive on the queue vector, not the config one"
    );
    assert_eq!(
        h.sink
            .as_ref()
            .expect("msix harness")
            .sent()
            .last()
            .copied(),
        Some(virtio_core::MsiMessage {
            address: MSI_ADDRESS,
            data: MSI_DATA_QUEUE
        })
    );
    // No INTx edge, and the ISR is unused under MSI-X (spec 4.1.4.5) — a driver
    // that read it would find nothing to acknowledge, which is correct.
    assert_eq!(h.irq.count(), 0, "INTx must stay silent while MSI-X is on");
    assert_eq!(h.take_isr(), 0);
    assert_eq!(h.pba(), 0, "nothing was left pending");
}

/// Several requests in a row: one message each, all on the same vector, and the
/// device stays live. The INTx equivalent counts edges; this counts messages.
#[test]
fn every_completion_is_its_own_message() {
    let mut h = Harness::new_msix(true);
    let before = h.msi_data().len();
    for sector in 0..4u64 {
        assert_eq!(h.read_sectors(sector, 512).0, S_OK);
    }
    assert_eq!(
        h.msi_data()[before..].to_vec(),
        vec![MSI_DATA_QUEUE; 4],
        "one message per completion, all on the queue's vector"
    );
    assert_eq!(h.irq.count(), 0);
}

/// A masked vector must not interrupt, and must not lose the interrupt either:
/// the request completes, the pending bit records that the device wanted
/// attention, and unmasking through the table delivers it.
#[test]
fn a_masked_vector_holds_the_completion_in_the_pba_until_it_is_unmasked() {
    let mut h = Harness::new_msix(true);
    let before = h.msi_data().len();
    h.set_vector_mask(1, true);

    // The data is available regardless — masking an interrupt does not stop a
    // device, which is exactly why a driver may poll while masked.
    let (status, _) = h.read_sectors(0, 512);
    assert_eq!(status, S_OK);
    assert_eq!(h.msi_data().len(), before, "no message while masked");
    assert_eq!(h.pba(), 1 << 1, "the pending bit names the masked vector");

    h.set_vector_mask(1, false);
    assert_eq!(&h.msi_data()[before..], &[MSI_DATA_QUEUE]);
    assert_eq!(h.pba(), 0);
    assert_eq!(h.irq.count(), 0, "and never as an INTx edge");
}

/// The function mask is a configuration-space bit rather than a table one, so it
/// takes the other route into the transport — and coalesces: MSI-X has one
/// pending bit per vector, not a counter, so several completions under the mask
/// release as one message.
#[test]
fn the_function_mask_coalesces_pending_completions_into_one_message() {
    let mut h = Harness::new_msix(true);
    let before = h.msi_data().len();
    h.write_msix_control(msix::MSIX_CTRL_ENABLE | msix::MSIX_CTRL_FUNCTION_MASK);

    for sector in 0..3u64 {
        assert_eq!(h.read_sectors(sector, 512).0, S_OK);
    }
    assert_eq!(h.msi_data().len(), before);
    assert_eq!(h.pba(), 1 << 1);

    h.write_msix_control(msix::MSIX_CTRL_ENABLE);
    assert_eq!(
        &h.msi_data()[before..],
        &[MSI_DATA_QUEUE],
        "one pending bit, one message"
    );
    assert_eq!(h.pba(), 0);
}

/// Disabling MSI-X mid-life must put the function back on INTx, which is what
/// `pci_free_irq_vectors` does on an unbind — and the device must not notice.
#[test]
fn disabling_msix_returns_the_function_to_intx() {
    let mut h = Harness::new_msix(true);
    let before = h.msi_data().len();
    h.write_msix_control(0);
    assert!(!h.transport.msix_enabled());

    assert_eq!(h.read_sectors(0, 512).0, S_OK);
    assert_eq!(h.msi_data().len(), before, "no MSI once disabled");
    assert_eq!(h.irq.count(), 1, "the INTx line carried it instead");
    assert_eq!(h.take_isr(), 1, "and the ISR says why");

    // Back to MSI-X, without re-programming anything: the table survived.
    h.write_msix_control(msix::MSIX_CTRL_ENABLE);
    assert_eq!(h.read_sectors(0, 512).0, S_OK);
    assert_eq!(&h.msi_data()[before..], &[MSI_DATA_QUEUE]);
    assert_eq!(h.irq.count(), 1, "no second edge");
}

/// A driver that assigns no vector to the request queue gets no interrupts, and
/// that is not a device failure: it is what `VIRTIO_MSI_NO_VECTOR` means. The
/// request still completes, which is how a polling driver works.
#[test]
fn a_queue_with_no_vector_completes_requests_without_interrupting() {
    let mut h = Harness::new_msix(true);
    let before = h.msi_data().len();
    h.write(common::QUEUE_SELECT, 2, 0);
    h.write(
        common::QUEUE_MSIX_VECTOR,
        2,
        u64::from(pci::VIRTIO_MSI_NO_VECTOR),
    );
    assert_eq!(
        h.read(common::QUEUE_MSIX_VECTOR, 2),
        u64::from(pci::VIRTIO_MSI_NO_VECTOR)
    );

    assert_eq!(h.read_sectors(0, 512).0, S_OK);
    assert_eq!(h.msi_data().len(), before);
    assert_eq!(h.irq.count(), 0, "and no INTx fallback per source either");
    assert_eq!(h.pba(), 0, "nothing pending: there is no vector to pend");
}

/// A hostile driver aiming a vector register outside the table, or writing the
/// table where there is no entry, may not take the device down — and the register
/// must report the refusal so the driver knows.
#[test]
fn hostile_vector_and_table_traffic_leaves_the_device_serving_requests() {
    let mut h = Harness::new_msix(true);
    for vector in [2u64, 3, 0xff, 0xfffe] {
        h.write(common::QUEUE_SELECT, 2, 0);
        h.write(common::QUEUE_MSIX_VECTOR, 2, vector);
        assert_eq!(
            h.read(common::QUEUE_MSIX_VECTOR, 2),
            u64::from(pci::VIRTIO_MSI_NO_VECTOR),
            "vector {vector} does not exist and must read back as refused"
        );
        h.write(common::CONFIG_MSIX_VECTOR, 2, vector);
        assert_eq!(
            h.read(common::CONFIG_MSIX_VECTOR, 2),
            u64::from(pci::VIRTIO_MSI_NO_VECTOR)
        );
    }
    // Table and PBA traffic at every offset and width, in and out of range.
    for offset in (pci::MSIX_TABLE_OFFSET..pci::MSIX_PBA_OFFSET + pci::MSIX_PBA_LEN).step_by(0x1ff)
    {
        for len in [1usize, 2, 4, 8] {
            let mut data = vec![0xffu8; len];
            h.transport.read_bar(offset, &mut data);
            h.transport.write_bar(offset, &vec![0xffu8; len]);
        }
    }
    assert!(h.transport.is_activated(), "device must still be live");

    // Re-programmed properly, it interrupts again — so none of the above left
    // the MSI-X state wedged.
    h.program_vector(1, MSI_ADDRESS, MSI_DATA_QUEUE);
    h.write(common::QUEUE_SELECT, 2, 0);
    h.write(common::QUEUE_MSIX_VECTOR, 2, 1);
    let before = h.msi_data().len();
    assert_eq!(h.read_sectors(0, 512).0, S_OK);
    assert_eq!(&h.msi_data()[before..], &[MSI_DATA_QUEUE]);
}

/// A device reset drops the vector assignments, so a driver that resets and
/// brings the device up again has to assign them afresh — and can.
#[test]
fn a_reset_drops_the_vectors_and_a_second_bring_up_restores_them() {
    let mut h = Harness::new_msix(true);
    h.set_status(0);
    assert!(!h.transport.is_activated());
    h.write(common::QUEUE_SELECT, 2, 0);
    assert_eq!(
        h.read(common::QUEUE_MSIX_VECTOR, 2),
        u64::from(pci::VIRTIO_MSI_NO_VECTOR),
        "a reset unassigns every vector"
    );
    // The table entry itself is PCI function state and survives the reset.
    assert_eq!(
        h.read(Harness::table_at(1, 2), 4),
        u64::from(MSI_DATA_QUEUE)
    );

    h.bring_up();
    h.enable_msix();
    let before = h.msi_data().len();
    assert_eq!(h.read_sectors(0, 512).0, S_OK);
    assert_eq!(&h.msi_data()[before..], &[MSI_DATA_QUEUE]);
}

// ====================================================== discard / write-zeroes
//
// The device is the same object as in the mmio suite, so the interesting part
// here is the *config space*: virtio-pci lets a driver read these fields at
// 1/2/4/8-byte widths from a BAR offset, where mmio only ever does aligned
// 32-bit reads. A field that decoded correctly on one and not the other would
// hand the guest a limit we do not enforce.

#[test]
fn the_reclaim_config_fields_decode_at_every_width_a_driver_uses() {
    let mut h = Harness::new(true);
    let features = h.device_features();
    assert_ne!(features & VIRTIO_BLK_F_DISCARD, 0);
    assert_ne!(features & VIRTIO_BLK_F_WRITE_ZEROES, 0);

    assert_eq!(h.device_config32(36), MAX_DISCARD_SECTORS);
    assert_eq!(h.device_config32(40), MAX_DISCARD_SEG);
    assert_eq!(h.device_config32(44), DISCARD_SECTOR_ALIGNMENT);
    assert_eq!(h.device_config32(48), MAX_WRITE_ZEROES_SECTORS);
    assert_eq!(h.device_config32(52), MAX_DISCARD_SEG);
    assert_eq!(h.device_config8(56), WRITE_ZEROES_MAY_UNMAP);

    // Byte-wise, the same field must read the same value — this is the decode
    // that only pci exercises.
    let expected = MAX_DISCARD_SECTORS.to_le_bytes();
    for (i, want) in expected.iter().enumerate() {
        assert_eq!(h.device_config8(36 + i as u64), *want, "byte {i}");
    }
    // A 16-bit read of the low half, and an 8-byte read spanning two fields.
    let mut half = [0u8; 2];
    h.transport.read_bar(pci::DEVICE_CFG_OFFSET + 36, &mut half);
    assert_eq!(
        u16::from_le_bytes(half),
        (MAX_DISCARD_SECTORS & 0xffff) as u16
    );
    let mut pair = [0u8; 8];
    h.transport.read_bar(pci::DEVICE_CFG_OFFSET + 36, &mut pair);
    assert_eq!(
        u64::from_le_bytes(pair),
        u64::from(MAX_DISCARD_SECTORS) | (u64::from(MAX_DISCARD_SEG) << 32)
    );
}

#[test]
fn discard_and_write_zeroes_round_trip_over_pci() {
    let mut h = Harness::with_sectors(true, 8 * 1024);
    let payload = vec![0xc3u8; 64 << 10];
    for piece in 0..8u64 {
        assert_eq!(h.write_sectors(piece * 128, &payload), S_OK);
    }
    let before = disk_image::allocated_bytes(&h.path).expect("allocated size");

    // One discard of the whole 1 MiB, one write-zeroes with unmap, and a
    // malformed one in between: all three answered in band.
    let array = BUF_BASE + 0x10000;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&2048u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    h.write_mem(array, &bytes);
    let status_addr = BUF_BASE + 0x800;
    let (status, used) = h.run_request(T_DISCARD, 0, &[(array, 16, false)], status_addr);
    assert_eq!(status, S_OK);
    assert_eq!(used, 1);

    // Reserved flag bits: unsupported, over pci exactly as over mmio.
    let mut bad = bytes.clone();
    bad[12..16].copy_from_slice(&0xdead_beefu32.to_le_bytes());
    h.write_mem(array, &bad);
    let (status, _) = h.run_request(T_WRITE_ZEROES, 0, &[(array, 16, false)], status_addr);
    assert_eq!(status, S_UNSUPP);

    // Write-zeroes with unmap over the second half.
    let mut zeroes = Vec::new();
    zeroes.extend_from_slice(&2048u64.to_le_bytes());
    zeroes.extend_from_slice(&2048u32.to_le_bytes());
    zeroes.extend_from_slice(&WRITE_ZEROES_FLAG_UNMAP.to_le_bytes());
    h.write_mem(array, &zeroes);
    let (status, _) = h.run_request(T_WRITE_ZEROES, 0, &[(array, 16, false)], status_addr);
    assert_eq!(status, S_OK);

    let after = disk_image::allocated_bytes(&h.path).expect("allocated size");
    assert!(
        after <= before,
        "reclaim must never grow the image: {before} -> {after}"
    );
    for piece in 0..8u64 {
        let (status, data) = h.read_sectors(piece * 128, 512);
        assert_eq!(status, S_OK);
        assert_eq!(
            data,
            vec![0u8; 512],
            "sectors from {} kept data",
            piece * 128
        );
    }
}
