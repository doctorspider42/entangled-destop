//! End-to-end virtio-blk tests over a real split virtqueue (backlog MVP-309,
//! EPIC 3 + EPIC 4 acceptance criteria).
//!
//! Each test brings the device up exactly the way a driver does — through the
//! virtio-mmio registers — lays out descriptor chains by hand in a
//! `GuestMemoryMmap`, kicks `QUEUE_NOTIFY` and inspects the used ring, the
//! status byte and the backing file.
//!
//! Two groups:
//!
//! * "well-behaved driver" round trips: `IN`, `OUT`, `FLUSH`, `GET_ID`,
//!   read-only rejection, reset survival, two coexisting devices;
//! * "malicious guest": looped chains, indices past the ring, buffers outside
//!   guest RAM, zero-length and oversized requests, out-of-range sectors,
//!   missing status bytes, indirect descriptors. None of them may panic.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use virtio_block::device::{VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_RO};
use virtio_block::{BlockDevice, RawDisk, SECTOR_SIZE, S_IOERR, S_OK, S_UNSUPP};
use virtio_core::chain::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::status;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, GuestMem, MmioTransport, VirtioDevice, VIRTIO_F_VERSION_1};
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
const T_GET_ID: u32 = 8;

static NEXT_IMAGE: AtomicUsize = AtomicUsize::new(0);

