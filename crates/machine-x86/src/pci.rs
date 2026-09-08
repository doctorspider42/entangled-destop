//! A minimal PCI root bus: legacy configuration mechanism #1 over a tree of
//! type-0 configuration headers (EPIC 19, the virtio-pci transport).
//!
//! This is the real bus the ADR-0003 gap map asked for. Its predecessor,
//! [`crate::platform::PciConfigSpace`], answers exactly one question — "what is
//! the host bridge device ID?" — so an EDK2 CloudHv firmware gets past its
//! `ASSERT` in SEC. It has no devices, no BARs and drops every write. This
//! module keeps that host bridge identity (byte for byte: the firmware still
//! switches on it) and adds everything a driver needs to find and program a real
//! device behind it.
//!
//! # Why mechanism #1 and nothing else
//!
//! Configuration access goes through the two legacy I/O ports — `0xcf8`
//! `CONFIG_ADDRESS`, `0xcfc` `CONFIG_DATA` — and *only* those. There is no
//! ECAM/MMCONFIG window: publishing one needs an ACPI MCFG table, and
//! `crate::acpi` deliberately publishes RSDP/XSDT/FADT/FACS/MADT/DSDT and no
//! more. Both consumers are fine with that:
//!
//! * Linux's `pci_legacy_init` probes conf1 by writing `0x8000_0000` to `0xcf8`
//!   and requiring it to read back unchanged, then runs `pci_sanity_check`,
//!   which wants some device on bus 0 to be either a host bridge by class or an
//!   Intel/Compaq vendor id. The host bridge here is both.
//! * EDK2's `PciHostBridgeDxe`/`PciBusDxe` on the CloudHv platform use
//!   `PciCf8Lib`, i.e. the same two ports.
//!
//! # What is modelled
//!
//! One bus (bus 0), function 0 of each device, header type 0. Per device: the
//! identity dwords, a command register whose I/O- and memory-space-enable bits
//! actually gate decoding, a status register with the "capabilities list"
//! bit set, up to six BARs implementing the sizing protocol, `interrupt_pin` /
//! `interrupt_line`, and a capability list built from opaque records the caller
//! supplies (virtio's four structure locators, in practice).
//!
//! Not modelled, and not needed by either consumer: multi-function devices,
//! PCI-to-PCI bridges (header type 1), 64-bit and I/O BARs, expansion ROMs,
//! power management, and `RW1C` status bits.
//!
//! # Untrusted guest
//!
//! A configuration access is four guest-controlled bytes at `0xcf8` plus a port
//! offset. Everything derived from them is bounds-checked here:
//!
//! * an address with the enable bit clear, or naming any bus/device/function
//!   that does not exist, reads back all-ones ("no device") and swallows writes;
//! * writes go through a per-dword write mask, so a guest can never change an
//!   identity register, a class code or the header type. A capability record is
//!   read-only unless its owner asked for specific bits to be writable
//!   ([`ConfigSpace::add_capability_writable`] — MSI-X's enable and function-mask
//!   bits, and nothing else in the same halfword);
//! * BAR writes are masked to the BAR's own size, which *is* the sizing
//!   protocol and keeps every window naturally aligned. A guest may move a BAR
//!   anywhere inside the aperture the DSDT advertises — every UEFI firmware
//!   re-allocates resources, so it must be able to — but
//!   [`PciRoot::locate_mmio`] refuses to decode anything *outside* that
//!   aperture, so a BAR parked over guest RAM, the LAPIC/IOAPIC or the
//!   virtio-mmio window shadows nothing and simply receives nothing;
//! * this module never allocates on a guest access.
//!
//! It is portable on purpose (ADR-0002): no KVM, no eventfds, no virtio types,
//! so the whole config-space model is unit-testable on any host. The Linux-only
//! half — irqfds, ioeventfds and the transports themselves — lives in
//! [`crate::virtio_pci`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use thiserror::Error;

use crate::layout;
use crate::platform::{CLOUDHV_HOST_BRIDGE_DEVICE_ID, HOST_BRIDGE_VENDOR_ID};

/// `CONFIG_ADDRESS`, the 32-bit BDF+register selector.
pub const CONFIG_ADDRESS_PORT: u16 = 0x0cf8;

/// `CONFIG_DATA`, the 32-bit data window. Byte and word accesses use the low
/// two bits of the port number as an offset inside the selected dword.
pub const CONFIG_DATA_PORT: u16 = 0x0cfc;

/// Bit 31 of `CONFIG_ADDRESS`: enables the mechanism.
const CONFIG_ADDRESS_ENABLE: u32 = 0x8000_0000;

/// What the CPU sees when nothing decodes a configuration access.
const NO_DEVICE: u32 = 0xffff_ffff;

/// Devices the root bus holds, host bridge included.
///
/// Bounded so the number of config spaces, BAR windows and IOAPIC pins a VM can
/// demand stays fixed: [`layout::PCI_MMIO_SLOTS`] aperture slots for the
/// devices, plus the bridge, which has no BARs.
pub const MAX_PCI_DEVICES: usize = layout::PCI_MMIO_SLOTS as usize + 1;

/// Device number of the host bridge, which is always present.
pub const HOST_BRIDGE_DEVICE: u8 = 0;

/// Class code for a host bridge: base class 0x06 (bridge) in bits 31:24, sub
/// class 0x00 (host bridge) in bits 23:16. Linux's `pci_sanity_check` accepts a
/// bus on the strength of this value.
const CLASS_HOST_BRIDGE: u32 = 0x0600_0000;

// ---- type-0 configuration header ------------------------------------------

/// Byte offsets of the type-0 header registers this module implements.
pub mod reg {
    /// Vendor id (low half) and device id (high half).
    pub const ID: u8 = 0x00;
    /// Command (low half) and status (high half).
    pub const COMMAND: u8 = 0x04;
    /// Revision id, programming interface, subclass, base class.
    pub const CLASS_REVISION: u8 = 0x08;
    /// Cache line size, latency timer, header type, BIST.
    pub const HEADER_TYPE: u8 = 0x0c;
    /// First of the six base address registers.
    pub const BAR0: u8 = 0x10;
    /// Subsystem vendor id (low half) and subsystem id (high half).
    pub const SUBSYSTEM: u8 = 0x2c;
    /// Pointer to the first capability record.
    pub const CAP_POINTER: u8 = 0x34;
    /// Interrupt line (byte 0) and interrupt pin (byte 1).
    pub const INTERRUPT: u8 = 0x3c;

    /// Where capability records start. Everything from here to the end of the
    /// 256-byte header is capability space.
    pub const FIRST_CAPABILITY: u8 = 0x40;

    /// Size of the configuration header, in bytes and in dwords.
    pub const SIZE: usize = 0x100;
    pub const DWORDS: usize = SIZE / 4;
}

/// Command-register bits.
pub mod command {
    /// Respond to I/O space accesses.
    pub const IO_SPACE: u16 = 1 << 0;
    /// Respond to memory space accesses — this is what gates BAR decoding.
    pub const MEMORY_SPACE: u16 = 1 << 1;
    /// Act as a bus master (meaningless here; the device DMAs through the host).
    pub const BUS_MASTER: u16 = 1 << 2;
    /// Do **not** assert the legacy interrupt line.
    pub const INTX_DISABLE: u16 = 1 << 10;

    /// Bits a guest may change. Everything else in the dword — the whole status
    /// half included — is read-only here.
    pub const WRITABLE: u32 = (IO_SPACE | MEMORY_SPACE | BUS_MASTER | INTX_DISABLE) as u32;
}

/// Status-register bit 4: the device has a capability list at
/// [`reg::CAP_POINTER`]. Without it a driver never looks for the virtio
/// structures.
const STATUS_CAP_LIST: u32 = 1 << 4;

