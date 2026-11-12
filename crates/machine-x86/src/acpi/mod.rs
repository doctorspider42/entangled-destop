//! ACPI tables for the Entangled Desktop x86-64 machine (RSDP, XSDT, FADT,
//! FACS, MADT, DSDT).
//!
//! Why the machine needs them, in the order the need was discovered:
//!
//! * The guest kernel logged `ACPI MADT or MP tables are not detected` until
//!   [`crate::mptable`] appeared. The MP table fixes the interrupt topology but
//!   nothing else: no sleep states, no CPU objects, no PCI root bridge, and
//!   Linux has been deprecating MPS support for a decade.
//! * EDK2 CloudHv logs `InstallAcpiTables: Not Found` and, with no MADT to
//!   count CPUs with, `PlatformMaxCpuCountInitialization: boot CPU count
//!   unavailable` — after which it assumes **254** CPUs (ADR-0003 gap map).
//! * `poweroff` in a guest had nowhere to go: without a FADT and a `\_S5`
//!   package Linux logs `Power off not available` and parks the vCPU.
//!
//! ## Table inventory
//!
//! | Table | Rev | Where | Points at |
//! |---|---|---|---|
//! | RSDP  | 2 | [`layout::ACPI_RSDP_START`] | XSDT |
//! | XSDT  | 1 | +0x40 | FADT, MADT |
//! | FADT  | 6 | +0x80 | FACS, DSDT, the PM register block |
//! | FACS  | — | +0x200 | — |
//! | MADT  | 5 | after the FACS | — |
//! | DSDT  | 2 | after the MADT | — |
//!
//! Everything lands in one 64 KiB region ([`layout::ACPI_TABLES_START`]),
//! published to the guest as ACPI-reclaimable in both the E820 map and the PVH
//! memory map, and handed over explicitly through
//! `boot_params.acpi_rsdp_addr` (direct Linux) and `hvm_start_info.rsdp_paddr`
//! (UEFI/PVH).
//!
//! ## Coexistence with the MP table
//!
//! Both are published. The MADT is built from exactly the same topology as
//! [`crate::mptable`] — LAPIC ids `0..vcpu_count` (matching the per-vCPU APIC id
//! written into CPUID leaf 1 by `vmm_core::Vcpu::new`), one IOAPIC with id
//! `vcpu_count` at [`layout::IOAPIC_ADDR`], and ISA IRQ 0 routed to GSI 2 — so a
//! guest cannot see two different machines. Linux ignores the MP table
//! completely once it has an MADT (`acpi_boot_init` runs before
//! `default_get_smp_config`), so the MP table is now a fallback for
//! `acpi=off` and for anything that never learns the RSDP address.
//!
//! ## Table generation: hand-rolled
//!
//! rust-vmm's `acpi_tables` crate (Apache-2.0, so the licence gate would pass)
//! was the alternative. Rejected for this table set: it is a 0.x API whose main
//! value is its AML builder, and our DSDT has five objects in it; the tables
//! themselves are fixed-layout structs that are clearer written out with their
//! spec offsets than assembled through a builder. Hand-rolling also keeps this
//! module dependency-free and therefore portable — it compiles and its tests
//! run on the Windows/WHP host too, where `vm-memory` is not available.
//!
//! Everything here is pure byte generation over host-controlled inputs; the
//! only guest-facing surface in this module tree is [`pm::AcpiPmBlock`], which
//! documents its own untrusted-input handling.

pub mod aml;
pub mod pm;

use crate::layout;

pub use pm::AcpiPmBlock;

/// Same ceiling as the MP table's: the 8-bit LAPIC id space we populate.
pub const MAX_ACPI_CPUS: u32 = 254;

/// Length of an ACPI system description table header.
const SDT_HEADER_LEN: usize = 36;

/// Length of the RSDP (ACPI 2.0+).
const RSDP_LEN: usize = 36;

/// Length of the FADT we emit (ACPI 6.0+ layout, up to and including
/// `HypervisorVendorIdentity`).
const FADT_LEN: usize = 276;

/// Length of the FACS.
const FACS_LEN: usize = 64;

/// Alignment used for every table. 64 because the FACS *must* be 64-byte
/// aligned (ACPI 6.5 §5.2.10); using it everywhere keeps the addresses
/// predictable and the tables individually mappable.
const TABLE_ALIGN: u64 = 64;

/// `OEMID` in every table header.
const OEM_ID: &[u8; 6] = b"ENTANG";
/// `OEM Table ID` in every table header.
const OEM_TABLE_ID: &[u8; 8] = b"EDESKTOP";
/// `Creator ID` in every table header.
const CREATOR_ID: &[u8; 4] = b"ENTG";

/// `HypervisorVendorIdentity` in the FADT (ACPI 6.0+).
const HYPERVISOR_ID: &[u8; 8] = b"ENTANGLD";

#[derive(Debug, thiserror::Error)]
pub enum AcpiError {
    #[error("{0} vCPUs exceed the ACPI limit of {MAX_ACPI_CPUS}")]
    TooManyCpus(u32),

    #[error(
        "the ACPI tables need {needed:#x} bytes but only {available:#x} are reserved at {base:#x}"
    )]
    TooLarge {
        base: u64,
        needed: u64,
        available: u64,
    },

    #[error("cannot write the ACPI tables to guest memory: {0}")]
    GuestMemory(String),
}

/// The generated table set as one contiguous blob, plus where each table landed.
#[derive(Debug, Clone)]
pub struct AcpiTables {
    base: u64,
    blob: Vec<u8>,
    rsdp: u64,
    xsdt: u64,
    fadt: u64,
    facs: u64,
    madt: u64,
    dsdt: u64,
}

fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment) * alignment
}

impl AcpiTables {
    /// Builds the whole table set for `vcpu_count` processors, laid out to be
    /// written at [`layout::ACPI_TABLES_START`].
    pub fn new(vcpu_count: u32, mmio64_base: u64) -> Result<Self, AcpiError> {
        Self::at(
            layout::ACPI_TABLES_START,
            layout::ACPI_TABLES_SIZE,
            vcpu_count,
            mmio64_base,
        )
    }

    /// Builds the table set for an arbitrary base and region size. Exists so the
    /// tests can prove the region check fires without pretending the machine's
    /// layout is different.
    pub fn at(
        base: u64,
        region_size: u64,
        vcpu_count: u32,
        mmio64_base: u64,
    ) -> Result<Self, AcpiError> {
        if vcpu_count == 0 || vcpu_count > MAX_ACPI_CPUS {
            return Err(AcpiError::TooManyCpus(vcpu_count));
        }

        // Leaf tables first: their sizes decide where the tables that point at
        // them can go.
        let madt_bytes = madt(vcpu_count);
        let dsdt_bytes = dsdt(vcpu_count, mmio64_base);
        let facs_bytes = facs();

        let rsdp_at = base;
        let xsdt_at = align_up(rsdp_at + RSDP_LEN as u64, TABLE_ALIGN);
        let xsdt_len = (SDT_HEADER_LEN + 2 * 8) as u64; // FADT + MADT
        let fadt_at = align_up(xsdt_at + xsdt_len, TABLE_ALIGN);
        let facs_at = align_up(fadt_at + FADT_LEN as u64, TABLE_ALIGN);
        let madt_at = align_up(facs_at + FACS_LEN as u64, TABLE_ALIGN);
        let dsdt_at = align_up(madt_at + madt_bytes.len() as u64, TABLE_ALIGN);
        let end = dsdt_at + dsdt_bytes.len() as u64;

        let needed = end - base;
        if needed > region_size {
            return Err(AcpiError::TooLarge {
                base,
                needed,
                available: region_size,
            });
        }

        let fadt_bytes = fadt(facs_at, dsdt_at);
        let xsdt_bytes = xsdt(&[fadt_at, madt_at]);
        let rsdp_bytes = rsdp(xsdt_at);
        debug_assert_eq!(xsdt_bytes.len() as u64, xsdt_len);
        debug_assert_eq!(fadt_bytes.len(), FADT_LEN);

        let mut blob = vec![0u8; needed as usize];
        for (at, bytes) in [
            (rsdp_at, &rsdp_bytes),
            (xsdt_at, &xsdt_bytes),
            (fadt_at, &fadt_bytes),
            (facs_at, &facs_bytes),
            (madt_at, &madt_bytes),
            (dsdt_at, &dsdt_bytes),
        ] {
            let offset = (at - base) as usize;
            blob[offset..offset + bytes.len()].copy_from_slice(bytes);
        }

        Ok(Self {
            base,
            blob,
            rsdp: rsdp_at,
            xsdt: xsdt_at,
            fadt: fadt_at,
            facs: facs_at,
            madt: madt_at,
            dsdt: dsdt_at,
        })
    }