fn temp_image(sectors: u64) -> PathBuf {
    let dir = std::env::temp_dir().join("entangled-blk-queue-tests");
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

/// A device wired behind an mmio transport, plus the guest-side ring.
struct Harness {
    mem: Arc<GuestMem>,
    ring: SplitRing,
    transport: MmioTransport,
    irq: Arc<TestIrqLine>,
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

    fn around(device: BlockDevice, path: PathBuf) -> Self {
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq.clone())
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            ring: SplitRing::layout(RING_BASE, RING_SIZE),
            transport,
            irq,
            path,
        };
        harness.bring_up();
        harness
    }

    // ---------------------------------------------------------- registers

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    /// Full driver bring-up: read the offered features, accept all of them,
    /// program the ring, set DRIVER_OK.
    fn bring_up(&mut self) {
        assert_eq!(self.read32(mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(self.read32(mmio::VERSION_REG), 2);
        assert_eq!(self.read32(mmio::DEVICE_ID), 2, "virtio-blk device id");

        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = self.read32(mmio::DEVICE_FEATURES);
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = self.read32(mmio::DEVICE_FEATURES);

        self.write32(mmio::STATUS, status::ACKNOWLEDGE);
        self.write32(mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        self.write32(mmio::DRIVER_FEATURES_SEL, 0);
        self.write32(mmio::DRIVER_FEATURES, low);
        self.write32(mmio::DRIVER_FEATURES_SEL, 1);
        self.write32(mmio::DRIVER_FEATURES, high);
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_ne!(
            self.transport.status() & status::FEATURES_OK,
            0,
            "device must accept the features it offered"
        );

        let ring = self.ring;
        self.write32(mmio::QUEUE_SEL, 0);
        assert!(self.read32(mmio::QUEUE_NUM_MAX) >= u32::from(RING_SIZE));
        self.write32(mmio::QUEUE_NUM, u32::from(RING_SIZE));
        self.write32(mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
        self.write32(mmio::QUEUE_DESC_HIGH, 0);
        self.write32(mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
        self.write32(mmio::QUEUE_DRIVER_HIGH, 0);
        self.write32(mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
        self.write32(mmio::QUEUE_DEVICE_HIGH, 0);
        self.write32(mmio::QUEUE_READY, 1);
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated(), "device must be live");
    }

    fn device_features(&mut self) -> u64 {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = u64::from(self.read32(mmio::DEVICE_FEATURES));
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = u64::from(self.read32(mmio::DEVICE_FEATURES));
        low | (high << 32)
    }

    fn capacity_from_config(&mut self) -> u64 {
        let mut raw = [0u8; 8];
        self.transport.read(mmio::CONFIG_SPACE, &mut raw);
        u64::from_le_bytes(raw)
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

    /// Writes a 16-byte virtio-blk request header at `addr`.
    fn write_header(&self, addr: u64, kind: u32, sector: u64) {
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&kind.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        self.write_mem(addr, &header);
    }

    /// Chains `descs` (address, length, extra flags) starting at descriptor
    /// index 0 and publishes the head on the available ring.
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

    fn notify(&mut self) {
        self.write32(mmio::QUEUE_NOTIFY, 0);
    }

    /// `(used_idx, (head, len))` of the most recent used-ring entry.
    fn last_used(&self) -> (u16, (u32, u32)) {
        let idx = self.ring.used_idx(&self.mem);
        assert!(idx > 0, "device did not add anything to the used ring");
        let slot = (idx - 1) % RING_SIZE;
        (idx, self.ring.used_elem(&self.mem, slot))
    }

    // --------------------------------------------------------- operations

    /// Runs a request built from a header, optional data buffers and a status
    /// byte, and returns `(status_byte, used_len)`.
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

        let (_, (head, len)) = self.last_used();
        assert_eq!(head, 0, "used ring must report the chain head");
        let status = self.read_mem(status_addr, 1)[0];
        (status, len)
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
fn advertises_capacity_and_features() {
    let mut h = Harness::new(true);
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);
    let features = h.device_features();
    assert_ne!(features & VIRTIO_F_VERSION_1, 0);
    assert_ne!(features & VIRTIO_BLK_F_FLUSH, 0);
    assert_eq!(
        features & VIRTIO_BLK_F_RO,
        0,
        "writable disk must not set RO"
    );
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

    // Used buffers raised the interrupt with the vring bit set.
    assert!(h.irq.count() > 0, "device must signal used buffers");
    assert_ne!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
    h.write32(mmio::INTERRUPT_ACK, mmio::INT_VRING);
    assert_eq!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
}

#[test]
fn read_spanning_multiple_data_descriptors() {
    let mut h = Harness::new(true);
    let payload: Vec<u8> = (0..1536u32).map(|i| (i % 97) as u8).collect();
    assert_eq!(h.write_sectors(0, &payload), S_OK);

    // Same data, read back through three separate 512-byte buffers.
    let a = BUF_BASE + 0x2000;
    let b = BUF_BASE + 0x3000;
    let c = BUF_BASE + 0x4000;
    let status_addr = BUF_BASE + 0x800;
    let (status, used_len) = h.run_request(
        T_IN,
        0,
        &[(a, 512, true), (b, 512, true), (c, 512, true)],
        status_addr,
    );
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1536 + 1, "data plus the status byte");

    let mut back = h.read_mem(a, 512);
    back.extend(h.read_mem(b, 512));
    back.extend(h.read_mem(c, 512));
    assert_eq!(back, payload);
}

#[test]
fn get_id_returns_the_image_name() {
    let mut h = Harness::new(true);
    let expected = h
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .expect("image has a file name");

    let data_addr = BUF_BASE + 0x2000;
    let status_addr = BUF_BASE + 0x800;
    h.write_mem(data_addr, &[0u8; 20]);
    let (status, used_len) = h.run_request(T_GET_ID, 0, &[(data_addr, 20, true)], status_addr);
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 21);

    let id = h.read_mem(data_addr, 20);
    let expected_bytes = expected.as_bytes();
    let common = expected_bytes.len().min(20);
    assert_eq!(&id[..common], &expected_bytes[..common]);
}

#[test]
fn flush_is_acknowledged() {
    let mut h = Harness::new(true);
    assert_eq!(h.write_sectors(1, &[0x11u8; 512]), S_OK);
    let status_addr = BUF_BASE + 0x800;
    let (status, used_len) = h.run_request(T_FLUSH, 0, &[], status_addr);
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1, "flush only writes the status byte");

    // Data really is on disk.
    let on_disk = std::fs::read(&h.path).expect("read image");
    assert_eq!(&on_disk[512..1024], &[0x11u8; 512]);
}

#[test]
fn read_only_device_offers_ro_and_rejects_writes() {
    // Pre-fill the image, then attach it read-only (the MVP-408 ISO path).
    let path = temp_image(DISK_SECTORS);
    std::fs::write(&path, {
        let mut data = vec![0u8; (DISK_SECTORS * SECTOR_SIZE) as usize];
        data[..512].fill(0x77);
        data
    })
    .expect("prefill image");
    let device = BlockDevice::open(&path, false).expect("open read-only");
    assert!(device.is_read_only());
    let mut h = Harness::around(device, path);

    assert_ne!(h.device_features() & VIRTIO_BLK_F_RO, 0);

    // Reads work…
    let (status, data) = h.read_sectors(0, 512);
    assert_eq!(status, S_OK);
    assert_eq!(data, vec![0x77u8; 512]);

    // …writes are refused, and the image is unchanged.
    assert_eq!(h.write_sectors(0, &[0x00u8; 512]), S_IOERR);
    let (status, data) = h.read_sectors(0, 512);
    assert_eq!(status, S_OK);
    assert_eq!(data, vec![0x77u8; 512]);
}

#[test]
fn written_data_survives_a_device_reset() {
    let mut h = Harness::new(true);
    let payload = [0x42u8; 512];
    assert_eq!(h.write_sectors(2, &payload), S_OK);

    // Driver-initiated reset: STATUS = 0.
    h.write32(mmio::STATUS, 0);
    assert_eq!(h.transport.status(), 0);
    assert!(!h.transport.is_activated());
    assert_eq!(h.transport.interrupt_status(), 0);
    // Queue registers are pristine again.
    h.write32(mmio::QUEUE_SEL, 0);
    assert_eq!(h.read32(mmio::QUEUE_READY), 0);
    assert_eq!(h.read32(mmio::QUEUE_DESC_LOW), 0);

    // A notify while down must be ignored, not crash.
    h.notify();

    // The driver re-initialises the rings and brings the device back up.
    h.ring.rewind(&h.mem);
    h.bring_up();

    let (status, back) = h.read_sectors(2, 512);
    assert_eq!(status, S_OK);
    assert_eq!(back, payload);
}

#[test]
fn two_devices_coexist_independently() {
    // MVP-407: /dev/vda and /dev/vdb, the second one read-only.
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

    // Writing vdb fails and does not disturb vda.
    assert_eq!(vdb.write_sectors(0, &[0u8; 512]), S_IOERR);
    let (status, a) = vda.read_sectors(0, 512);
    assert_eq!((status, a), (S_OK, vec![0xaau8; 512]));

    // Their GET_ID replies differ.
    assert_ne!(vda.path.file_name(), vdb.path.file_name());
}

// ========================================================= malicious guest

#[test]
fn unknown_request_type_is_unsupported() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;
    for bogus in [3u32, 7, 11, 13, 0xffff_ffff] {
        let (status, used_len) = h.run_request(bogus, 0, &[], status_addr);
        assert_eq!(status, S_UNSUPP, "type {bogus} must be unsupported");
        assert_eq!(used_len, 1);
    }
}

#[test]
fn out_of_range_sectors_are_io_errors() {
    let mut h = Harness::new(true);
    for sector in [DISK_SECTORS, DISK_SECTORS + 1, u64::MAX, u64::MAX / 2] {
        let (status, _) = h.read_sectors(sector, 512);
        assert_eq!(status, S_IOERR, "read at sector {sector} must fail");
        assert_eq!(
            h.write_sectors(sector, &[1u8; 512]),
            S_IOERR,
            "write at sector {sector} must fail"
        );
    }
    // A request that starts in range but runs off the end.
    let (status, _) = h.read_sectors(DISK_SECTORS - 1, 4096);
    assert_eq!(status, S_IOERR);
}

#[test]
fn unaligned_lengths_are_io_errors() {
    let mut h = Harness::new(true);
    let (status, _) = h.read_sectors(0, 100);
    assert_eq!(status, S_IOERR);
    assert_eq!(h.write_sectors(0, &[0u8; 300]), S_IOERR);
}

#[test]
fn oversized_requests_are_rejected() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;
    // A single descriptor claiming 4 GiB - 1 of payload: far past the cap and
    // far past guest RAM. Must fail cleanly without staging anything.
    let (status, _) = h.run_request(T_IN, 0, &[(BUF_BASE + 0x2000, u32::MAX, true)], status_addr);
    assert_eq!(status, S_IOERR);

    // Many descriptors that together exceed MAX_REQUEST_BYTES.
    let mut data = Vec::new();
    for i in 0..8u64 {
        data.push((BUF_BASE + 0x2000 + i * 0x1000, 1 << 20, true));
    }
    let (status, _) = h.run_request(T_IN, 0, &data, status_addr);
    assert_eq!(status, S_IOERR);
}