/// BAR bit 0 clear = memory space; bits 2:1 = 00 = 32-bit; bit 3 = 0 = not
/// prefetchable. All four are what a 32-bit non-prefetchable memory BAR reads
/// back, and all four are read-only to the guest.
const BAR_MEMORY_32_FLAGS: u32 = 0;

/// Bits of a memory BAR that hold the flags rather than the address.
const BAR_FLAG_MASK: u32 = 0xf;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PciError {
    #[error("PCI bus is full: {MAX_PCI_DEVICES} devices (host bridge included) is the maximum")]
    BusFull,

    #[error("BAR index {index} does not exist (a type-0 header has six)")]
    NoSuchBar { index: u8 },

    #[error("BAR size {size:#x} must be a power of two of at least 16 bytes")]
    BadBarSize { size: u32 },

    #[error(
        "BAR base {base:#x} is not aligned to its size {size:#x}; a PCI memory \
         BAR decodes only naturally aligned windows"
    )]
    MisalignedBar { base: u32, size: u32 },

    #[error(
        "capability record of {len} bytes does not fit: {free} bytes left in the \
         configuration header"
    )]
    CapabilitySpaceExhausted { len: usize, free: usize },

    #[error("capability record must be at least 2 bytes (id and next pointer)")]
    CapabilityTooShort,
}

/// One 32-bit memory BAR. The address bits the guest may program are
/// `!(size - 1)`, kept in the register's write mask rather than duplicated here.
#[derive(Debug, Clone, Copy)]
struct MemoryBar {
    /// Window size in bytes; a power of two.
    size: u32,
}

/// One PCI function's configuration space: a type-0 header plus its capability
/// list.
///
/// Stored as 64 dwords with a parallel write mask, which is what makes
/// sub-dword accesses and capability-list walks fall out for free instead of
/// needing a field-by-field decoder.
pub struct ConfigSpace {
    regs: [u32; reg::DWORDS],
    write_mask: [u32; reg::DWORDS],
    bars: [Option<MemoryBar>; 6],
    /// Byte offset where the next capability record goes.
    next_capability: u8,
    /// Byte offset of the most recently added record, so its `cap_next` can be
    /// patched when another one follows.
    last_capability: Option<u8>,
    /// Mirrors "the guest has not set `INTX_DISABLE`", shared with whatever
    /// raises the interrupt line so it can honour the bit without reaching back
    /// into the config space.
    intx_enabled: Arc<AtomicBool>,
    /// Registers whose value is published to whatever sits behind this config
    /// space, refreshed after every guest write (see [`Self::mirror_dword`]).
    ///
    /// One entry per *distinct* register, so the list is bounded by
    /// [`reg::DWORDS`] and, in practice, holds one: the MSI-X message control.
    mirrors: Vec<(u8, Arc<AtomicU32>)>,
    /// The register file as the host finished building it, taken by
    /// [`Self::seal`] and restored by [`Self::reset`] (ADR-0005).
    ///
    /// A snapshot rather than a field-by-field reset because *every* guest-
    /// writable register has to go back: the command register (or the rebooted
    /// firmware finds memory decoding already on for a BAR it has not placed),
    /// the BAR addresses themselves, the MSI-X message control, the cache-line
    /// and latency scratch. Enumerating them by hand is how one gets forgotten.
    power_on: [u32; reg::DWORDS],
}

impl ConfigSpace {
    /// A type-0 header for a single-function device.
    pub fn type0(vendor_id: u16, device_id: u16, class_code: u32, revision: u8) -> Self {
        let mut space = Self {
            regs: [0; reg::DWORDS],
            write_mask: [0; reg::DWORDS],
            bars: [None; 6],
            next_capability: reg::FIRST_CAPABILITY,
            last_capability: None,
            intx_enabled: Arc::new(AtomicBool::new(true)),
            mirrors: Vec::new(),
            power_on: [0; reg::DWORDS],
        };
        space.set(reg::ID, u32::from(vendor_id) | (u32::from(device_id) << 16));
        // The class code occupies bits 31:8 and the revision bits 7:0.
        space.set(
            reg::CLASS_REVISION,
            (class_code & 0xffff_ff00) | u32::from(revision),
        );
        // Header type 0, single function, no BIST.
        space.set(reg::HEADER_TYPE, 0);
        // The command register starts at 0: a freshly reset PCI device decodes
        // nothing until its driver enables it, and our BAR dispatch honours that.
        space.set_mask(reg::COMMAND, command::WRITABLE);
        // Cache line size and latency timer are writable scratch; the header
        // type and BIST bytes above them are not.
        space.set_mask(reg::HEADER_TYPE, 0x0000_ffff);
        space
    }

    /// The host bridge at `00:00.0`.
    ///
    /// Identity unchanged from [`crate::platform::PciConfigSpace`]: an EDK2
    /// CloudHv firmware reads the device id in SEC and `ASSERT (FALSE)`s on
    /// anything it does not recognise, and Linux's `pci_sanity_check` accepts
    /// bus 0 because of this device's class *and* its Intel vendor id.
    pub fn host_bridge() -> Self {
        Self::type0(
            HOST_BRIDGE_VENDOR_ID,
            CLOUDHV_HOST_BRIDGE_DEVICE_ID,
            CLASS_HOST_BRIDGE,
            0,
        )
    }

    /// Sets the subsystem vendor and device ids (read-only to the guest).
    ///
    /// Not cosmetic for virtio: Linux's `vp_modern_probe` takes the virtio
    /// *vendor* id from the PCI subsystem vendor id.
    pub fn with_subsystem(mut self, vendor_id: u16, device_id: u16) -> Self {
        self.set(
            reg::SUBSYSTEM,
            u32::from(vendor_id) | (u32::from(device_id) << 16),
        );
        self
    }

    /// Programs BAR `index` as a 32-bit non-prefetchable memory window of
    /// `size` bytes at `base`, which is what the host has reserved for it.
    ///
    /// `size` must be a power of two (the sizing protocol cannot express
    /// anything else) and `base` must be aligned to it (PCI decodes only
    /// naturally aligned windows). Both are host programming errors, checked
    /// here rather than producing a device that decodes the wrong addresses.
    pub fn with_memory_bar(mut self, index: u8, base: u32, size: u32) -> Result<Self, PciError> {
        let slot = self
            .bars
            .get_mut(usize::from(index))
            .ok_or(PciError::NoSuchBar { index })?;
        if size < 16 || !size.is_power_of_two() {
            return Err(PciError::BadBarSize { size });
        }
        if base % size != 0 {
            return Err(PciError::MisalignedBar { base, size });
        }
        let address_mask = !(size - 1);
        *slot = Some(MemoryBar { size });
        let register = reg::BAR0 + index * 4;
        self.set(register, (base & address_mask) | BAR_MEMORY_32_FLAGS);
        // The guest owns the address bits; the flag bits are ours. A write of
        // all-ones therefore reads back as `address_mask | flags`, which *is*
        // the sizing protocol.
        self.set_mask(register, address_mask);
        Ok(self)
    }

