//! Intel MultiProcessor Specification (v1.4) tables.
//!
//! Without an MP table or ACPI MADT the guest kernel logs "ACPI MADT or MP
//! tables are not detected" and falls back to 8259 virtual-wire mode, never
//! programming the IOAPIC. KVM's irqfd injections for the virtio GSIs are
//! then lost intermittently — observed as a stall on the first virtio-blk
//! read with INTERRUPT_STATUS still pending host-side (~1 boot in 3).
//! Publishing this table makes Linux route ISA IRQs 0..15 (serial IRQ 4 and
//! the virtio-mmio GSIs 5..9 included) through the in-kernel IOAPIC, like
//! every other KVM VMM does.
//!
//! The table is written into the classic BIOS scan window at
//! [`layout::MPTABLE_START`]; Linux probes 0xF0000..0xFFFFF for the "_MP_"
//! floating pointer regardless of E820 (the range is already marked
//! reserved in our map).

use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::layout;

/// MP spec limit for the 8-bit LAPIC id space we populate.
pub const MAX_MPTABLE_CPUS: u32 = 254;

/// ISA IRQs published as IOAPIC inputs (0..16 covers serial IRQ 4 and the
/// virtio window starting at [`layout::VIRTIO_MMIO_FIRST_IRQ`]).
const ISA_IRQ_COUNT: u8 = 16;

#[derive(Debug, thiserror::Error)]
pub enum MpTableError {
    #[error("cannot write MP tables to guest memory: {0}")]
    GuestMemory(String),

    #[error("{0} vCPUs exceed the MP table limit of {MAX_MPTABLE_CPUS}")]
    TooManyCpus(u32),
}

/// Builds and writes the floating pointer + configuration table for
/// `vcpu_count` processors. Call once after guest memory exists, before the
/// guest boots.
pub fn write<M: GuestMemory>(mem: &M, vcpu_count: u32) -> Result<(), MpTableError> {
    if vcpu_count == 0 || vcpu_count > MAX_MPTABLE_CPUS {
        return Err(MpTableError::TooManyCpus(vcpu_count));
    }
    let table = build(vcpu_count);
    mem.write_slice(&table, GuestAddress(layout::MPTABLE_START))
        .map_err(|e| MpTableError::GuestMemory(e.to_string()))
}