#[test]
fn buffers_outside_guest_memory_are_io_errors() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;

    // Read into a buffer past the end of guest RAM: the sector range is fine,
    // so this exercises the checked guest-memory write path.
    let (status, _) = h.run_request(T_IN, 0, &[(MEM_SIZE + 0x1000, 512, true)], status_addr);
    assert_eq!(status, S_IOERR);

    // Write from a buffer past the end of guest RAM.
    let (status, _) = h.run_request(T_OUT, 0, &[(0xdead_0000, 512, false)], status_addr);
    assert_eq!(status, S_IOERR);

    // The disk is untouched.
    let (status, data) = h.read_sectors(0, 512);
    assert_eq!((status, data), (S_OK, vec![0u8; 512]));
}

#[test]
fn header_outside_guest_memory_drops_the_chain() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;
    h.write_mem(status_addr, &[0xff]);
    h.submit(&[(0xffff_0000, 16, 0), (status_addr, 1, VIRTQ_DESC_F_WRITE)]);
    h.notify();

    let (_, (head, len)) = h.last_used();
    assert_eq!(head, 0);
    assert_eq!(len, 0, "unusable chain is returned with zero length");
    // Status byte untouched, and the device is still healthy.
    assert_eq!(h.read_mem(status_addr, 1)[0], 0xff);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn header_too_short_drops_the_chain() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;
    h.write_header(BUF_BASE, T_IN, 0);
    h.write_mem(status_addr, &[0xff]);
    h.submit(&[(BUF_BASE, 8, 0), (status_addr, 1, VIRTQ_DESC_F_WRITE)]);
    h.notify();

    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
    assert_eq!(h.read_mem(status_addr, 1)[0], 0xff);
}