    /// Guest physical address of the RSDP — what goes into
    /// `boot_params.acpi_rsdp_addr` and `hvm_start_info.rsdp_paddr`.
    pub fn rsdp_address(&self) -> u64 {
        self.rsdp
    }

    pub fn xsdt_address(&self) -> u64 {
        self.xsdt
    }

    pub fn fadt_address(&self) -> u64 {
        self.fadt
    }

    pub fn facs_address(&self) -> u64 {
        self.facs
    }

    pub fn madt_address(&self) -> u64 {
        self.madt
    }

    pub fn dsdt_address(&self) -> u64 {
        self.dsdt
    }

    /// Where the blob is written.
    pub fn base_address(&self) -> u64 {
        self.base
    }

    /// The packed blob, ready to copy into guest memory at
    /// [`Self::base_address`].
    pub fn blob(&self) -> &[u8] {
        &self.blob
    }

    /// One table out of the blob, by its start address. Used by the tests (and
    /// `entangled doctor`) to re-parse what the guest will see.
    pub fn table_at(&self, address: u64, len: usize) -> Option<&[u8]> {
        let offset = usize::try_from(address.checked_sub(self.base)?).ok()?;
        self.blob.get(offset..offset.checked_add(len)?)
    }
}

// ---- table encoders ------------------------------------------------------

/// Sum of every byte, which a valid ACPI table's must be zero.
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Header + body with the length and checksum patched in.
fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let mut table = Vec::with_capacity(SDT_HEADER_LEN + body.len());
    table.extend_from_slice(signature);
    table.extend_from_slice(&((SDT_HEADER_LEN + body.len()) as u32).to_le_bytes());
    table.push(revision);
    table.push(0); // checksum, patched below
    table.extend_from_slice(OEM_ID);
    table.extend_from_slice(OEM_TABLE_ID);
    table.extend_from_slice(&1u32.to_le_bytes()); // OEM revision
    table.extend_from_slice(CREATOR_ID);
    table.extend_from_slice(&1u32.to_le_bytes()); // creator revision
    debug_assert_eq!(table.len(), SDT_HEADER_LEN);
    table.extend_from_slice(body);
    table[9] = checksum(&table).wrapping_neg();
    table
}

/// The Root System Description Pointer (ACPI 2.0+, revision 2: XSDT only).
fn rsdp(xsdt_addr: u64) -> Vec<u8> {
    let mut r = Vec::with_capacity(RSDP_LEN);
    r.extend_from_slice(b"RSD PTR "); // the trailing space is part of it
    r.push(0); // checksum over the first 20 bytes, patched below
    r.extend_from_slice(OEM_ID);
    r.push(2); // revision 2 = ACPI 2.0+
    r.extend_from_slice(&0u32.to_le_bytes()); // RsdtAddress: none, XSDT only
    r.extend_from_slice(&(RSDP_LEN as u32).to_le_bytes());
    r.extend_from_slice(&xsdt_addr.to_le_bytes());
    r.push(0); // extended checksum over all 36 bytes, patched below
    r.extend_from_slice(&[0u8; 3]); // reserved
    debug_assert_eq!(r.len(), RSDP_LEN);
    // The ACPI 1.0 checksum covers only the first 20 bytes, and must be
    // computed before the extended one.
    r[8] = checksum(&r[..20]).wrapping_neg();
    r[32] = checksum(&r).wrapping_neg();
    r
}

/// The Extended System Description Table: 64-bit pointers to every other table
/// except the FACS and the DSDT, which hang off the FADT.
fn xsdt(tables: &[u64]) -> Vec<u8> {
    let mut body = Vec::with_capacity(tables.len() * 8);
    for table in tables {
        body.extend_from_slice(&table.to_le_bytes());
    }
    sdt(b"XSDT", 1, &body)
}

/// The Firmware ACPI Control Structure. Not a described table: no checksum, and
/// it is reachable only through the FADT.
fn facs() -> Vec<u8> {
    let mut f = vec![0u8; FACS_LEN];
    f[0..4].copy_from_slice(b"FACS");
    f[4..8].copy_from_slice(&(FACS_LEN as u32).to_le_bytes());
    // HardwareSignature 0, FirmwareWakingVector 0 (no S3 support), GlobalLock 0.
    // Flags 0: no S4 bios support. Version 2 is the ACPI 4.0+ FACS.
    f[32] = 2; // Version
    f
}

/// A Generic Address Structure in system I/O space.
fn gas_io(port: u16, bit_width: u8, access_size: u8) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = 1; // AddressSpaceId: SystemIO
    g[1] = bit_width;
    g[2] = 0; // BitOffset
    g[3] = access_size; // 1 = byte, 2 = word, 3 = dword
    g[4..12].copy_from_slice(&u64::from(port).to_le_bytes());
    g
}

/// FADT `Flags` (ACPI 6.5 table 5.10).
const FADT_WBINVD: u32 = 1 << 0;
const FADT_PROC_C1: u32 = 1 << 2;
const FADT_P_LVL2_UP: u32 = 1 << 3;
/// "No power button device". **Not** set: the PM block implements the fixed
/// power button (`PWRBTN_STS`/`PWRBTN_EN` in PM1, raised on the SCI), which
/// is what lets the host ask a guest to shut down cleanly.
#[cfg(test)]
const FADT_PWR_BUTTON: u32 = 1 << 4;
/// "No sleep button device".
const FADT_SLP_BUTTON: u32 = 1 << 5;
/// "RTC wake status is not in fixed register space" — our RTC has no alarm.
const FADT_FIX_RTC: u32 = 1 << 6;
/// `RESET_REG_SUP`: the `RESET_REG`/`RESET_VALUE` pair below is real.
///
/// Set since ADR-0005. Without it Linux's `acpi_reboot()` is a no-op and the
/// guest walks the rest of its reboot ladder (keyboard controller, then 0xCF9,
/// then a triple fault) before anything reaches the host — which on WHP means
/// the last rung is absorbed by the hypervisor and the reboot never lands at
/// all. The register named is the one `crate::reset` implements.
const FADT_RESET_REG_SUP: u32 = 1 << 10;

/// FADT `IAPC_BOOT_ARCH` (ACPI 6.5 table 5.11).
///
/// Only `VGA_NOT_PRESENT` is set. Deliberately *not* set:
///
/// * `MSI_NOT_SUPPORTED` (bit 3) — true today (there is no PCI device with an
///   MSI capability) but it makes Linux print "ACPI FADT declares the system
///   doesn't support MSI, so disable it" and disable MSI globally, which would
///   silently defeat the first virtio-pci device with MSI-X (EPIC 19). An
///   absent capability already means no MSI; a global veto is a trap.
/// * `CMOS_RTC_NOT_PRESENT` (bit 5) — the UEFI machine *has* an MC146818 at
///   0x70/0x71 (`crate::rtc`). A direct-Linux guest does not, and setting the
///   bit there would save the ~1.4 s `rtc_cmos` probe timeout; that needs the
///   FADT to know the boot mode, which is a follow-up (see the acpi-machine
///   skill).
/// * `LEGACY_DEVICES` (bit 0) — no ISA bus behind the IOAPIC's ISA IRQs
///   beyond the UART and the keyboard controller.
///
/// `8042` (bit 1) is set: the keyboard controller (`crate::i8042`) is there.
const IAPC_VGA_NOT_PRESENT: u16 = 1 << 2;
/// FADT `IAPC_BOOT_ARCH` bit 1: an 8042 is present.
const IAPC_8042: u16 = 1 << 1;