/// The full blob: 16-byte floating pointer immediately followed by the
/// configuration table it points at.
fn build(vcpu_count: u32) -> Vec<u8> {
    let config_addr = (layout::MPTABLE_START + 16) as u32;
    let ioapic_id = vcpu_count as u8; // above every LAPIC id (0..vcpu_count)

    // ---- configuration table body (entries) -----------------------------
    let mut entries = Vec::new();
    let mut entry_count: u16 = 0;

    // Processor entries, LAPIC id == index.
    for cpu in 0..vcpu_count as u8 {
        let mut e = [0u8; 20];
        e[0] = 0x00; // type: processor
        e[1] = cpu; // LAPIC id
        e[2] = 0x14; // LAPIC version
        e[3] = 0x01 | if cpu == 0 { 0x02 } else { 0x00 }; // EN | BP for CPU0
        e[4..8].copy_from_slice(&0x0600u32.to_le_bytes()); // family 6 signature
        e[8..12].copy_from_slice(&0x0201u32.to_le_bytes()); // FPU + APIC
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // Bus 0: ISA.
    let mut bus = [0u8; 8];
    bus[0] = 0x01;
    bus[1] = 0; // bus id
    bus[2..8].copy_from_slice(b"ISA   ");
    entries.extend_from_slice(&bus);
    entry_count += 1;

    // The IOAPIC.
    let mut ioapic = [0u8; 8];
    ioapic[0] = 0x02;
    ioapic[1] = ioapic_id;
    ioapic[2] = 0x11; // version
    ioapic[3] = 0x01; // enabled
    ioapic[4..8].copy_from_slice(&layout::IOAPIC_ADDR.to_le_bytes());
    entries.extend_from_slice(&ioapic);
    entry_count += 1;

    // I/O interrupt assignments: ISA IRQ n -> IOAPIC pin n, except the
    // timer (IRQ 0 -> pin 2, PC convention) and the cascade (IRQ 2, unused).
    for irq in 0..ISA_IRQ_COUNT {
        if irq == 2 {
            continue;
        }
        let pin = if irq == 0 { 2 } else { irq };
        let mut e = [0u8; 8];
        e[0] = 0x03; // type: I/O interrupt
        e[1] = 0x00; // INT (vectored)
        // flags 0: polarity/trigger conform to the ISA bus (edge, high)
        e[4] = 0; // source bus: ISA
        e[5] = irq;
        e[6] = ioapic_id;
        e[7] = pin;
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // Local interrupts: LINT0 = ExtINT, LINT1 = NMI, on all processors.
    for (lint, int_type) in [(0u8, 0x03u8), (1, 0x01)] {
        let mut e = [0u8; 8];
        e[0] = 0x04; // type: local interrupt
        e[1] = int_type;
        e[4] = 0; // source bus
        e[5] = 0; // source irq
        e[6] = 0xff; // destination: all LAPICs
        e[7] = lint;
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // ---- configuration table header (44 bytes) ---------------------------
    let base_length = (44 + entries.len()) as u16;
    let mut config = Vec::with_capacity(base_length as usize);
    config.extend_from_slice(b"PCMP");
    config.extend_from_slice(&base_length.to_le_bytes());
    config.push(0x04); // spec rev 1.4
    config.push(0); // checksum, patched below
    config.extend_from_slice(b"ENTANGLE"); // OEM id, 8 bytes
    config.extend_from_slice(b"DESKTOP     "); // product id, 12 bytes
    config.extend_from_slice(&0u32.to_le_bytes()); // OEM table pointer
    config.extend_from_slice(&0u16.to_le_bytes()); // OEM table size
    config.extend_from_slice(&entry_count.to_le_bytes());
    config.extend_from_slice(&layout::LAPIC_ADDR.to_le_bytes());
    config.extend_from_slice(&0u16.to_le_bytes()); // extended table length
    config.push(0); // extended table checksum
    config.push(0); // reserved
    debug_assert_eq!(config.len(), 44);
    config.extend_from_slice(&entries);
    let sum = checksum(&config);
    config[7] = sum.wrapping_neg();

    // ---- floating pointer structure (16 bytes) ---------------------------
    let mut fps = Vec::with_capacity(16);
    fps.extend_from_slice(b"_MP_");
    fps.extend_from_slice(&config_addr.to_le_bytes());
    fps.push(1); // length in 16-byte units
    fps.push(0x04); // spec rev 1.4
    fps.push(0); // checksum, patched below
    fps.extend_from_slice(&[0, 0, 0, 0, 0]); // features 1-5: config table present
    let sum = checksum(&fps);
    fps[10] = sum.wrapping_neg();
    debug_assert_eq!(fps.len(), 16);

    let mut blob = fps;
    blob.extend_from_slice(&config);
    blob
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_of(blob: &[u8]) -> &[u8] {
        &blob[16..]
    }

    #[test]
    fn floating_pointer_is_valid() {
        let blob = build(1);
        assert_eq!(&blob[..4], b"_MP_");
        assert_eq!(checksum(&blob[..16]), 0, "MPFPS checksum must sum to zero");
        let ptr = u32::from_le_bytes([blob[4], blob[5], blob[6], blob[7]]);
        assert_eq!(u64::from(ptr), layout::MPTABLE_START + 16);
    }

    #[test]
    fn config_table_is_valid_and_checksummed() {
        for cpus in [1u32, 2, 4] {
            let blob = build(cpus);
            let cfg = config_of(&blob);
            assert_eq!(&cfg[..4], b"PCMP");
            let base_length = u16::from_le_bytes([cfg[4], cfg[5]]) as usize;
            assert_eq!(base_length, cfg.len());
            assert_eq!(checksum(cfg), 0, "config checksum must sum to zero");

            // entries: cpus + bus + ioapic + 15 io-ints + 2 local ints
            let count = u16::from_le_bytes([cfg[34], cfg[35]]);
            assert_eq!(u32::from(count), cpus + 1 + 1 + 15 + 2);

            let lapic = u32::from_le_bytes([cfg[36], cfg[37], cfg[38], cfg[39]]);
            assert_eq!(lapic, layout::LAPIC_ADDR);
        }
    }

    #[test]
    fn virtio_irqs_are_routed_to_the_ioapic() {
        let blob = build(1);
        let cfg = config_of(&blob);
        let entries = &cfg[44..];
        // Walk entries; collect (source irq -> pin) from type-3 records.
        let mut offset = 0;
        let mut routes = Vec::new();
        while offset < entries.len() {
            match entries[offset] {
                0x00 => offset += 20,
                0x01 | 0x02 => offset += 8,
                0x03 => {
                    routes.push((entries[offset + 5], entries[offset + 7]));
                    offset += 8;
                }
                0x04 => offset += 8,
                other => panic!("unknown entry type {other}"),
            }
        }
        // Serial IRQ 4 and every virtio GSI (5..=9) identity-mapped; timer on pin 2.
        assert!(routes.contains(&(0, 2)));
        for irq in [4u8, 5, 6, 7, 8, 9] {
            assert!(routes.contains(&(irq, irq)), "missing route for IRQ {irq}");
        }
        assert!(!routes.iter().any(|&(src, _)| src == 2), "cascade skipped");
    }

    #[test]
    fn rejects_zero_and_too_many_cpus() {
        let mem =
            vm_memory::GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 2 << 20)]).unwrap();
        assert!(matches!(write(&mem, 0), Err(MpTableError::TooManyCpus(0))));
        assert!(matches!(
            write(&mem, 255),
            Err(MpTableError::TooManyCpus(255))
        ));
        write(&mem, 4).unwrap();
        let mut sig = [0u8; 4];
        mem.read_slice(&mut sig, GuestAddress(layout::MPTABLE_START))
            .unwrap();
        assert_eq!(&sig, b"_MP_");
    }
}