#[test]
fn chain_without_a_status_descriptor_is_dropped() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_IN, 0);
    // Header only: nothing device-writable at all, so there is nowhere to
    // report a result and the chain is unusable.
    h.submit(&[(BUF_BASE, 16, 0)]);
    h.notify();
    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn the_last_writable_buffer_is_always_the_status_byte() {
    // Per the spec the status byte is the final device-writable buffer, so a
    // driver that sends header + one writable buffer and expects it to be read
    // data gets a zero-length read whose status lands in that buffer. The
    // device must not treat the trailing buffer as payload.
    let mut h = Harness::new(true);
    let buffer = BUF_BASE + 0x2000;
    h.write_header(BUF_BASE, T_IN, 0);
    h.write_mem(buffer, &[0xffu8; 512]);
    h.submit(&[(BUF_BASE, 16, 0), (buffer, 512, VIRTQ_DESC_F_WRITE)]);
    h.notify();

    let (_, (head, len)) = h.last_used();
    assert_eq!(head, 0);
    assert_eq!(len, 1, "only the status byte was written");
    assert_eq!(h.read_mem(buffer, 1)[0], S_OK);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn zero_length_status_descriptor_is_dropped() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_FLUSH, 0);
    h.submit(&[(BUF_BASE, 16, 0), (BUF_BASE + 0x800, 0, VIRTQ_DESC_F_WRITE)]);
    h.notify();
    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
}