/// The Fixed ACPI Description Table: where the ACPI PM register block is, and
/// where the DSDT and FACS are.
fn fadt(facs_addr: u64, dsdt_addr: u64) -> Vec<u8> {
    let mut body = vec![0u8; FADT_LEN - SDT_HEADER_LEN];
    // Offsets below are absolute FADT offsets, as the spec numbers them.
    let mut put = |offset: usize, bytes: &[u8]| {
        let at = offset - SDT_HEADER_LEN;
        body[at..at + bytes.len()].copy_from_slice(bytes);
    };

    put(36, &(facs_addr as u32).to_le_bytes()); // FIRMWARE_CTRL
    put(40, &(dsdt_addr as u32).to_le_bytes()); // DSDT
    put(45, &[0]); // Preferred_PM_Profile: unspecified
    put(46, &(layout::ACPI_SCI_GSI as u16).to_le_bytes()); // SCI_INT
                                                           // SMI_CMD stays 0: there is no SMI on this machine, so ACPICA must decide
                                                           // from PM1a_CNT.SCI_EN that the platform is already in ACPI mode. See
                                                           // `pm::PM1_CNT_SCI_EN`.
    put(56, &pm::PM1A_EVT_PORT.to_le_bytes()); // PM1a_EVT_BLK
    put(64, &pm::PM1A_CNT_PORT.to_le_bytes()); // PM1a_CNT_BLK
    put(76, &pm::PM_TIMER_PORT.to_le_bytes()); // PM_TMR_BLK
    put(80, &pm::GPE0_PORT.to_le_bytes()); // GPE0_BLK
    put(88, &[pm::PM1_EVT_LEN]);
    put(89, &[pm::PM1_CNT_LEN]);
    put(90, &[0]); // PM2_CNT_LEN: no PM2 control register
    put(91, &[pm::PM_TMR_LEN]);
    put(92, &[pm::GPE0_BLK_LEN]);
    // C2/C3 latencies above their "not supported" thresholds (100 / 1000 us).
    put(96, &0x0fffu16.to_le_bytes()); // P_LVL2_LAT
    put(98, &0x0fffu16.to_le_bytes()); // P_LVL3_LAT
    put(108, &[crate::rtc::REG_CENTURY]); // CENTURY
    put(109, &(IAPC_VGA_NOT_PRESENT | IAPC_8042).to_le_bytes());
    put(
        112,
        &(FADT_WBINVD
            | FADT_PROC_C1
            | FADT_P_LVL2_UP
            | FADT_SLP_BUTTON
            | FADT_FIX_RTC
            | FADT_RESET_REG_SUP)
            .to_le_bytes(),
    );
    // RESET_REG (116) and RESET_VALUE (128): the 0xCF9 reset control register
    // (`crate::reset`), written with a cold-reset value. This is the *first*
    // rung of Linux's reboot ladder (`acpi_reboot()`), which is why publishing
    // it makes a guest reboot prompt instead of a sequence of failed attempts
    // (ADR-0005).
    put(116, &gas_io(crate::reset::PORT_RESET_CONTROL, 8, 1));
    put(128, &[crate::reset::ACPI_RESET_VALUE]);
    put(131, &[0]); // FADT Minor Version: 6.0
    put(132, &facs_addr.to_le_bytes()); // X_FIRMWARE_CTRL
    put(140, &dsdt_addr.to_le_bytes()); // X_DSDT
                                        // The X_ blocks must describe the same registers as the 32-bit fields, with
                                        // widths that match the *_LEN fields — ACPICA warns loudly otherwise.
    put(148, &gas_io(pm::PM1A_EVT_PORT, 8 * pm::PM1_EVT_LEN, 2));
    put(172, &gas_io(pm::PM1A_CNT_PORT, 8 * pm::PM1_CNT_LEN, 2));
    put(208, &gas_io(pm::PM_TIMER_PORT, 8 * pm::PM_TMR_LEN, 3));
    put(220, &gas_io(pm::GPE0_PORT, 8 * pm::GPE0_BLK_LEN, 1));
    // The ACPI 5.0 sleep registers. Linux does not use them (we are not
    // hardware-reduced) but EDK2's CloudHv `ResetShutdown()` writes the control
    // one unconditionally, so they must describe the real ports.
    put(244, &gas_io(pm::SLEEP_CONTROL_PORT, 8, 1));
    put(256, &gas_io(pm::SLEEP_STATUS_PORT, 8, 1));
    put(268, HYPERVISOR_ID);

    sdt(b"FACP", 6, &body)
}

/// MADT entry types (ACPI 6.5 table 5.20).
const MADT_LAPIC: u8 = 0;
const MADT_IOAPIC: u8 = 1;
const MADT_INTERRUPT_SOURCE_OVERRIDE: u8 = 2;
const MADT_LAPIC_NMI: u8 = 4;

/// `Multiple APIC Flags`: PCAT_COMPAT — a dual-8259 setup is present (KVM's
/// in-kernel PIC), which is what lets Linux fall back to virtual wire mode.
const MADT_PCAT_COMPAT: u32 = 1 << 0;

/// MADT interrupt flags: polarity in bits 1:0, trigger mode in bits 3:2, where
/// 0 means "conforms to the bus specification" (for ISA: active high, edge).
const MADT_INTI_ACTIVE_HIGH_LEVEL: u16 = 0b1101;

/// ISA bus id, as used by the interrupt source overrides.
const ISA_BUS: u8 = 0;