    /// Sets `interrupt_pin` (nonzero means "this device has a legacy interrupt")
    /// and `interrupt_line`, the GSI the host has wired it to. **Both are
    /// read-only to the guest.**
    ///
    /// On real hardware `interrupt_line` is writable, because a real BIOS routes
    /// `INTA#` through a PIRQ router and then records where it landed. This
    /// machine has no router: each device's line is a fixed IOAPIC pin chosen by
    /// [`crate::virtio_pci`] and injected through a KVM irqfd, so the register
    /// describes wiring nothing in the guest can change. Letting a guest write it
    /// only lets the guest lie to itself.
    ///
    /// That is not hypothetical. EDK2's `PciBusDxe` clobbers the register twice
    /// during enumeration — `PciDeviceSupport.c` writes `PCI_INT_LINE_UNKNOWN`
    /// (`0xff`) and `PciEnumeratorSupport.c` writes `0` — on the assumption that
    /// a platform driver will program the real value afterwards. Ours cannot,
    /// because there is nothing to program. Linux then reads the clobbered value
    /// and, finding no `_PRT` under `\_SB.PCI0` either, gives up on the line:
    /// `0xff` means "not connected" (PCI 3.0 §6.2.4), so `acpi_pci_irq_enable()`
    /// sets `IRQ_NOTCONNECTED` and `vp_find_vqs_intx()`'s `request_irq` fails —
    /// every virtio device's probe ends in `VIRTIO_CONFIG_S_FAILED`, which is
    /// exactly what booting an Ubuntu ISO looked like before this became
    /// read-only. With the register preserved, Linux logs `PCI INT A: no GSI -
    /// using ISA IRQ 5` and proceeds, and the ISA pin it then requests is one the
    /// MP table and the MADT already route to the IOAPIC as an edge — which is
    /// the shape our irqfd injection actually is.
    ///
    /// A `_PRT` in the DSDT is the properly furnished answer and belongs in the
    /// same change as level-triggered `INTA#` support; see ADR-0003.
    pub fn with_interrupt(mut self, pin: u8, line: u8) -> Self {
        self.set(reg::INTERRUPT, u32::from(line) | (u32::from(pin) << 8));
        // Mask left at 0: the whole dword is read-only.
        self
    }

    /// Appends one read-only capability record and links it into the list.
    ///
    /// `record[0]` is the capability id and `record[1]` its `cap_next` byte,
    /// which this function owns: the caller leaves it at 0 and the list is built
    /// here, so a record's content stays the caller's business (for virtio: which
    /// BAR, which offset, which length) and the *list* stays the bus's.
    ///
    /// Returns the byte offset the record landed at, which is what the caller
    /// needs to [`mirror_dword`](Self::mirror_dword) a register inside it.
    pub fn add_capability(&mut self, record: &[u8]) -> Result<u8, PciError> {
        self.add_capability_writable(record, &[])
    }

    /// [`Self::add_capability`] with a per-dword write mask, for a capability
    /// whose registers the guest may change.
    ///
    /// `write_mask[n]` is the mask for the record's *n*-th dword; dwords beyond
    /// the end of the slice stay read-only. That is how MSI-X gets in: its
    /// message control register holds the enable and function-mask bits a driver
    /// writes, while the table size in the same halfword must stay read-only —
    /// a guest that could widen the table would be describing entries the host
    /// never allocated.
    ///
    /// The mask is applied to whole dwords of the *header*, so a record must be
    /// dword-aligned for it to line up. Every record placed here is: lengths are
    /// rounded up to a dword and the list starts at [`reg::FIRST_CAPABILITY`].
    pub fn add_capability_writable(
        &mut self,
        record: &[u8],
        write_mask: &[u32],
    ) -> Result<u8, PciError> {
        if record.len() < 2 {
            return Err(PciError::CapabilityTooShort);
        }
        let at = usize::from(self.next_capability);
        // Records are dword-aligned, which every real capability is.
        let len = (record.len() + 3) & !3;
        let free = reg::SIZE.saturating_sub(at);
        if len > free {
            return Err(PciError::CapabilitySpaceExhausted {
                len: record.len(),
                free,
            });
        }
        for (i, byte) in record.iter().enumerate() {
            self.write_masked_byte(at + i, *byte, true);
        }
        // The mask goes down *after* the bytes: `write_masked_byte(host)` writes
        // through any mask, but a later guest write must see the final one.
        for (dword, mask) in write_mask.iter().enumerate() {
            // `at + len <= reg::SIZE` (checked above) bounds this; `set_mask`
            // ignores an out-of-range register either way.
            let register = u8::try_from(at + dword * 4).unwrap_or(u8::MAX);
            self.set_mask(register, *mask);
        }
        // Link the previous record to this one, or publish the list head.
        match self.last_capability {
            Some(previous) => {
                self.write_masked_byte(usize::from(previous) + 1, self.next_capability, true)
            }
            None => {
                self.set(reg::CAP_POINTER, u32::from(self.next_capability));
                // Advertising a list the driver must not be told about would be
                // pointless: the status bit is what makes it look.
                let status = self.get(reg::COMMAND) | (STATUS_CAP_LIST << 16);
                self.set(reg::COMMAND, status);
            }
        }
        let placed = self.next_capability;
        self.last_capability = Some(placed);
        // `len <= free` and `at + free == reg::SIZE`, so this stays in range.
        self.next_capability = u8::try_from(at + len).unwrap_or(u8::MAX);
        Ok(placed)
    }

    /// Publishes the dword at `register` into `handle`, refreshed after every
    /// guest write to it. `handle` is seeded with the register's current value.
    ///
    /// The generic form of what [`Self::intx_flag`] does for one bit, and it
    /// exists for the same reason: a register the *guest* owns has to reach the
    /// host object whose behaviour it changes, without that object reaching back
    /// into a configuration space guarded by the bus lock. MSI-X's message
    /// control is the case in point — the transport must know, on every interrupt
    /// it delivers, whether the driver has MSI-X enabled or the function masked.
    ///
    /// The handle comes from the caller rather than from here so that the object
    /// whose behaviour the register controls owns it; and it is a bare
    /// `AtomicU32`, because this module publishes a dword and stays ignorant of
    /// what its bits mean (interpreting them is `virtio_core::msix`'s job).
    ///
    /// Mirroring the same register twice replaces the handle, so the list is
    /// bounded by the number of dwords in the header.
    pub fn mirror_dword(&mut self, register: u8, handle: Arc<AtomicU32>) {
        let register = register & 0xfc;
        handle.store(self.get(register), Ordering::Release);
        match self.mirrors.iter_mut().find(|(r, _)| *r == register) {
            Some(slot) => slot.1 = handle,
            None => self.mirrors.push((register, handle)),
        }
    }

    // ---------------------------------------------------------- power-on

    /// Records the current register file as this function's power-on state
    /// (ADR-0005). Called by [`PciRoot::attach`] once the host has finished
    /// building the config space, so nothing has to remember to.
    pub fn seal(&mut self) {
        self.power_on = self.regs;
    }

    /// Machine reset: the register file back to what [`Self::seal`] captured,
    /// and everything published from it re-published.
    ///
    /// The write *mask* and the capability list are untouched: they are the
    /// function's shape, decided by the host, not state a guest can move.
    pub fn reset(&mut self) {
        self.regs = self.power_on;
        self.intx_enabled.store(
            self.command() & command::INTX_DISABLE == 0,
            Ordering::Release,
        );
        for (register, handle) in &self.mirrors {
            handle.store(self.regs[Self::index(*register)], Ordering::Release);
        }
    }

    /// This function's register file, for a snapshot (ADR-0006).
    ///
    /// The whole file, not a list of the interesting registers — the same
    /// reasoning `seal`/`reset` already follow. Every guest-writable dword has
    /// to come back: the command register (or the restored guest finds memory
    /// decoding off for a BAR it placed hours ago), the BAR addresses, the
    /// MSI-X message control, the cache-line and latency scratch. Enumerating
    /// them by hand is how one gets forgotten.
    ///
    /// The write mask, the capability list and the BAR *sizes* are not in it:
    /// they are the function's shape, decided by the host that built it, and a
    /// snapshot that could change them would let a file redefine the hardware.
    pub fn save_state(&self) -> Vec<u32> {
        self.regs.to_vec()
    }