#[test]
fn empty_chain_is_dropped() {
    let mut h = Harness::new(true);
    // A single zero-length device-readable descriptor: no header, no status.
    h.ring.write_desc(&h.mem, 0, 0, 0, 0, 0);
    h.ring.publish(&h.mem, 0);
    h.notify();
    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn looped_descriptor_chain_is_dropped_without_hanging() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_IN, 0);
    // Descriptor 0 chains to itself forever.
    h.ring
        .write_desc(&h.mem, 0, BUF_BASE, 16, VIRTQ_DESC_F_NEXT, 0);
    h.ring.publish(&h.mem, 0);
    h.notify();

    let (_, (head, len)) = h.last_used();
    assert_eq!(head, 0);
    assert_eq!(len, 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // The device still works afterwards.
    let (status, _) = h.read_sectors(0, 512);
    assert_eq!(status, S_OK);
}

#[test]
fn two_descriptor_loop_is_dropped() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_IN, 0);
    h.ring
        .write_desc(&h.mem, 0, BUF_BASE, 16, VIRTQ_DESC_F_NEXT, 1);
    h.ring
        .write_desc(&h.mem, 1, BUF_BASE, 16, VIRTQ_DESC_F_NEXT, 0);
    h.ring.publish(&h.mem, 0);
    h.notify();

    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
}

#[test]
fn descriptor_next_index_past_the_ring_is_dropped() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_IN, 0);
    h.ring
        .write_desc(&h.mem, 0, BUF_BASE, 16, VIRTQ_DESC_F_NEXT, RING_SIZE + 40);
    h.ring.publish(&h.mem, 0);
    h.notify();

    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn available_ring_head_past_the_ring_needs_reset_but_does_not_panic() {
    let mut h = Harness::new(true);
    // Publish a head index the ring cannot contain. The used ring cannot
    // reference it either, so the device reports a host-visible failure and
    // the transport tells the driver to reset — but nothing panics.
    h.ring.set_avail_entry(&h.mem, 0, RING_SIZE + 7);
    h.ring.set_avail_idx(&h.mem, 1);
    h.notify();

    assert_ne!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
    assert_ne!(h.transport.interrupt_status() & mmio::INT_CONFIG, 0);
}

#[test]
fn available_index_far_ahead_of_the_device_is_refused() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_FLUSH, 0);
    h.submit(&[(BUF_BASE, 16, 0), (BUF_BASE + 0x800, 1, VIRTQ_DESC_F_WRITE)]);
    // Claim far more available entries than the ring can hold.
    h.ring.set_avail_idx(&h.mem, RING_SIZE * 4);
    h.notify();
    // virtio-queue refuses the iteration; nothing is processed and the host
    // neither loops nor panics.
    assert_eq!(h.ring.used_idx(&h.mem), 0);
}

#[test]
fn indirect_descriptors_are_dropped() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_IN, 0);
    // VIRTIO_F_INDIRECT_DESC is never offered, so this is a protocol violation.
    h.ring
        .write_desc(&h.mem, 0, BUF_BASE, 64, VIRTQ_DESC_F_INDIRECT, 0);
    h.ring.publish(&h.mem, 0);
    h.notify();

    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn writable_before_readable_chain_is_dropped() {
    let mut h = Harness::new(true);
    h.write_header(BUF_BASE, T_OUT, 0);
    // Device-writable status byte first, header second — the spec forbids it.
    h.submit(&[(BUF_BASE + 0x800, 1, VIRTQ_DESC_F_WRITE), (BUF_BASE, 16, 0)]);
    h.notify();
    let (_, (_, len)) = h.last_used();
    assert_eq!(len, 0);
}