/// The Multiple APIC Description Table. Same topology as [`crate::mptable`]:
/// LAPIC id == vCPU index, one IOAPIC with id `vcpu_count`, ISA IRQ 0 on
/// GSI 2.
fn madt(vcpu_count: u32) -> Vec<u8> {
    let ioapic_id = vcpu_count as u8; // above every LAPIC id (0..vcpu_count)
    let mut body = Vec::new();
    body.extend_from_slice(&layout::LAPIC_ADDR.to_le_bytes());
    body.extend_from_slice(&MADT_PCAT_COMPAT.to_le_bytes());

    // One enabled Local APIC per vCPU. The APIC id must match what CPUID leaf 1
    // EBX[31:24] reports for that vCPU (`vmm_core::Vcpu::new`) or firmware's
    // `GetBspNumber()` cannot find the BSP.
    for cpu in 0..vcpu_count as u8 {
        body.extend_from_slice(&[MADT_LAPIC, 8, cpu, cpu]);
        body.extend_from_slice(&1u32.to_le_bytes()); // flags: enabled
    }

    // The IOAPIC, with GSI base 0 so GSI n is pin n.
    body.extend_from_slice(&[MADT_IOAPIC, 12, ioapic_id, 0]);
    body.extend_from_slice(&layout::IOAPIC_ADDR.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // global system interrupt base

    // ISA IRQ 0 (the PIT) arrives on IOAPIC pin 2, the PC convention the MP
    // table already publishes. Flags 0 = conforms to the ISA bus (edge, high),
    // matching the MP table's flags for the same line.
    body.extend_from_slice(&[MADT_INTERRUPT_SOURCE_OVERRIDE, 10, ISA_BUS, 0]);
    body.extend_from_slice(&2u32.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());

    // The SCI, declared level-triggered/active-high as an interrupt of that
    // kind must be. Nothing in this machine drives it yet (there are no GPE
    // sources), but ACPICA installs a handler for `FADT.SCI_INT` and the GSI
    // has to carry the right trigger mode when a GPE source appears.
    body.extend_from_slice(&[
        MADT_INTERRUPT_SOURCE_OVERRIDE,
        10,
        ISA_BUS,
        layout::ACPI_SCI_GSI as u8,
    ]);
    body.extend_from_slice(&layout::ACPI_SCI_GSI.to_le_bytes());
    body.extend_from_slice(&MADT_INTI_ACTIVE_HIGH_LEVEL.to_le_bytes());

    // LINT1 = NMI on every processor, like the MP table's local interrupt
    // entries. (LINT0 = ExtINT has no MADT equivalent: Linux wires it up itself
    // from PCAT_COMPAT.)
    for cpu in 0..vcpu_count as u8 {
        body.extend_from_slice(&[MADT_LAPIC_NMI, 6, cpu]);
        body.extend_from_slice(&0u16.to_le_bytes()); // flags: bus conformant
        body.push(1); // Local APIC LINT#
    }

    sdt(b"APIC", 5, &body)
}

/// `\_S5`'s `SLP_TYP` value, and the one [`pm::AcpiPmBlock`] acts on.
const S5_SLP_TYP: u64 = pm::SLP_TYP_S5 as u64;

/// The Differentiated System Description Table: real AML, deliberately small.
///
/// * `\_S5` — the soft-off package. `acpi_sleep_init()` refuses to register the
///   power-off handler without it, which is the whole reason `poweroff` printed
///   `Power off not available`.
/// * `\_SB.PCI0` — the PCI root bridge (`_HID` `PNP0A03`), whose `_CRS`
///   publishes the bus number range, the `0xcf8`/`0xcfc` configuration ports and
///   the 32-bit MMIO aperture ([`layout::PCI_MMIO_HOLE_BASE`]). The window stops
///   below the virtio-mmio slots on purpose — see that constant.
/// * `\_SB.Cnnn` — one `ACPI0007` processor device per vCPU, `_UID` matching the
///   MADT's ACPI processor UID, which is how Linux pairs a CPU with its ACPI
///   object.
fn dsdt(vcpu_count: u32, mmio64_base: u64) -> Vec<u8> {
    let mut body = aml::name_path(
        &aml::root_name("_S5"),
        &aml::package(&[
            aml::integer(S5_SLP_TYP), // PM1a_CNT.SLP_TYP
            aml::integer(S5_SLP_TYP), // PM1b_CNT.SLP_TYP (no PM1b: ignored)
            aml::integer(0),          // reserved
            aml::integer(0),          // reserved
        ]),
    );

    let mut sb = Vec::new();

    let mut pci0 = aml::name("_HID", &aml::eisa_id("PNP0A03"));
    pci0.extend_from_slice(&aml::name("_ADR", &aml::integer(0)));
    pci0.extend_from_slice(&aml::name("_UID", &aml::integer(0)));
    let mut pci_resources = vec![
        aml::word_bus_number(0, 0),
        aml::io_port(crate::platform::PCI_CONFIG_ADDRESS, 8),
        // The legacy VGA ranges, ports 0x3b0-0x3df and memory
        // 0xa0000-0xbffff (ADR-0008): a VGA-class function is entitled
        // to them, and Windows gives a full WDDM driver of one (the 3D
        // virtio-gpu driver) code 12, "not enough free resources", when
        // the root bridge does not pass them down. Nothing decodes them;
        // BasicDisplay and the display-only driver never asked.
        aml::word_io(0x03b0, 0x03df),
        aml::dword_memory(0x000a_0000, 0x0002_0000),
        aml::dword_memory(
            layout::PCI_MMIO_HOLE_BASE as u32,
            layout::PCI_MMIO_HOLE_SIZE as u32,
        ),
    ];
    // The 64-bit aperture (EPIC 20): where a shared-memory BAR lives.
    // Prefetchable, because the BAR is, and Linux refuses to claim a
    // prefetchable BAR inside a window that is not. Left out for
    // [`NO_MMIO64`]: a 32-bit Windows XP stops with ACPI_BIOS_ERROR
    // (0xA5, 2) on a root bridge window it cannot address.
    if mmio64_base == NO_MMIO64 {
        // A BIOS-era machine's root bridge passes down every I/O port but
        // the configuration ones, as a PC's does. Its IDE controller is a
        // PCI function at the fixed ISA ports (0x1f0, 0x170, 0x3f6, 0x376),
        // and Windows XP gives a PCI device no port its bridge does not
        // own: with only the VGA range here the controller never started
        // (stop 0x7B). The two ranges replace the VGA one, which they cover.
        pci_resources[2] = aml::word_io(0x0000, 0x0cf7);
        pci_resources.insert(3, aml::word_io(0x0d00, 0xffff));
    }
    if mmio64_base != NO_MMIO64 {
        pci_resources.push(aml::qword_memory(
            mmio64_base,
            layout::PCI_MMIO64_SIZE,
            true,
        ));
    }
    pci0.extend_from_slice(&aml::name("_CRS", &aml::resource_template(&pci_resources)));
    if mmio64_base == NO_MMIO64 {
        pci0.extend_from_slice(&aml::name("_PRT", &legacy_prt()));
    }
    // The 8042 keyboard controller (`crate::i8042`, ADR-0008 §5): Windows
    // enumerates a PS/2 keyboard only through this device.
    let mut ps2 = aml::name("_HID", &aml::eisa_id("PNP0303"));
    ps2.extend_from_slice(&aml::name(
        "_CRS",
        &aml::resource_template(&[
            aml::io_port(crate::i8042::PORT_DATA, 1),
            aml::io_port(crate::i8042::PORT_COMMAND, 1),
            aml::irq_no_flags(crate::i8042::KEYBOARD_IRQ as u8),
        ]),
    ));
    // Where the keyboard hangs. Beside the root bridge for every machine so
    // far; *under* it for a BIOS-era one (ADR-0012), as on a real board,
    // where it sits behind the PCI-ISA bridge. Windows XP reserves a
    // device's interrupt when it enumerates it, and its ACPI driver can
    // route interrupts only once the root bridge has started; a keyboard
    // that is the bridge's sibling is enumerated before that, and the
    // reservation goes through a null pointer (stop 0x7E in `acpi.sys`).
    // A child is enumerated after its parent starts.
    let ps2k = aml::device("PS2K", &ps2);
    if mmio64_base == NO_MMIO64 {
        pci0.extend_from_slice(&ps2k);
        sb.extend_from_slice(&aml::device("PCI0", &pci0));
    } else {
        sb.extend_from_slice(&aml::device("PCI0", &pci0));
        sb.extend_from_slice(&ps2k);
    }

    for cpu in 0..vcpu_count {
        let mut processor = aml::name("_HID", &aml::string("ACPI0007"));
        processor.extend_from_slice(&aml::name("_UID", &aml::integer(u64::from(cpu))));
        sb.extend_from_slice(&aml::device(&cpu_name(cpu), &processor));
    }

    body.extend_from_slice(&aml::scope(&aml::root_name("_SB"), &sb));

    // Revision 2 or higher tells the interpreter integers are 64-bit.
    sdt(b"DSDT", 2, &body)
}

/// `_PRT` for a BIOS-era machine (ADR-0012): which interrupt each PCI
/// device's `INTA#` is wired to.
///
/// The wiring is the bus's own: device `n + 1` holds slot `n`, and its pin
/// goes to [`layout::virtio_irq`]`(n)`, the same number the function reports
/// in `interrupt_line`. Every guest before Windows XP found that register
/// enough. XP's ACPI driver takes the routing interface from the PCI driver
/// only when the root bridge has a `_PRT`, and without it the first PCI
/// function with an interrupt pin sends it through a null pointer (stop
/// 0x7E in `acpi.sys`). Each entry is hard-wired: source 0, and the global
/// system interrupt as the index.
fn legacy_prt() -> Vec<u8> {
    let entries: Vec<Vec<u8>> = (0..31u32)
        .filter_map(|slot| {
            let gsi = layout::virtio_irq(slot as usize)?;
            Some(aml::package(&[
                // Device `slot + 1`, any function.
                aml::dword(((slot + 1) << 16) | 0xffff),
                aml::integer(0), // INTA#
                aml::integer(0), // hard-wired, no link device
                aml::integer(u64::from(gsi)),
            ]))
        })
        .collect();
    aml::package(&entries)
}

/// `C000`.. — the AML name of the processor device for `cpu`. Four characters,
/// which is exactly one `NameSeg`, for every id the MADT can hold.
fn cpu_name(cpu: u32) -> String {
    format!("C{:03X}", cpu & 0xfff)
}

// ---- publication ---------------------------------------------------------

/// Builds the tables for `vcpu_count` processors and writes them into guest
/// memory at [`layout::ACPI_TABLES_START`], returning the RSDP address.
///
/// Call once after guest memory exists and before the guest boots, next to
/// [`crate::mptable::write`]. The boot paths do **not** call this: they only
/// advertise [`layout::ACPI_RSDP_START`], so a machine that forgets this call
/// leaves a zeroed region there, the RSDP signature check fails and the guest
/// falls back to the MP table instead of reading garbage.
pub fn write<M: vm_memory::GuestMemory>(mem: &M, vcpu_count: u32) -> Result<u64, AcpiError> {
    use vm_memory::{Address, Bytes, GuestAddress};

    // The 64-bit aperture starts at the top of RAM, which the memory object
    // already knows — so this stays a two-argument call for all seventeen of
    // its callers instead of threading a size through every one of them.
    let mmio64_base = layout::pci_mmio64_base_above(mem.last_addr().raw_value());
    let tables = AcpiTables::new(vcpu_count, mmio64_base)?;
    mem.write_slice(tables.blob(), GuestAddress(tables.base_address()))
        .map_err(|e| AcpiError::GuestMemory(e.to_string()))?;
    tracing::info!(
        rsdp = format_args!("{:#x}", tables.rsdp_address()),
        xsdt = format_args!("{:#x}", tables.xsdt_address()),
        fadt = format_args!("{:#x}", tables.fadt_address()),
        madt = format_args!("{:#x}", tables.madt_address()),
        dsdt = format_args!("{:#x}", tables.dsdt_address()),
        mmio64 = format_args!("{mmio64_base:#x}"),
        bytes = tables.blob().len(),
        vcpus = vcpu_count,
        "published ACPI tables"
    );
    Ok(tables.rsdp_address())
}

/// The `mmio64_base` of a machine whose PCI root bridge has no window above
/// 4 GiB (ADR-0012).
pub const NO_MMIO64: u64 = 0;

/// [`write`] for a machine that boots through a BIOS (ADR-0012): the same
/// tables without the root bridge's 64-bit window, plus an RSDT.
///
/// Both are for a guest of the BIOS era. Windows XP follows the RSDT and
/// knows no XSDT ([`write_rsdt`]), and being 32-bit it refuses a `_CRS` that
/// names memory it cannot address. The machine calls this after [`write`],
/// over the same region; the tables land at the same addresses.
pub fn write_legacy<M: vm_memory::GuestMemory>(mem: &M, vcpu_count: u32) -> Result<u64, AcpiError> {
    use vm_memory::{Bytes, GuestAddress};
    let tables = AcpiTables::new(vcpu_count, NO_MMIO64)?;
    mem.write_slice(tables.blob(), GuestAddress(tables.base_address()))
        .map_err(|e| AcpiError::GuestMemory(e.to_string()))?;
    write_rsdt(mem)?;
    Ok(tables.rsdp_address())
}

/// Where [`write_rsdt`] puts the RSDT: half-way into the tables' region,
/// far past anything [`write`] lays out.
const RSDT_OFFSET: u64 = layout::ACPI_TABLES_SIZE / 2;

/// Adds an RSDT to the tables [`write`] published, for a guest that predates
/// the XSDT (ADR-0012).
///
/// The tables are XSDT-only, which every guest so far reads. Windows XP's
/// loader does not: it follows the RSDP's 32-bit `RsdtAddress`, finds 0, and
/// takes whatever is at physical address 4 for a table length. So a machine
/// that boots through a BIOS calls this after [`write`]: the RSDT lists the
/// XSDT's tables with 32-bit pointers, and the RSDP in guest memory gets its
/// address and new checksums. Every other machine keeps the RSDP it had.
pub fn write_rsdt<M: vm_memory::GuestMemory>(mem: &M) -> Result<u64, AcpiError> {
    use vm_memory::{Bytes, GuestAddress};
    let memory = |e: vm_memory::GuestMemoryError| AcpiError::GuestMemory(e.to_string());

    let rsdp_at = layout::ACPI_RSDP_START;
    let mut rsdp = [0u8; RSDP_LEN];
    mem.read_slice(&mut rsdp, GuestAddress(rsdp_at))
        .map_err(memory)?;
    let u64_at = |bytes: &[u8], at: usize| {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(raw)
    };
    let xsdt_at = u64_at(&rsdp, 24);
    let mut header = [0u8; SDT_HEADER_LEN];
    mem.read_slice(&mut header, GuestAddress(xsdt_at))
        .map_err(memory)?;
    // These are tables this module wrote a moment ago, not guest input, but
    // the length is still read out of guest memory: bound it by the region.
    let xsdt_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if &rsdp[..8] != b"RSD PTR " || &header[..4] != b"XSDT" || xsdt_len > RSDT_OFFSET as usize {
        return Err(AcpiError::GuestMemory(
            "no ACPI tables to add an RSDT to: call acpi::write first".into(),
        ));
    }
    let mut entries = vec![0u8; xsdt_len.saturating_sub(SDT_HEADER_LEN)];
    mem.read_slice(&mut entries, GuestAddress(xsdt_at + SDT_HEADER_LEN as u64))
        .map_err(memory)?;
    let mut body = Vec::with_capacity(entries.len() / 2);
    for entry in entries.chunks_exact(8) {
        // Every table sits below 1 MiB, so the pointers fit.
        body.extend_from_slice(&(u64_at(entry, 0) as u32).to_le_bytes());
    }
    let rsdt = sdt(b"RSDT", 1, &body);
    let rsdt_at = layout::ACPI_TABLES_START + RSDT_OFFSET;
    mem.write_slice(&rsdt, GuestAddress(rsdt_at))
        .map_err(memory)?;

    rsdp[16..20].copy_from_slice(&(rsdt_at as u32).to_le_bytes());
    rsdp[8] = 0;
    rsdp[32] = 0;
    rsdp[8] = checksum(&rsdp[..20]).wrapping_neg();
    rsdp[32] = checksum(&rsdp).wrapping_neg();
    mem.write_slice(&rsdp, GuestAddress(rsdp_at))
        .map_err(memory)?;
    tracing::info!(
        rsdt = format_args!("{rsdt_at:#x}"),
        tables = body.len() / 4,
        "published an RSDT beside the XSDT"
    );
    Ok(rsdt_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BIOS machine's root bridge names no memory above 4 GiB.
    #[test]
    fn the_legacy_tables_have_no_64_bit_window() {
        let with = AcpiTables::new(1, TEST_MMIO64_BASE).unwrap();
        let without = AcpiTables::new(1, NO_MMIO64).unwrap();
        // A QWord address space descriptor is 0x8a with a 43-byte body.
        let has_qword =
            |tables: &AcpiTables| tables.blob().windows(3).any(|w| w == [0x8a, 0x2b, 0x00]);
        assert!(has_qword(&with));
        assert!(!has_qword(&without));
    }

    /// A BIOS machine's root bridge routes its devices' interrupts, and the
    /// keyboard is its child; every other machine's tables have neither.
    #[test]
    fn the_legacy_root_bridge_has_a_prt_and_the_keyboard_under_it() {
        let find = |tables: &AcpiTables, name: &[u8]| {
            tables.blob().windows(name.len()).position(|w| w == name)
        };
        let legacy = AcpiTables::new(1, NO_MMIO64).unwrap();
        let default = AcpiTables::new(1, TEST_MMIO64_BASE).unwrap();
        assert!(find(&default, b"_PRT").is_none());
        // The legacy bridge owns the I/O space on both sides of 0xcf8.
        let io_window = |min: u16, max: u16| {
            let mut bytes = vec![0x88, 0x0d, 0x00, 0x01];
            bytes.extend_from_slice(&aml::word_io(min, max)[4..]);
            bytes
        };
        assert!(find(&legacy, &io_window(0x0000, 0x0cf7)).is_some());
        assert!(find(&legacy, &io_window(0x0d00, 0xffff)).is_some());
        assert!(find(&default, &io_window(0x0000, 0x0cf7)).is_none());
        let prt = find(&legacy, b"_PRT").expect("a _PRT");
        // The first entry: device 1, INTA#, hard-wired to the first PCI IRQ.
        let entry = [0x0c, 0xff, 0xff, 0x01, 0x00];
        let first = legacy.blob()[prt..]
            .windows(entry.len())
            .position(|w| w == entry)
            .expect("an entry for device 1");
        assert!(first < 16, "the entry opens the package");
        // PCI0's package length covers PS2K only in the legacy tables: the
        // keyboard comes before the first processor either way, so compare
        // where PCI0's body ends through the next sibling's position.
        let (pci0, ps2k) = (
            find(&legacy, b"PCI0").unwrap(),
            find(&legacy, b"PS2K").unwrap(),
        );
        let pkg_len = |tables: &AcpiTables, at: usize| {
            // DeviceOp (5b 82) PkgLength NameSeg: the length sits between.
            let lead = tables.blob()[at - 2];
            let low = tables.blob()[at - 1];
            (usize::from(lead & 0x0f)) | (usize::from(low) << 4)
        };
        assert!(pci0 + pkg_len(&legacy, pci0) > ps2k, "PS2K inside PCI0");
        let (pci0, ps2k) = (
            find(&default, b"PCI0").unwrap(),
            find(&default, b"PS2K").unwrap(),
        );
        assert!(pci0 + pkg_len(&default, pci0) < ps2k, "PS2K beside PCI0");
    }

    /// The RSDT a BIOS machine adds lists the XSDT's tables, and the RSDP
    /// that points at it still checksums.
    #[test]
    fn an_rsdt_can_be_added_beside_the_xsdt() {
        use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 2 << 20)]).unwrap();
        // Without tables there is nothing to add to.
        assert!(write_rsdt(&mem).is_err());
        write(&mem, 2).unwrap();
        let tables = AcpiTables::new(2, layout::pci_mmio64_base_above((2 << 20) - 1)).unwrap();
        let rsdt_at = write_rsdt(&mem).unwrap();

        let mut rsdp = [0u8; RSDP_LEN];
        mem.read_slice(&mut rsdp, GuestAddress(tables.rsdp_address()))
            .unwrap();
        assert_eq!(u64::from(u32_at(&rsdp, 16)), rsdt_at);
        assert_eq!(u64_at(&rsdp, 24), tables.xsdt_address());
        assert_eq!(checksum(&rsdp[..20]), 0);
        assert_eq!(checksum(&rsdp), 0);

        let mut rsdt = [0u8; SDT_HEADER_LEN + 8];
        mem.read_slice(&mut rsdt, GuestAddress(rsdt_at)).unwrap();
        assert_valid_sdt(&rsdt, b"RSDT", 1);
        assert_eq!(
            u64::from(u32_at(&rsdt, SDT_HEADER_LEN)),
            tables.fadt_address()
        );
        assert_eq!(
            u64::from(u32_at(&rsdt, SDT_HEADER_LEN + 4)),
            tables.madt_address()
        );
    }

    /// The 64-bit aperture base of a guest small enough to have no high RAM —
    /// 4 GiB, which is what `pci_mmio64_base` returns for every guest below the
    /// 32-bit hole and what these tables are built for.
    const TEST_MMIO64_BASE: u64 = layout::TOP_OF_32BIT;

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }
    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }
    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// Header shape shared by every described table: signature, a length that
    /// matches reality, and a checksum that sums to zero.
    fn assert_valid_sdt(table: &[u8], signature: &[u8; 4], revision: u8) {
        assert_eq!(&table[..4], signature);
        assert_eq!(
            u32_at(table, 4) as usize,
            table.len(),
            "{} length field disagrees with the table",
            String::from_utf8_lossy(signature)
        );
        assert_eq!(table[8], revision);
        assert_eq!(
            checksum(table),
            0,
            "{} checksum must sum to zero",
            String::from_utf8_lossy(signature)
        );
        assert_eq!(&table[10..16], OEM_ID);
    }

    #[test]
    fn rsdp_is_valid_and_points_at_the_xsdt() {
        let tables = AcpiTables::new(2, TEST_MMIO64_BASE).unwrap();
        let rsdp = tables.table_at(tables.rsdp_address(), RSDP_LEN).unwrap();
        assert_eq!(&rsdp[..8], b"RSD PTR ");
        assert_eq!(rsdp[15], 2, "revision 2 = XSDT-based");
        assert_eq!(u32_at(rsdp, 20) as usize, RSDP_LEN);
        assert_eq!(u32_at(rsdp, 16), 0, "no RSDT");
        assert_eq!(u64_at(rsdp, 24), tables.xsdt_address());
        assert_ne!(u64_at(rsdp, 24), 0, "EDK2 gives up on a zero XsdtAddress");
        assert_eq!(checksum(&rsdp[..20]), 0, "ACPI 1.0 checksum");
        assert_eq!(checksum(rsdp), 0, "extended checksum");
        // The legacy 0xe0000..0xfffff scan must also be able to find it.
        assert_eq!(
            tables.rsdp_address() % 16,
            0,
            "RSDP must be 16-byte aligned"
        );
        assert!((0x000e_0000..0x0010_0000).contains(&tables.rsdp_address()));
    }

    #[test]
    fn xsdt_lists_the_fadt_and_madt_only() {
        let tables = AcpiTables::new(4, TEST_MMIO64_BASE).unwrap();
        let xsdt = tables
            .table_at(tables.xsdt_address(), SDT_HEADER_LEN + 16)
            .unwrap();
        assert_valid_sdt(xsdt, b"XSDT", 1);
        assert_eq!(u64_at(xsdt, SDT_HEADER_LEN), tables.fadt_address());
        assert_eq!(u64_at(xsdt, SDT_HEADER_LEN + 8), tables.madt_address());
        // The DSDT and FACS must NOT be here: EDK2's InstallCloudHvTables
        // installs every XSDT entry as a table and then the DSDT separately.
        for entry in [tables.dsdt_address(), tables.facs_address()] {
            assert!(
                !(0..2).any(|i| u64_at(xsdt, SDT_HEADER_LEN + i * 8) == entry),
                "{entry:#x} must not be in the XSDT"
            );
        }
    }

    #[test]
    fn fadt_describes_the_pm_block_and_the_dsdt() {
        let tables = AcpiTables::new(1, TEST_MMIO64_BASE).unwrap();
        let fadt = tables.table_at(tables.fadt_address(), FADT_LEN).unwrap();
        assert_valid_sdt(fadt, b"FACP", 6);
        assert_eq!(fadt.len(), 276);

        assert_eq!(u32_at(fadt, 36) as u64, tables.facs_address());
        assert_eq!(u32_at(fadt, 40) as u64, tables.dsdt_address());
        assert_eq!(u64_at(fadt, 132), tables.facs_address());
        assert_eq!(
            u64_at(fadt, 140),
            tables.dsdt_address(),
            "X_DSDT is how EDK2 finds the DSDT at all"
        );
        assert_ne!(u64_at(fadt, 140), 0);

        assert_eq!(u16_at(fadt, 46) as u32, layout::ACPI_SCI_GSI);
        assert_eq!(u32_at(fadt, 48), 0, "no SMI command port");
        assert_eq!(u32_at(fadt, 56), u32::from(pm::PM1A_EVT_PORT));
        assert_eq!(u32_at(fadt, 64), u32::from(pm::PM1A_CNT_PORT));
        assert_eq!(u32_at(fadt, 76), u32::from(pm::PM_TIMER_PORT));
        assert_eq!(u32_at(fadt, 80), u32::from(pm::GPE0_PORT));
        assert_eq!(fadt[88], 4, "PM1_EVT_LEN");
        assert_eq!(fadt[89], 2, "PM1_CNT_LEN");
        assert_eq!(fadt[91], 4, "PM_TMR_LEN");
        assert_eq!(fadt[92], 4, "GPE0_BLK_LEN");
        assert_eq!(&fadt[268..276], HYPERVISOR_ID);

        // IAPC_BOOT_ARCH: no VGA, but MSI must NOT be vetoed — a set bit 3 makes
        // Linux disable MSI for the whole machine, which would silently break
        // the first virtio-pci device with MSI-X.
        assert_eq!(u16_at(fadt, 109) & (1 << 2), 1 << 2, "VGA_NOT_PRESENT");
        assert_eq!(
            u16_at(fadt, 109) & (1 << 3),
            0,
            "MSI_NOT_SUPPORTED must stay clear (EPIC 19 needs MSI-X)"
        );
        assert_eq!(
            u16_at(fadt, 109) & (1 << 5),
            0,
            "the UEFI machine has a CMOS RTC, so CMOS_RTC_NOT_PRESENT is wrong"
        );
        assert_eq!(fadt[108], crate::rtc::REG_CENTURY, "CENTURY index");

        // We advertise a 24-bit PM timer, which is what platform.rs implements.
        assert_eq!(u32_at(fadt, 112) & (1 << 8), 0, "TMR_VAL_EXT must be clear");
        // Not hardware-reduced: Linux must take the PM1a_CNT path.
        assert_eq!(u32_at(fadt, 112) & (1 << 20), 0, "HW_REDUCED_ACPI clear");
        assert_eq!(
            u32_at(fadt, 112) & FADT_PWR_BUTTON,
            0,
            "the fixed power button is present (PWR_BUTTON clear)"
        );
    }

    /// Every X_ block must agree with its 32-bit twin and with the *_LEN field,
    /// or ACPICA prints "32/64X address mismatch in FADT" and overrides one.
    #[test]
    fn fadt_extended_blocks_agree_with_the_legacy_ones() {
        let tables = AcpiTables::new(1, TEST_MMIO64_BASE).unwrap();
        let fadt = tables.table_at(tables.fadt_address(), FADT_LEN).unwrap();
        for (legacy, extended, len_at) in [
            (56usize, 148usize, 88usize),
            (64, 172, 89),
            (76, 208, 91),
            (80, 220, 92),
        ] {
            let gas = &fadt[extended..extended + 12];
            assert_eq!(gas[0], 1, "SystemIO address space");
            assert_eq!(
                u64_at(gas, 4),
                u64::from(u32_at(fadt, legacy)),
                "X_ block at {extended} must name the same port"
            );
            assert_eq!(
                gas[1],
                fadt[len_at] * 8,
                "X_ block at {extended} must be {} bits wide",
                fadt[len_at] * 8
            );
        }
        // The ACPI 5.0 sleep registers EDK2 uses.
        assert_eq!(u64_at(fadt, 244 + 4), u64::from(pm::SLEEP_CONTROL_PORT));
        assert_eq!(fadt[244 + 1], 8);
        assert_eq!(u64_at(fadt, 256 + 4), u64::from(pm::SLEEP_STATUS_PORT));
    }

    #[test]
    fn facs_is_a_64_byte_aligned_signed_structure() {
        let tables = AcpiTables::new(1, TEST_MMIO64_BASE).unwrap();
        assert_eq!(tables.facs_address() % 64, 0, "ACPI 6.5 §5.2.10");
        let facs = tables.table_at(tables.facs_address(), FACS_LEN).unwrap();
        assert_eq!(&facs[..4], b"FACS");
        assert_eq!(u32_at(facs, 4) as usize, FACS_LEN);
        assert_eq!(u32_at(facs, 12), 0, "no firmware waking vector");
    }

    #[test]
    fn madt_matches_the_mp_table_topology() {
        for cpus in [1u32, 2, 4, 16] {
            let tables = AcpiTables::new(cpus, TEST_MMIO64_BASE).unwrap();
            let len = u32_at(
                tables
                    .table_at(tables.madt_address(), SDT_HEADER_LEN)
                    .unwrap(),
                4,
            ) as usize;
            let madt = tables.table_at(tables.madt_address(), len).unwrap();
            assert_valid_sdt(madt, b"APIC", 5);
            assert_eq!(u32_at(madt, 36), layout::LAPIC_ADDR);
            assert_eq!(u32_at(madt, 40), MADT_PCAT_COMPAT);

            let mut lapics = Vec::new();
            let mut nmis = 0;
            let mut ioapics = Vec::new();
            let mut overrides = Vec::new();
            let mut at = 44;
            while at < madt.len() {
                let kind = madt[at];
                let entry_len = usize::from(madt[at + 1]);
                assert!(
                    entry_len >= 2 && at + entry_len <= madt.len(),
                    "runaway entry"
                );
                match kind {
                    MADT_LAPIC => {
                        assert_eq!(entry_len, 8);
                        lapics.push((madt[at + 2], madt[at + 3], u32_at(madt, at + 4)));
                    }
                    MADT_IOAPIC => {
                        assert_eq!(entry_len, 12);
                        ioapics.push((madt[at + 2], u32_at(madt, at + 4), u32_at(madt, at + 8)));
                    }
                    MADT_INTERRUPT_SOURCE_OVERRIDE => {
                        assert_eq!(entry_len, 10);
                        overrides.push((
                            madt[at + 2],
                            madt[at + 3],
                            u32_at(madt, at + 4),
                            u16_at(madt, at + 8),
                        ));
                    }
                    MADT_LAPIC_NMI => {
                        assert_eq!(entry_len, 6);
                        assert_eq!(madt[at + 5], 1, "NMI belongs on LINT1");
                        nmis += 1;
                    }
                    other => panic!("unexpected MADT entry type {other}"),
                }
                at += entry_len;
            }

            // One enabled LAPIC per vCPU, uid == apic id == index (the same
            // identity mptable.rs and the CPUID APIC-id fix use).
            assert_eq!(lapics.len() as u32, cpus);
            for (index, (uid, apic_id, flags)) in lapics.iter().enumerate() {
                assert_eq!(u32::from(*uid), index as u32);
                assert_eq!(u32::from(*apic_id), index as u32);
                assert_eq!(*flags & 1, 1, "CPU {index} must be enabled");
            }
            assert_eq!(nmis as u32, cpus);

            // One IOAPIC, id above every LAPIC id, at the architectural address,
            // GSI base 0 so GSI n is pin n.
            assert_eq!(ioapics.len(), 1);
            assert_eq!(ioapics[0], (cpus as u8, layout::IOAPIC_ADDR, 0));

            // ISA IRQ 0 on GSI 2, bus-conformant flags — exactly what the MP
            // table publishes for the timer.
            assert!(overrides.contains(&(ISA_BUS, 0, 2, 0)));
            // The SCI, level/high, on a GSI no virtio slot can claim.
            assert!(overrides.contains(&(
                ISA_BUS,
                layout::ACPI_SCI_GSI as u8,
                layout::ACPI_SCI_GSI,
                MADT_INTI_ACTIVE_HIGH_LEVEL
            )));
            const {
                let mut n = 0;
                while n < layout::VIRTIO_IRQS.len() {
                    assert!(
                        layout::VIRTIO_IRQS[n] != layout::ACPI_SCI_GSI,
                        "the SCI must not share a GSI with a virtio slot"
                    );
                    n += 1;
                }
            };
        }
    }

    #[test]
    fn dsdt_carries_a_real_s5_package_and_a_pci_root_bridge() {
        let tables = AcpiTables::new(2, TEST_MMIO64_BASE).unwrap();
        let len = u32_at(
            tables
                .table_at(tables.dsdt_address(), SDT_HEADER_LEN)
                .unwrap(),
            4,
        ) as usize;
        let dsdt = tables.table_at(tables.dsdt_address(), len).unwrap();
        assert_valid_sdt(dsdt, b"DSDT", 2);
        let aml_body = &dsdt[SDT_HEADER_LEN..];

        // \_S5 as ACPICA looks for it, with SLP_TYP 5 first.
        let s5 = b"\\_S5_".as_slice();
        let at = find(aml_body, s5).expect("no \\_S5 in the DSDT");
        assert_eq!(aml_body[at - 1], 0x08, "_S5 must be a NameOp");
        assert_eq!(aml_body[at + 5], 0x12, "PackageOp");
        assert_eq!(aml_body[at + 7], 4, "the S5 package has four elements");
        assert_eq!(&aml_body[at + 8..at + 10], &[0x0a, pm::SLP_TYP_S5]);

        // \_SB.PCI0 with a PCI root bridge _HID and a _CRS.
        assert!(find(aml_body, b"\\_SB_").is_some());
        assert!(find(aml_body, b"PCI0").is_some());
        assert!(find(aml_body, b"_HID").is_some());
        let hid = find(aml_body, b"_HID").unwrap();
        assert_eq!(
            &aml_body[hid + 4..hid + 9],
            &[0x0c, 0x41, 0xd0, 0x0a, 0x03],
            "PCI0._HID must be EisaId(\"PNP0A03\")"
        );
        assert!(find(aml_body, b"_CRS").is_some());
        // The _CRS must publish the config ports and the MMIO aperture.
        assert!(find(aml_body, &layout::PCI_MMIO_HOLE_BASE.to_le_bytes()[..4]).is_some());

        // One ACPI0007 processor device per vCPU, named C000, C001, …
        assert_eq!(count(aml_body, b"ACPI0007"), 2);
        assert!(find(aml_body, b"C000").is_some());
        assert!(find(aml_body, b"C001").is_some());
        assert!(find(aml_body, b"C002").is_none());
    }

    /// The PCI window in the DSDT must not cover the virtio-mmio slots, or
    /// Linux refuses the platform devices' memory regions.
    #[test]
    fn dsdt_pci_window_stops_below_the_virtio_slots() {
        let tables = AcpiTables::new(1, TEST_MMIO64_BASE).unwrap();
        let len = u32_at(
            tables
                .table_at(tables.dsdt_address(), SDT_HEADER_LEN)
                .unwrap(),
            4,
        ) as usize;
        let dsdt = tables.table_at(tables.dsdt_address(), len).unwrap();
        // Every DWordMemory descriptor (tag 0x87, payload length 0x17): the
        // legacy VGA range first, then the 32-bit hole.
        let tag = [0x87, 0x17, 0x00];
        let windows: Vec<(u32, u32)> = dsdt
            .windows(tag.len())
            .enumerate()
            .filter(|(_, w)| *w == tag)
            .map(|(at, _)| (u32_at(dsdt, at + 10), u32_at(dsdt, at + 14)))
            .collect();
        assert_eq!(windows.first(), Some(&(0xa_0000, 0xb_ffff)), "{windows:x?}");
        let &(min, max) = windows
            .iter()
            .find(|(min, _)| u64::from(*min) == layout::PCI_MMIO_HOLE_BASE)
            .expect("no 32-bit PCI hole in _CRS");
        assert!(
            u64::from(max) < layout::VIRTIO_MMIO_BASE,
            "PCI window {min:#x}..{max:#x} overlaps the virtio-mmio window"
        );
    }

    /// The 64-bit aperture the shared-memory BAR lives in has to be in the
    /// same `_CRS`, above RAM, and prefetchable — the three things that decide
    /// whether the guest claims BAR 2 or throws it away (EPIC 20, VEN-2001).
    #[test]
    fn dsdt_publishes_the_prefetchable_64_bit_aperture() {
        // A 4096 MiB guest, whose aperture the firmware puts at 0x1_4000_0000.
        let base = layout::pci_mmio64_base(4096 << 20);
        assert_eq!(base, 0x1_4000_0000);
        let tables = AcpiTables::new(1, base).unwrap();
        let len = u32_at(
            tables
                .table_at(tables.dsdt_address(), SDT_HEADER_LEN)
                .unwrap(),
            4,
        ) as usize;
        let dsdt = tables.table_at(tables.dsdt_address(), len).unwrap();
        // QWordMemory: tag 0x8a, payload length 0x2b, resource type 0 (memory).
        let at = find(dsdt, &[0x8a, 0x2b, 0x00, 0x00]).expect("no QWordMemory in _CRS");
        assert_eq!(dsdt[at + 5] & 0x01, 0x01, "the window must be writable");
        assert_eq!(
            (dsdt[at + 5] >> 1) & 0x03,
            3,
            "the window must be prefetchable, or Linux will not claim a \
             prefetchable BAR inside it"
        );
        let min = u64_at(dsdt, at + 14);
        let max = u64_at(dsdt, at + 22);
        let length = u64_at(dsdt, at + 38);
        assert_eq!(min, base);
        assert_eq!(length, layout::PCI_MMIO64_SIZE);
        assert_eq!(max, base + length - 1);
        assert!(
            min >= layout::TOP_OF_32BIT,
            "the aperture must be above 4 GiB"
        );
        assert!(
            min > u64::from(layout::PCI_MMIO_HOLE_BASE as u32) + layout::PCI_MMIO_HOLE_SIZE,
            "the two apertures must not overlap"
        );
    }

    #[test]
    fn tables_fit_the_reserved_region_even_at_the_cpu_limit() {
        let tables = AcpiTables::new(MAX_ACPI_CPUS, TEST_MMIO64_BASE).unwrap();
        assert!(
            tables.blob().len() as u64 <= layout::ACPI_TABLES_SIZE,
            "{} bytes for {MAX_ACPI_CPUS} CPUs",
            tables.blob().len()
        );
        // …and the region must not run into the MP table.
        assert!(
            layout::ACPI_TABLES_START + tables.blob().len() as u64 <= layout::MPTABLE_START,
            "the ACPI blob overlaps the MP table"
        );
    }

    #[test]
    fn rejects_zero_and_too_many_cpus_and_a_region_that_is_too_small() {
        assert!(matches!(
            AcpiTables::new(0, TEST_MMIO64_BASE),
            Err(AcpiError::TooManyCpus(0))
        ));
        assert!(matches!(
            AcpiTables::new(255, TEST_MMIO64_BASE),
            Err(AcpiError::TooManyCpus(255))
        ));
        assert!(matches!(
            AcpiTables::at(layout::ACPI_TABLES_START, 0x100, 2, TEST_MMIO64_BASE),
            Err(AcpiError::TooLarge { .. })
        ));
    }

    /// Tables must be individually mappable, so none may straddle another and
    /// all must be aligned.
    #[test]
    fn tables_are_aligned_and_ordered_without_overlap() {
        let tables = AcpiTables::new(8, TEST_MMIO64_BASE).unwrap();
        let addresses = [
            tables.rsdp_address(),
            tables.xsdt_address(),
            tables.fadt_address(),
            tables.facs_address(),
            tables.madt_address(),
            tables.dsdt_address(),
        ];
        for pair in addresses.windows(2) {
            assert!(pair[0] < pair[1], "tables must be laid out in order");
        }
        for address in addresses.iter().skip(1) {
            assert_eq!(address % TABLE_ALIGN, 0, "{address:#x} is misaligned");
        }
        assert_eq!(tables.base_address(), layout::ACPI_TABLES_START);
        assert_eq!(tables.rsdp_address(), layout::ACPI_RSDP_START);
    }

    #[test]
    fn cpu_names_are_one_name_segment() {
        for cpu in [0u32, 1, 15, 16, 253] {
            let name = cpu_name(cpu);
            assert_eq!(name.len(), 4, "{name} is not a NameSeg");
            assert!(name.starts_with('C'));
            assert!(name[1..].chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert_eq!(cpu_name(0), "C000");
        assert_eq!(cpu_name(253), "C0FD");
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }
}