    /// Puts the register file back and re-publishes everything derived from it.
    ///
    /// The re-publishing is the part that is easy to miss and impossible to
    /// diagnose: the INTx-enable flag and the mirrored MSI-X control dword are
    /// held by *other* objects (the interrupt line, the MSI-X capability), and
    /// a config space restored without refreshing them is a function whose
    /// registers say one thing and whose interrupts do another.
    pub fn load_state(&mut self, regs: &[u32]) -> Result<(), crate::state::StateError> {
        if regs.len() != self.regs.len() {
            return Err(crate::state::StateError::Count {
                what: "PCI configuration dwords",
                snapshot: regs.len(),
                current: self.regs.len(),
            });
        }
        self.regs.copy_from_slice(regs);
        self.intx_enabled.store(
            self.command() & command::INTX_DISABLE == 0,
            Ordering::Release,
        );
        for (register, handle) in &self.mirrors {
            handle.store(self.regs[Self::index(*register)], Ordering::Release);
        }
        Ok(())
    }

    // -------------------------------------------------------------- state

    /// The command register.
    pub fn command(&self) -> u16 {
        self.get(reg::COMMAND) as u16
    }

    /// The status register.
    pub fn status(&self) -> u16 {
        (self.get(reg::COMMAND) >> 16) as u16
    }

    /// Whether the device currently decodes memory accesses. All BAR dispatch
    /// goes through this.
    pub fn memory_enabled(&self) -> bool {
        self.command() & command::MEMORY_SPACE != 0
    }

    /// Whether the device currently decodes I/O accesses. Nothing here has an
    /// I/O BAR, so it exists for completeness and for the tests that prove the
    /// two enable bits are independent.
    pub fn io_enabled(&self) -> bool {
        self.command() & command::IO_SPACE != 0
    }

    /// Shared "INTx is not disabled" flag, for the host object that raises the
    /// interrupt line.
    pub fn intx_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.intx_enabled)
    }

    /// Everything that decides which addresses this function claims: the six BAR
    /// registers and whether memory decoding is on.
    ///
    /// Compared before and after a guest write to tell "the guest moved a window"
    /// from "the guest scribbled on the latency timer". Cheap, `Copy`, and
    /// allocation-free, because it is computed twice per configuration write.
    fn decode_state(&self) -> ([u32; 6], bool) {
        let mut bars = [0u32; 6];
        for (index, slot) in bars.iter_mut().enumerate() {
            // `index < 6`, so `BAR0 + index * 4` stays inside the header.
            let register = reg::BAR0.saturating_add((index as u8).saturating_mul(4));
            *slot = self.get(register);
        }
        (bars, self.memory_enabled())
    }

    /// The window BAR `index` currently decodes, or `None` when the BAR does
    /// not exist, is unprogrammed, or memory decoding is off.
    pub fn bar_window(&self, index: u8) -> Option<(u64, u64)> {
        if !self.memory_enabled() {
            return None;
        }
        let bar = (*self.bars.get(usize::from(index))?)?;
        let base = self.get(reg::BAR0 + index * 4) & !BAR_FLAG_MASK;
        // An unprogrammed BAR (or one the guest parked at 0) decodes nothing:
        // address 0 is guest RAM, and claiming it would shadow real memory.
        (base != 0).then_some((u64::from(base), u64::from(bar.size)))
    }

    // ------------------------------------------------------------ accesses

    /// The dword at `register` (which is masked to a dword boundary).
    pub fn read_dword(&self, register: u8) -> u32 {
        self.get(register & 0xfc)
    }

    /// Writes `data` into the selected dword starting `byte_offset` bytes into
    /// it. Bytes that fall outside the dword, and bits the write mask does not
    /// allow, are dropped.
    ///
    /// Returns true when the write changed a [mirrored](Self::mirror_dword)
    /// register, so the caller can tell whoever holds that mirror to act on it.
    pub fn write_dword_bytes(&mut self, register: u8, byte_offset: usize, data: &[u8]) -> bool {
        let register = register & 0xfc;
        let base = usize::from(register);
        for (i, byte) in data.iter().enumerate() {
            let within = byte_offset + i;
            if within >= 4 {
                // Real hardware does not decode past the selected dword either.
                break;
            }
            self.write_masked_byte(base + within, *byte, false);
        }
        // A command-register write may have changed INTx enablement.
        if register == reg::COMMAND {
            self.intx_enabled.store(
                self.command() & command::INTX_DISABLE == 0,
                Ordering::Release,
            );
        }
        let value = self.get(register);
        let mut changed = false;
        for (mirrored, handle) in &self.mirrors {
            if *mirrored == register && handle.swap(value, Ordering::AcqRel) != value {
                changed = true;
            }
        }
        changed
    }

    // ------------------------------------------------------------- helpers

    fn index(register: u8) -> usize {
        usize::from(register) / 4
    }

    fn get(&self, register: u8) -> u32 {
        self.regs.get(Self::index(register)).copied().unwrap_or(0)
    }

    fn set(&mut self, register: u8, value: u32) {
        if let Some(slot) = self.regs.get_mut(Self::index(register)) {
            *slot = value;
        }
    }

    fn set_mask(&mut self, register: u8, mask: u32) {
        if let Some(slot) = self.write_mask.get_mut(Self::index(register)) {
            *slot = mask;
        }
    }

    /// Writes one byte of the header. `host` bypasses the write mask, which is
    /// how the host lays capability records down in otherwise read-only space;
    /// a guest write always goes through the mask.
    fn write_masked_byte(&mut self, at: usize, value: u8, host: bool) {
        let dword = at / 4;
        let shift = (at % 4) * 8;
        let (Some(current), Some(mask)) = (self.regs.get(dword), self.write_mask.get(dword)) else {
            return;
        };
        let byte_mask = if host { 0xff } else { (mask >> shift) & 0xff };
        let kept = current & !(byte_mask << shift);
        let new = (u32::from(value) & byte_mask) << shift;
        if let Some(slot) = self.regs.get_mut(dword) {
            *slot = kept | new;
        }
    }
}

// ---- the root bus ---------------------------------------------------------

/// What a guest configuration write changed, beyond the register itself.
///
/// The `owner` token is the one the caller passed to [`PciRoot::attach`], so the
/// host can find whatever sits behind that configuration space and act on it.
/// Both flags are things the host **must** act on:
///
/// * `decode_changed` — a BAR moved, changed size, or its memory-space enable
///   flipped, so anything wired to a fixed guest physical address inside that
///   window (a KVM ioeventfd above all) is now pointing at an address the device
///   no longer answers on;
/// * `mirror_changed` — a [mirrored](ConfigSpace::mirror_dword) register changed
///   value. For virtio that is the MSI-X message control: enabling MSI-X or
///   clearing the function mask makes every interrupt the PBA remembers
///   deliverable at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigWrite {
    /// The function's owner token, when it has one. `None` for the host bridge,
    /// which is a configuration space and nothing else.
    pub owner: Option<usize>,
    pub decode_changed: bool,
    pub mirror_changed: bool,
}

impl ConfigWrite {
    /// Whether anything at all behind the configuration space needs attention.
    pub fn needs_attention(&self) -> bool {
        self.owner.is_some() && (self.decode_changed || self.mirror_changed)
    }
}

/// A decoded `CONFIG_ADDRESS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigTarget {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    /// Dword-aligned register offset inside the 256-byte header.
    pub register: u8,
}

/// One function on the bus.
struct PciFunction {
    device: u8,
    config: ConfigSpace,
    /// Opaque token the caller uses to find whatever sits behind this config
    /// space (for virtio-pci: the index of the bus slot holding the transport).
    /// `None` for the host bridge, which is only a config space.
    owner: Option<usize>,
}