#[test]
fn write_with_a_device_writable_payload_is_ignored_as_data() {
    let mut h = Harness::new(true);
    let data_addr = BUF_BASE + 0x1000;
    let status_addr = BUF_BASE + 0x800;
    h.write_mem(data_addr, &[0x33u8; 512]);

    // The payload is marked device-writable, so it is not write data: the
    // OUT request sees a zero-length payload and succeeds trivially, and the
    // disk stays empty.
    let (status, _) = h.run_request(T_OUT, 0, &[(data_addr, 512, true)], status_addr);
    assert_eq!(status, S_OK);
    let (status, data) = h.read_sectors(0, 512);
    assert_eq!((status, data), (S_OK, vec![0u8; 512]));
}

#[test]
fn zero_length_requests_are_harmless() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;

    // No data descriptors at all.
    let (status, used_len) = h.run_request(T_IN, 0, &[], status_addr);
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1);

    // A zero-length data descriptor.
    let (status, used_len) = h.run_request(T_IN, 0, &[(BUF_BASE + 0x2000, 0, true)], status_addr);
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1);

    // GET_ID with a zero-length reply buffer.
    let (status, used_len) =
        h.run_request(T_GET_ID, 0, &[(BUF_BASE + 0x2000, 0, true)], status_addr);
    assert_eq!(status, S_OK);
    assert_eq!(used_len, 1);
}

#[test]
fn get_id_without_a_reply_buffer_is_an_io_error() {
    let mut h = Harness::new(true);
    let status_addr = BUF_BASE + 0x800;
    let (status, used_len) = h.run_request(T_GET_ID, 0, &[], status_addr);
    assert_eq!(status, S_IOERR);
    assert_eq!(used_len, 1);
}

#[test]
fn guest_writes_to_the_config_space_are_ignored() {
    let mut h = Harness::new(true);
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);
    h.transport.write(mmio::CONFIG_SPACE, &[0u8; 8]);
    assert_eq!(h.capacity_from_config(), DISK_SECTORS);
    // Reads past the config space are zeroes, not a panic.
    let mut far = [0xffu8; 8];
    h.transport.read(mmio::CONFIG_SPACE + 0x400, &mut far);
    assert_eq!(far, [0u8; 8]);
}

#[test]
fn many_requests_in_one_notification() {
    let mut h = Harness::new(true);
    // Fill the ring with independent single-descriptor FLUSH chains and kick
    // once; the device must drain all of them.
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
}

#[test]
fn device_refuses_unknown_queue_indices() {
    let path = temp_image(DISK_SECTORS);
    let mut device = BlockDevice::open(&path, true).expect("open");
    // Not activated yet: any notify is an error, never a panic.
    assert!(device.notify(0).is_err());
    assert!(device.notify(1).is_err());
    assert!(device.notify(u16::MAX).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn raw_disk_can_be_attached_directly() {
    // The `with_disk` constructor path used by callers that open the file
    // themselves (installer flows).
    let path = temp_image(DISK_SECTORS);
    let file = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open");
    let disk = RawDisk::from_file(file, true).expect("wrap file");
    let device = BlockDevice::with_disk("scratch".into(), disk);
    assert_eq!(device.capacity_sectors(), DISK_SECTORS);
    assert_eq!(device.name(), "scratch");

    let mut h = Harness::around(device, path);
    assert_eq!(h.write_sectors(0, &[0x5au8; 512]), S_OK);
    let (status, back) = h.read_sectors(0, 512);
    assert_eq!((status, back), (S_OK, vec![0x5au8; 512]));
}