/// The PCI root bus: the latched configuration address plus every function's
/// configuration space, and the address decoding for both mechanisms the guest
/// uses — configuration accesses on `0xcf8`/`0xcfc` and MMIO accesses into the
/// devices' BAR windows.
///
/// Deliberately knows nothing about virtio, KVM or interrupts.
pub struct PciRoot {
    /// Latched `CONFIG_ADDRESS`. Fully guest-controlled; decoded on every use.
    address: u32,
    functions: Vec<PciFunction>,
}

impl Default for PciRoot {
    fn default() -> Self {
        Self::new()
    }
}

impl PciRoot {
    /// A bus with only the host bridge on it.
    pub fn new() -> Self {
        let mut config = ConfigSpace::host_bridge();
        config.seal();
        Self {
            address: 0,
            functions: vec![PciFunction {
                device: HOST_BRIDGE_DEVICE,
                config,
                owner: None,
            }],
        }
    }

    /// Machine reset (ADR-0005): the latched configuration address and every
    /// function's configuration space back to power-on.
    ///
    /// Only the bus's own state — the transports behind the functions are reset
    /// by whoever owns them (`VirtioPciBus::reset`), because a config space
    /// knows nothing about what sits behind it and this module keeps it that
    /// way.
    pub fn reset(&mut self) {
        self.address = 0;
        for function in &mut self.functions {
            function.config.reset();
        }
    }

    /// The bus, for a snapshot (ADR-0006): the latched configuration address
    /// and every function's register file, host bridge included.
    pub fn save_state(&self) -> crate::state::SavedPciRoot {
        crate::state::SavedPciRoot {
            address: self.address,
            functions: self
                .functions
                .iter()
                .map(|function| crate::state::SavedPciFunction {
                    device: function.device,
                    regs: function.config.save_state(),
                })
                .collect(),
        }
    }

    /// Puts it back.
    ///
    /// The function *list* is this machine's, built from its configuration; the
    /// snapshot only supplies the register values. A snapshot with a different
    /// number of functions, or with them at different device numbers, describes
    /// a different bus — and a guest restored onto one would find its
    /// `00:01.0` is now somebody else's device.
    pub fn load_state(
        &mut self,
        state: &crate::state::SavedPciRoot,
    ) -> Result<(), crate::state::StateError> {
        if state.functions.len() != self.functions.len() {
            return Err(crate::state::StateError::Count {
                what: "PCI functions",
                snapshot: state.functions.len(),
                current: self.functions.len(),
            });
        }
        for (function, saved) in self.functions.iter_mut().zip(&state.functions) {
            if function.device != saved.device {
                return Err(crate::state::StateError::BadValue {
                    what: "PCI device number",
                    value: u64::from(saved.device),
                });
            }
            function.config.load_state(&saved.regs)?;
        }
        self.address = state.address;
        Ok(())
    }

    /// True when `port` belongs to the legacy configuration mechanism.
    pub fn contains(port: u16) -> bool {
        (CONFIG_ADDRESS_PORT..CONFIG_ADDRESS_PORT + 8).contains(&port)
    }

    /// Adds `config` as the next device number, tagged with `owner`. Returns the
    /// device number it landed on.
    pub fn attach(&mut self, mut config: ConfigSpace, owner: usize) -> Result<u8, PciError> {
        if self.functions.len() >= MAX_PCI_DEVICES {
            return Err(PciError::BusFull);
        }
        // Whatever the host built is this function's power-on state; a reset
        // puts it back (ADR-0005).
        config.seal();
        // Device numbers are dense from the host bridge upwards, and
        // `MAX_PCI_DEVICES` is far below the 32 a bus allows.
        let device = u8::try_from(self.functions.len()).unwrap_or(u8::MAX);
        self.functions.push(PciFunction {
            device,
            config,
            owner: Some(owner),
        });
        Ok(device)
    }

    /// Number of functions on the bus, host bridge included.
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// The configuration space of the function tagged with `owner`.
    pub fn config_of(&self, owner: usize) -> Option<&ConfigSpace> {
        self.functions
            .iter()
            .find(|f| f.owner == Some(owner))
            .map(|f| &f.config)
    }

    /// Decodes the latched address, or `None` when the mechanism is disabled.
    fn decode(&self) -> Option<ConfigTarget> {
        if self.address & CONFIG_ADDRESS_ENABLE == 0 {
            return None;
        }
        Some(ConfigTarget {
            bus: ((self.address >> 16) & 0xff) as u8,
            device: ((self.address >> 11) & 0x1f) as u8,
            function: ((self.address >> 8) & 0x07) as u8,
            // Bits 1:0 are always zero: accesses are dword-aligned.
            register: (self.address & 0xfc) as u8,
        })
    }

    /// The function a decoded target names, if it exists. Only bus 0 and
    /// function 0 exist here.
    fn function(&self, target: &ConfigTarget) -> Option<&PciFunction> {
        if target.bus != 0 || target.function != 0 {
            return None;
        }
        self.functions.iter().find(|f| f.device == target.device)
    }

    fn function_mut(&mut self, target: &ConfigTarget) -> Option<&mut PciFunction> {
        if target.bus != 0 || target.function != 0 {
            return None;
        }
        self.functions
            .iter_mut()
            .find(|f| f.device == target.device)
    }

    /// Guest read from a configuration port. `data` may be 1, 2 or 4 bytes.
    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        let offset = usize::from(port.wrapping_sub(CONFIG_ADDRESS_PORT));
        let value = if offset < 4 {
            self.address
        } else {
            match self.decode() {
                Some(target) => self
                    .function(&target)
                    .map_or(NO_DEVICE, |f| f.config.read_dword(target.register)),
                None => NO_DEVICE,
            }
        };
        // A byte/word access reads that slice of the selected dword; the port
        // offset within the 4-byte window selects which. Anything past the end
        // of the dword floats high, exactly like an unclaimed port.
        let within = offset & 0x3;
        let bytes = value.to_le_bytes();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = bytes.get(within + i).copied().unwrap_or(0xff);
        }
    }

    /// Guest write to a configuration port.
    ///
    /// Returns a [`ConfigWrite`] describing what else the write changed — see
    /// there for why the host must act on each flag, and
    /// [`crate::virtio_pci::VirtioPciBus::io_write`] for how it does.
    #[must_use = "a moved BAR strands every ioeventfd registered inside it, and a \
                  changed MSI-X control register may have released pending vectors"]
    pub fn io_write(&mut self, port: u16, data: &[u8]) -> ConfigWrite {
        let offset = usize::from(port.wrapping_sub(CONFIG_ADDRESS_PORT));
        if offset < 4 {
            let mut bytes = self.address.to_le_bytes();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = bytes.get_mut(offset + i) {
                    *slot = *byte;
                }
            }
            self.address = u32::from_le_bytes(bytes);
            return ConfigWrite::default();
        }
        let Some(target) = self.decode() else {
            return ConfigWrite::default();
        };
        let within = offset & 0x3;
        let Some(function) = self.function_mut(&target) else {
            return ConfigWrite::default();
        };
        // Only a function with an owner has anything behind it to be stranded;
        // the host bridge is a config space and nothing else.
        let owner = function.owner;
        let before = function.config.decode_state();
        let mirror_changed = function
            .config
            .write_dword_bytes(target.register, within, data);
        let after = function.config.decode_state();
        ConfigWrite {
            owner,
            decode_changed: before != after,
            mirror_changed,
        }
    }

    /// The window BAR `bar` of the function tagged `owner` decodes, or `None`
    /// when it decodes nothing (see [`ConfigSpace::bar_window`]).
    pub fn bar_window_of(&self, owner: usize, bar: u8) -> Option<(u64, u64)> {
        self.config_of(owner)?.bar_window(bar)
    }

    /// Decodes a guest physical address into the function that claims it.
    ///
    /// Returns `(owner token, BAR index, offset inside the window)`. `None` when
    /// no enabled BAR covers the address — which includes the case of a guest
    /// that has parked a BAR outside the host's aperture: the window is then
    /// simply not decoded, so writes go nowhere and reads produce zeroes.
    pub fn locate_mmio(&self, addr: u64) -> Option<(usize, u8, u64)> {
        if !(layout::PCI_MMIO_BASE..layout::PCI_MMIO_END).contains(&addr) {
            return None;
        }
        for function in &self.functions {
            // The host bridge has no owner and no BARs; skip it rather than
            // ending the search (it is the first function on the bus).
            let Some(owner) = function.owner else {
                continue;
            };
            for index in 0..6u8 {
                let Some((base, size)) = function.config.bar_window(index) else {
                    continue;
                };
                if let Some(offset) = addr.checked_sub(base) {
                    if offset < size {
                        return Some((owner, index, offset));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a virtio device: one 16 KiB memory BAR in the aperture,
    /// INTA# on the first PCI GSI, and two capability records.
    fn device_config(slot: u64) -> ConfigSpace {
        let base = u32::try_from(layout::pci_bar_slot(slot)).expect("aperture is below 4 GiB");
        let size = u32::try_from(layout::PCI_MMIO_SLOT_SIZE).expect("slot size fits");
        let mut config = ConfigSpace::type0(0x1af4, 0x1042, 0x0180_0000, 1)
            .with_subsystem(0x1af4, 0)
            .with_memory_bar(0, base, size)
            .expect("valid BAR")
            .with_interrupt(1, 5);
        // Two 16-byte vendor-specific records, the shape virtio uses.
        config
            .add_capability(&[0x09, 0, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0])
            .expect("first capability fits");
        config
            .add_capability(&[0x09, 0, 16, 3, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0x10, 0, 0])
            .expect("second capability fits");
        config
    }

    fn root_with_one_device() -> PciRoot {
        let mut root = PciRoot::new();
        assert_eq!(root.attach(device_config(0), 0), Ok(1));
        root
    }

    /// Selects a register the way `PciRead32`/`outl 0xcf8` does.
    fn select(root: &mut PciRoot, bus: u8, dev: u8, func: u8, reg: u8) {
        let address = CONFIG_ADDRESS_ENABLE
            | (u32::from(bus) << 16)
            | (u32::from(dev) << 11)
            | (u32::from(func) << 8)
            | u32::from(reg & 0xfc);
        let _ = root.io_write(CONFIG_ADDRESS_PORT, &address.to_le_bytes());
    }

    fn read32(root: &PciRoot) -> u32 {
        let mut data = [0u8; 4];
        root.io_read(CONFIG_DATA_PORT, &mut data);
        u32::from_le_bytes(data)
    }

    fn cfg_read32(root: &mut PciRoot, dev: u8, reg: u8) -> u32 {
        select(root, 0, dev, 0, reg);
        read32(root)
    }

    fn cfg_write32(root: &mut PciRoot, dev: u8, reg: u8, value: u32) {
        select(root, 0, dev, 0, reg);
        let _ = root.io_write(CONFIG_DATA_PORT, &value.to_le_bytes());
    }

    // ------------------------------------------ configuration mechanism #1

    /// The very first thing Linux's `pci_check_type1` does: write
    /// `0x8000_0000` to `0xcf8` and require it to read back unchanged. If this
    /// fails there is no PCI bus as far as the guest is concerned.
    #[test]
    fn the_config_address_latch_reads_back_verbatim() {
        let mut root = PciRoot::new();
        let _ = root.io_write(CONFIG_ADDRESS_PORT, &CONFIG_ADDRESS_ENABLE.to_le_bytes());
        let mut data = [0u8; 4];
        root.io_read(CONFIG_ADDRESS_PORT, &mut data);
        assert_eq!(u32::from_le_bytes(data), CONFIG_ADDRESS_ENABLE);

        // Byte-wise writes compose the same latch, and the low two bits of the
        // register field stay out of the decode.
        let _ = root.io_write(CONFIG_ADDRESS_PORT, &[0x03]);
        let _ = root.io_write(CONFIG_ADDRESS_PORT + 3, &[0x80]);
        root.io_read(CONFIG_ADDRESS_PORT, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x8000_0003);
        assert_eq!(root.decode().expect("enabled").register, 0);
    }

    #[test]
    fn a_disabled_address_decodes_nothing() {
        let mut root = root_with_one_device();
        let _ = root.io_write(CONFIG_ADDRESS_PORT, &0u32.to_le_bytes());
        assert_eq!(read32(&root), NO_DEVICE);
        // …and a write through it must not reach any device.
        let _ = root.io_write(CONFIG_DATA_PORT, &0xffff_ffffu32.to_le_bytes());
        assert_eq!(cfg_read32(&mut root, 1, reg::ID), 0x1042_1af4);
    }

    #[test]
    fn only_existing_devices_answer() {
        let mut root = root_with_one_device();
        // The host bridge and our one device exist…
        assert_eq!(cfg_read32(&mut root, 0, reg::ID) & 0xffff, 0x8086);
        assert_eq!(cfg_read32(&mut root, 1, reg::ID) & 0xffff, 0x1af4);
        // …nothing else does, on any bus or function.
        for (bus, dev, func) in [
            (0, 2, 0),
            (0, 31, 0),
            (0, 0, 1),
            (0, 1, 3),
            (1, 0, 0),
            (255, 1, 0),
        ] {
            select(&mut root, bus, dev, func, reg::ID);
            assert_eq!(
                read32(&root),
                NO_DEVICE,
                "{bus:02x}:{dev:02x}.{func} must read as absent"
            );
        }
    }

    /// Sub-dword accesses are how firmware reads a single id: OVMF's
    /// `PciRead16 (OVMF_HOSTBRIDGE_DID)` is a 16-bit read at offset 2.
    #[test]
    fn byte_and_word_accesses_slice_the_selected_dword() {
        let mut root = root_with_one_device();
        select(&mut root, 0, 1, 0, reg::ID);
        let mut word = [0u8; 2];
        root.io_read(CONFIG_DATA_PORT, &mut word);
        assert_eq!(u16::from_le_bytes(word), 0x1af4, "vendor id");
        root.io_read(CONFIG_DATA_PORT + 2, &mut word);
        assert_eq!(u16::from_le_bytes(word), 0x1042, "device id");
        for (port_offset, expected) in [(0u16, 0xf4u8), (1, 0x1a), (2, 0x42), (3, 0x10)] {
            let mut byte = [0u8; 1];
            root.io_read(CONFIG_DATA_PORT + port_offset, &mut byte);
            assert_eq!(byte[0], expected, "byte at +{port_offset}");
        }
        // An access that runs off the end of the dword floats high rather than
        // spilling into the next register.
        let mut wide = [0u8; 4];
        root.io_read(CONFIG_DATA_PORT + 2, &mut wide);
        assert_eq!(wide, [0x42, 0x10, 0xff, 0xff]);
    }

    #[test]
    fn identity_and_class_registers_are_read_only() {
        let mut root = root_with_one_device();
        for (reg, expected) in [
            (reg::ID, 0x1042_1af4),
            (reg::CLASS_REVISION, 0x0180_0001),
            (reg::SUBSYSTEM, 0x0000_1af4),
        ] {
            cfg_write32(&mut root, 1, reg, 0xdead_beef);
            assert_eq!(cfg_read32(&mut root, 1, reg), expected, "register {reg:#x}");
        }
        // Byte-wise attempts at the same thing.
        select(&mut root, 0, 1, 0, reg::ID);
        let _ = root.io_write(CONFIG_DATA_PORT + 1, &[0xff]);
        assert_eq!(cfg_read32(&mut root, 1, reg::ID), 0x1042_1af4);
        // Header type stays 0 even though the bytes below it are writable.
        cfg_write32(&mut root, 1, reg::HEADER_TYPE, 0xffff_ffff);
        let header = cfg_read32(&mut root, 1, reg::HEADER_TYPE);
        assert_eq!(header >> 16, 0, "header type and BIST are read-only");
        assert_eq!(
            header & 0xffff,
            0xffff,
            "cache line and latency are scratch"
        );
    }

    #[test]
    fn the_class_code_and_revision_share_one_dword() {
        let config = ConfigSpace::type0(0x1af4, 0x1041, 0x0200_0000, 1);
        assert_eq!(config.read_dword(reg::CLASS_REVISION), 0x0200_0001);
        // A class code carrying junk in the revision byte must not overwrite it.
        let config = ConfigSpace::type0(0x1af4, 0x1041, 0x0200_00ff, 3);
        assert_eq!(config.read_dword(reg::CLASS_REVISION), 0x0200_0003);
        // Base class in bits 31:24, sub class in 23:16 — a host bridge is 0x0600.
        assert_eq!(
            ConfigSpace::host_bridge().read_dword(reg::CLASS_REVISION) >> 16,
            0x0600
        );
    }

    // --------------------------------------------------- BAR sizing protocol

    /// The protocol every PCI enumerator uses: write all-ones, read back the
    /// size mask, then write the address. Getting this wrong makes Linux
    /// mis-size or reject the window.
    #[test]
    fn bar_sizing_protocol_reports_the_window_size() {
        let mut root = root_with_one_device();
        let base = u32::try_from(layout::pci_bar_slot(0)).unwrap();
        let size = u32::try_from(layout::PCI_MMIO_SLOT_SIZE).unwrap();
        assert_eq!(cfg_read32(&mut root, 1, reg::BAR0), base);

        cfg_write32(&mut root, 1, reg::BAR0, 0xffff_ffff);
        let mask = cfg_read32(&mut root, 1, reg::BAR0);
        assert_eq!(mask, !(size - 1), "size mask for a {size:#x}-byte window");
        // Decoded the way an enumerator decodes it: low four bits are flags,
        // and the size is the lowest set address bit.
        assert_eq!(mask & BAR_FLAG_MASK, 0, "32-bit memory, not prefetchable");
        assert_eq!((!(mask & !BAR_FLAG_MASK)).wrapping_add(1), size);

        // Programming it back restores the window.
        cfg_write32(&mut root, 1, reg::BAR0, base);
        assert_eq!(cfg_read32(&mut root, 1, reg::BAR0), base);
        // The low address bits below the size are not writable.
        cfg_write32(&mut root, 1, reg::BAR0, base | 0x3fff);
        assert_eq!(cfg_read32(&mut root, 1, reg::BAR0), base);
    }

    #[test]
    fn unimplemented_bars_read_zero_and_ignore_writes() {
        let mut root = root_with_one_device();
        for index in 1..6u8 {
            let register = reg::BAR0 + index * 4;
            cfg_write32(&mut root, 1, register, 0xffff_ffff);
            assert_eq!(
                cfg_read32(&mut root, 1, register),
                0,
                "BAR {index} must read as absent"
            );
        }
        // The host bridge has no BARs at all.
        cfg_write32(&mut root, 0, reg::BAR0, 0xffff_ffff);
        assert_eq!(cfg_read32(&mut root, 0, reg::BAR0), 0);
    }

    #[test]
    fn bad_bar_geometry_is_a_host_error() {
        let base = u32::try_from(layout::PCI_MMIO_BASE).unwrap();
        assert_eq!(
            ConfigSpace::type0(1, 2, 0, 1)
                .with_memory_bar(6, base, 0x1000)
                .err(),
            Some(PciError::NoSuchBar { index: 6 })
        );
        for size in [0u32, 8, 0x3000, 0x5000] {
            assert_eq!(
                ConfigSpace::type0(1, 2, 0, 1)
                    .with_memory_bar(0, base, size)
                    .err(),
                Some(PciError::BadBarSize { size }),
                "size {size:#x}"
            );
        }
        assert_eq!(
            ConfigSpace::type0(1, 2, 0, 1)
                .with_memory_bar(0, base + 0x100, 0x4000)
                .err(),
            Some(PciError::MisalignedBar {
                base: base + 0x100,
                size: 0x4000
            })
        );
    }

    // ------------------------------------------------- command-register gating

    #[test]
    fn memory_decoding_is_gated_by_the_command_register() {
        let mut root = root_with_one_device();
        let base = layout::pci_bar_slot(0);
        // A freshly reset device decodes nothing, however well programmed.
        assert_eq!(cfg_read32(&mut root, 1, reg::COMMAND) & 0xffff, 0);
        assert_eq!(root.locate_mmio(base), None);

        cfg_write32(&mut root, 1, reg::COMMAND, u32::from(command::MEMORY_SPACE));
        assert_eq!(root.locate_mmio(base), Some((0, 0, 0)));
        assert_eq!(root.locate_mmio(base + 0x2001), Some((0, 0, 0x2001)));
        // One past the end of the window is not ours.
        assert_eq!(root.locate_mmio(base + layout::PCI_MMIO_SLOT_SIZE), None);

        // I/O enable alone does not bring memory decoding back.
        cfg_write32(&mut root, 1, reg::COMMAND, u32::from(command::IO_SPACE));
        assert!(root.config_of(0).expect("device 1").io_enabled());
        assert!(!root.config_of(0).expect("device 1").memory_enabled());
        assert_eq!(root.locate_mmio(base), None);
    }

    #[test]
    fn only_the_defined_command_bits_are_writable() {
        let mut root = root_with_one_device();
        cfg_write32(&mut root, 1, reg::COMMAND, 0xffff_ffff);
        let command = cfg_read32(&mut root, 1, reg::COMMAND) & 0xffff;
        assert_eq!(command, command::WRITABLE);
        // The status half is read-only, and still advertises the capability list.
        let status = cfg_read32(&mut root, 1, reg::COMMAND) >> 16;
        assert_eq!(status, STATUS_CAP_LIST);
    }

    /// `pci_intx(dev, 0)` sets `INTX_DISABLE`; whatever raises the line must be
    /// able to see that without reaching into the config space.
    #[test]
    fn intx_disable_is_visible_to_the_interrupt_line() {
        let mut root = root_with_one_device();
        let flag = root.config_of(0).expect("device 1").intx_flag();
        assert!(flag.load(Ordering::Acquire), "INTx starts enabled");

        cfg_write32(
            &mut root,
            1,
            reg::COMMAND,
            u32::from(command::MEMORY_SPACE | command::INTX_DISABLE),
        );
        assert!(!flag.load(Ordering::Acquire));
        cfg_write32(&mut root, 1, reg::COMMAND, u32::from(command::MEMORY_SPACE));
        assert!(flag.load(Ordering::Acquire));
    }

    /// `interrupt_line` is host wiring, so nothing the guest writes may change
    /// it. EDK2's `PciBusDxe` writes `0xff` (`PCI_INT_LINE_UNKNOWN`) and then `0`
    /// during enumeration, expecting a platform driver to fill in the routed
    /// value afterwards; there is no such driver here because there is no PIRQ
    /// router. Letting those writes stick left Linux with `IRQ_NOTCONNECTED` and
    /// every virtio probe ending in `VIRTIO_CONFIG_S_FAILED`.
    #[test]
    fn the_interrupt_register_publishes_the_pin_and_the_line() {
        let mut root = root_with_one_device();
        let value = cfg_read32(&mut root, 1, reg::INTERRUPT);
        assert_eq!(value & 0xff, 5, "interrupt_line: the host's GSI");
        assert_eq!((value >> 8) & 0xff, 1, "interrupt_pin: INTA#");

        // Both of EDK2's clobbers, and a full-dword write, leave it alone.
        for clobber in [0xffff_ffffu32, 0x0000_0000, 0x0000_00ff] {
            cfg_write32(&mut root, 1, reg::INTERRUPT, clobber);
            let value = cfg_read32(&mut root, 1, reg::INTERRUPT);
            assert_eq!(value & 0xff, 5, "line survives a write of {clobber:#x}");
            assert_eq!(
                (value >> 8) & 0xff,
                1,
                "pin survives a write of {clobber:#x}"
            );
        }
        // A byte write to just the line register is refused too, which is the
        // width PciBusDxe actually uses (`EfiPciIoWidthUint8` at 0x3c).
        select(&mut root, 0, 1, 0, reg::INTERRUPT);
        let _ = root.io_write(CONFIG_DATA_PORT, &[0xff]);
        assert_eq!(cfg_read32(&mut root, 1, reg::INTERRUPT) & 0xff, 5);
    }

    // ------------------------------------------------------- capability list

    /// A driver finds every virtio structure by walking this list, so the walk
    /// has to terminate, stay inside the header, and hand back the records
    /// exactly as they were added.
    #[test]
    fn the_capability_list_walks_correctly() {
        let mut root = root_with_one_device();
        // The status bit is what makes a driver look for the list at all.
        assert_ne!(
            cfg_read32(&mut root, 1, reg::COMMAND) >> 16 & STATUS_CAP_LIST,
            0
        );

        let mut next = (cfg_read32(&mut root, 1, reg::CAP_POINTER) & 0xff) as u8;
        assert_eq!(next, reg::FIRST_CAPABILITY);
        let mut seen = Vec::new();
        let mut guard = 0;
        while next != 0 {
            guard += 1;
            assert!(
                guard <= MAX_PCI_DEVICES * 8,
                "capability list does not terminate"
            );
            assert!(next >= reg::FIRST_CAPABILITY, "list must stay in cap space");
            let header = cfg_read32(&mut root, 1, next);
            let id = (header & 0xff) as u8;
            let len = ((header >> 16) & 0xff) as u8;
            let cfg_type = ((header >> 24) & 0xff) as u8;
            assert_eq!(id, 0x09, "vendor-specific capability");
            assert!(
                u32::from(next) + u32::from(len) <= reg::SIZE as u32,
                "record at {next:#x} runs past the header"
            );
            seen.push((next, cfg_type));
            next = ((header >> 8) & 0xff) as u8;
        }
        assert_eq!(seen, vec![(0x40, 1), (0x50, 3)]);
        // Records are read-only.
        cfg_write32(&mut root, 1, 0x40, 0xffff_ffff);
        assert_eq!(cfg_read32(&mut root, 1, 0x40) & 0xff, 0x09);
    }

    #[test]
    fn capability_space_is_bounded() {
        let mut config = ConfigSpace::type0(1, 2, 0, 1);
        let record = [0x09u8; 16];
        // 0x40..0x100 is 192 bytes: exactly twelve 16-byte records.
        for i in 0..12 {
            assert!(config.add_capability(&record).is_ok(), "record {i}");
        }
        assert!(matches!(
            config.add_capability(&record),
            Err(PciError::CapabilitySpaceExhausted { .. })
        ));
        assert_eq!(
            config.add_capability(&[0x09]),
            Err(PciError::CapabilityTooShort)
        );
    }

    #[test]
    fn a_device_without_capabilities_says_so() {
        let config = ConfigSpace::type0(1, 2, 0, 1);
        assert_eq!(config.read_dword(reg::CAP_POINTER), 0);
        assert_eq!(u32::from(config.status()) & STATUS_CAP_LIST, 0);
    }

    // ------------------------------------------------------ address decoding

    #[test]
    fn several_devices_get_distinct_windows_and_device_numbers() {
        let mut root = PciRoot::new();
        for slot in 0..3u64 {
            let device = root
                .attach(device_config(slot), slot as usize)
                .expect("bus has room");
            assert_eq!(u64::from(device), slot + 1, "dense device numbers");
            cfg_write32(
                &mut root,
                device,
                reg::COMMAND,
                u32::from(command::MEMORY_SPACE),
            );
        }
        for slot in 0..3u64 {
            let base = layout::pci_bar_slot(slot);
            assert_eq!(root.locate_mmio(base), Some((slot as usize, 0, 0)));
            assert_eq!(
                root.locate_mmio(base + layout::PCI_MMIO_SLOT_SIZE - 1),
                Some((slot as usize, 0, layout::PCI_MMIO_SLOT_SIZE - 1))
            );
        }
    }

    #[test]
    fn the_bus_is_bounded() {
        let mut root = PciRoot::new();
        for slot in 0..MAX_PCI_DEVICES - 1 {
            assert!(root.attach(device_config(slot as u64), slot).is_ok());
        }
        assert_eq!(root.len(), MAX_PCI_DEVICES);
        assert_eq!(root.attach(device_config(0), 99), Err(PciError::BusFull));
        // Every window the bound allows stays inside the aperture, which stays
        // clear of both guest RAM and the virtio-mmio window.
        const _: () = assert!(layout::PCI_MMIO_END <= layout::VIRTIO_MMIO_BASE);
        const _: () = assert!(layout::PCI_MMIO_BASE >= layout::MMIO_HOLE_START);
        assert_eq!(
            root.locate_mmio(layout::PCI_MMIO_END - 1),
            None,
            "the last aperture slot decodes nothing until its device enables it"
        );
    }

    /// Malicious guest: a BAR pointed somewhere else entirely must decode
    /// nothing, rather than making the machine dispatch a foreign address into a
    /// device.
    #[test]
    fn a_bar_moved_out_of_the_aperture_decodes_nothing() {
        let mut root = root_with_one_device();
        cfg_write32(&mut root, 1, reg::COMMAND, u32::from(command::MEMORY_SPACE));
        assert!(root.locate_mmio(layout::pci_bar_slot(0)).is_some());

        // Park it on the virtio-mmio window, on guest RAM, and at zero.
        for base in [layout::VIRTIO_MMIO_BASE as u32, 0x1000_0000, 0] {
            cfg_write32(&mut root, 1, reg::BAR0, base);
            assert_eq!(
                root.locate_mmio(u64::from(base)),
                None,
                "a BAR at {base:#x} must not decode"
            );
            // And the aperture itself no longer answers for this device.
            assert_eq!(root.locate_mmio(layout::pci_bar_slot(0)), None);
        }
    }

    #[test]
    fn addresses_outside_the_aperture_are_never_claimed() {
        let mut root = root_with_one_device();
        cfg_write32(&mut root, 1, reg::COMMAND, u32::from(command::MEMORY_SPACE));
        for addr in [
            0,
            0x1000,
            layout::PCI_MMIO_BASE - 1,
            layout::PCI_MMIO_END,
            layout::VIRTIO_MMIO_BASE,
            u64::MAX,
        ] {
            assert_eq!(root.locate_mmio(addr), None, "address {addr:#x}");
        }
    }

    /// Every port the mechanism claims, and nothing else — in particular not
    /// the ACPI PM timer or the serial console.
    #[test]
    fn port_range_is_exactly_the_two_registers() {
        for port in 0xcf8..=0xcffu16 {
            assert!(PciRoot::contains(port), "port {port:#x}");
        }
        assert!(!PciRoot::contains(0xcf7));
        assert!(!PciRoot::contains(0xd00));
        assert!(!PciRoot::contains(0x608));
        for port in 0x3f8..=0x3ffu16 {
            assert!(!PciRoot::contains(port));
        }
    }
}
