//! The minimum platform surface an EDK2 CloudHv firmware probes before it can
//! run at all (backlog UEFI-1802, ADR-0003 phase 2 gap map).
//!
//! Two pieces, both taken from EDK2's own `OvmfPkg/Include/IndustryStandard/
//! CloudHv.h` rather than invented here:
//!
//! * **PCI configuration space** on the legacy `0xcf8`/`0xcfc` ports, with a
//!   single host bridge at `00:00.0` whose device ID is `CLOUDHV_DEVICE_ID`
//!   (`0x0d57`). Every OVMF phase starts by doing
//!   `PciRead16 (OVMF_HOSTBRIDGE_DID)` and switching on the answer; an
//!   unclaimed port reads back `0xffff`, lands in the `default:` arm and
//!   `ASSERT (FALSE)`s in SEC before the firmware has done anything.
//! * **The ACPI power-management timer** at `CLOUDHV_ACPI_TIMER_IO_ADDRESS`
//!   (`0x0608`). For a CloudHv platform `InternalAcpiGetTimerTick()` is a plain
//!   `IoRead32 (0x608)`, and `MicroSecondDelay()` spins on it — a constant
//!   value there is an unbreakable hang, not a slow boot. That timer has since
//!   moved into [`crate::acpi::pm::AcpiPmBlock`] along with the rest of the ACPI
//!   PM register block, because a direct-Linux guest now gets a FADT and needs
//!   it too.
//!
//! This is emphatically **not** a PCI bus: there is exactly one device, it has
//! no BARs, and configuration writes are dropped. A real bus (and virtio-pci
//! on top of it) is the phase-3 work UEFI-1803 depends on.
//!
//! Enabled only for `BootMode::Uefi`, so a direct-Linux guest sees exactly the
//! machine it saw before this module existed.

// ---- PCI configuration space --------------------------------------------

/// `CONFIG_ADDRESS`, the 32-bit BDF+register selector.
pub const PCI_CONFIG_ADDRESS: u16 = 0x0cf8;
/// `CONFIG_DATA`, the 32-bit data window (byte/word accesses use the low bits
/// of the port number as an offset within the selected dword).
pub const PCI_CONFIG_DATA: u16 = 0x0cfc;

/// Cloud Hypervisor's host bridge device ID (`CLOUDHV_DEVICE_ID`).
pub const CLOUDHV_HOST_BRIDGE_DEVICE_ID: u16 = 0x0d57;
/// Intel, the vendor OVMF expects for every host bridge it knows.
pub const HOST_BRIDGE_VENDOR_ID: u16 = 0x8086;

/// The value the CPU sees when nothing decodes a configuration access.
const NO_DEVICE: u32 = 0xffff_ffff;

/// Config-space register offsets we implement.
const REG_ID: u8 = 0x00; // vendor + device
const REG_STATUS_COMMAND: u8 = 0x04;
const REG_CLASS_REVISION: u8 = 0x08;
const REG_HEADER: u8 = 0x0c; // cache line, latency, header type, BIST

/// Class code for a host bridge: base 0x06 (bridge), sub 0x00 (host).
const CLASS_HOST_BRIDGE: u32 = 0x0600_0000;

/// A one-device PCI configuration space: the host bridge and nothing else.
#[derive(Debug, Default)]
pub struct PciConfigSpace {
    /// Latched `CONFIG_ADDRESS`.
    address: u32,
}

/// A decoded `CONFIG_ADDRESS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigTarget {
    bus: u8,
    device: u8,
    function: u8,
    register: u8,
}

impl PciConfigSpace {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when `port` belongs to the legacy configuration mechanism.
    pub fn contains(port: u16) -> bool {
        (PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 8).contains(&port)
    }

    /// Machine reset (ADR-0005): the only mutable state here is the latched
    /// `CONFIG_ADDRESS`, and a half-written one must not be inherited by the
    /// next boot's first configuration read.
    pub fn reset(&mut self) {
        self.address = 0;
    }

    /// The latched `CONFIG_ADDRESS` (ADR-0006).
    pub fn address(&self) -> u32 {
        self.address
    }

    /// Puts it back.
    pub fn set_address(&mut self, address: u32) {
        self.address = address;
    }

    fn decode(&self) -> Option<ConfigTarget> {
        // Bit 31 enables the mechanism; bits 1:0 of the register field are
        // always zero (accesses are dword-aligned).
        if self.address & 0x8000_0000 == 0 {
            return None;
        }
        Some(ConfigTarget {
            bus: ((self.address >> 16) & 0xff) as u8,
            device: ((self.address >> 11) & 0x1f) as u8,
            function: ((self.address >> 8) & 0x07) as u8,
            register: (self.address & 0xfc) as u8,
        })
    }

    /// The dword at `target`, or all-ones when nothing is there.
    fn read_dword(&self, target: ConfigTarget) -> u32 {
        if target.bus != 0 || target.device != 0 || target.function != 0 {
            return NO_DEVICE;
        }
        match target.register {
            REG_ID => {
                u32::from(HOST_BRIDGE_VENDOR_ID) | (u32::from(CLOUDHV_HOST_BRIDGE_DEVICE_ID) << 16)
            }
            REG_STATUS_COMMAND => 0,
            REG_CLASS_REVISION => CLASS_HOST_BRIDGE,
            // Header type 0 (a normal, single-function device); no BIST.
            REG_HEADER => 0,
            // Everything else (BARs, capability pointer, interrupt line) reads
            // as zero: absent, not broken.
            _ => 0,
        }
    }

    /// Guest read from a configuration port. `data` may be 1, 2 or 4 bytes.
    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        let offset = usize::from(port - PCI_CONFIG_ADDRESS);
        let value = if offset < 4 {
            self.address
        } else {
            match self.decode() {
                Some(target) => self.read_dword(target),
                None => NO_DEVICE,
            }
        };
        // A byte/word access reads that slice of the selected dword; the port
        // offset within the 4-byte window selects which.
        let within = offset & 0x3;
        let bytes = value.to_le_bytes();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = bytes.get(within + i).copied().unwrap_or(0xff);
        }
    }

    /// Guest write to a configuration port. Only `CONFIG_ADDRESS` is stored;
    /// writes to configuration *data* are dropped, since the host bridge has
    /// nothing writable and there is no bus to enumerate.
    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        let offset = usize::from(port - PCI_CONFIG_ADDRESS);
        if offset >= 4 {
            tracing::trace!(
                port = format_args!("{port:#x}"),
                address = format_args!("{:#x}", self.address),
                "dropping PCI config-space write (no writable registers)"
            );
            return;
        }
        let mut bytes = self.address.to_le_bytes();
        for (i, byte) in data.iter().enumerate() {
            if let Some(slot) = bytes.get_mut(offset + i) {
                *slot = *byte;
            }
        }
        self.address = u32::from_le_bytes(bytes);
    }
}

// ---- ACPI power-management timer ----------------------------------------
//
// Moved out: the PM timer at `CLOUDHV_ACPI_TIMER_IO_ADDRESS` is now one register
// of `crate::acpi::pm::AcpiPmBlock`, which owns the whole 0x600..0x610 block and
// is present in *both* boot modes — the FADT declares it to direct-Linux guests
// too, so it can no longer be a UEFI-only device. `AcpiPmTimer` itself still
// exists, in `crate::acpi::pm`.

/// The firmware-facing platform devices, enabled for UEFI boots only.
#[derive(Debug, Default)]
pub struct FirmwarePlatform {
    pub pci: PciConfigSpace,
    /// The RTC/CMOS: `EFI_RUNTIME_SERVICES.GetTime()` has nowhere else to come
    /// from, and `PcRtcInit()` fails its entry point without it.
    pub rtc: crate::rtc::Rtc,
}

impl FirmwarePlatform {
    pub fn new() -> Self {
        Self {
            pci: PciConfigSpace::new(),
            rtc: crate::rtc::Rtc::new(),
        }
    }

    /// True when `port` belongs to one of these devices.
    pub fn contains(port: u16) -> bool {
        PciConfigSpace::contains(port) || crate::rtc::Rtc::contains(port)
    }

    /// Machine reset (ADR-0005): both devices back to power-on, which is what
    /// the firmware about to be re-entered expects to find.
    pub fn reset(&mut self) {
        self.pci.reset();
        self.rtc.reset();
    }

    /// Both devices' state (ADR-0006).
    pub fn save_state(&self) -> crate::state::SavedPlatform {
        crate::state::SavedPlatform {
            config_address: self.pci.address(),
            rtc: self.rtc.save_state(),
        }
    }

    /// Puts it back.
    pub fn load_state(
        &mut self,
        state: &crate::state::SavedPlatform,
    ) -> Result<(), crate::state::StateError> {
        self.pci.set_address(state.config_address);
        self.rtc.load_state(&state.rtc)
    }

    /// Returns true when the read was handled.
    pub fn io_read(&mut self, port: u16, data: &mut [u8]) -> bool {
        if PciConfigSpace::contains(port) {
            self.pci.io_read(port, data);
            return true;
        }
        if crate::rtc::Rtc::contains(port) {
            self.rtc.io_read(port, data);
            return true;
        }
        false
    }

    /// Returns true when the write was handled (possibly by dropping it).
    pub fn io_write(&mut self, port: u16, data: &[u8]) -> bool {
        if PciConfigSpace::contains(port) {
            self.pci.io_write(port, data);
            return true;
        }
        if crate::rtc::Rtc::contains(port) {
            self.rtc.io_write(port, data);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Selects a register the way `PciRead16 (OVMF_HOSTBRIDGE_DID)` does.
    fn select(pci: &mut PciConfigSpace, bus: u8, dev: u8, func: u8, reg: u8) {
        let address = 0x8000_0000
            | (u32::from(bus) << 16)
            | (u32::from(dev) << 11)
            | (u32::from(func) << 8)
            | u32::from(reg & 0xfc);
        pci.io_write(PCI_CONFIG_ADDRESS, &address.to_le_bytes());
    }

    /// The read that decides whether the firmware boots at all: a 16-bit read
    /// at offset 0x02 of 00:00.0 must return 0x0d57.
    #[test]
    fn host_bridge_device_id_is_cloudhv() {
        let mut pci = PciConfigSpace::new();
        select(&mut pci, 0, 0, 0, 0x00);
        let mut did = [0u8; 2];
        // OVMF_HOSTBRIDGE_DID is offset 0x02, i.e. the high word: read at
        // CONFIG_DATA + 2.
        pci.io_read(PCI_CONFIG_DATA + 2, &mut did);
        assert_eq!(
            u16::from_le_bytes(did),
            CLOUDHV_HOST_BRIDGE_DEVICE_ID,
            "OVMF asserts in SEC on anything else"
        );

        let mut vid = [0u8; 2];
        pci.io_read(PCI_CONFIG_DATA, &mut vid);
        assert_eq!(u16::from_le_bytes(vid), HOST_BRIDGE_VENDOR_ID);

        let mut full = [0u8; 4];
        pci.io_read(PCI_CONFIG_DATA, &mut full);
        assert_eq!(u32::from_le_bytes(full), 0x0d57_8086);
    }

    #[test]
    fn only_bus0_device0_function0_exists() {
        let mut pci = PciConfigSpace::new();
        for (bus, dev, func) in [(0, 0, 1), (0, 1, 0), (0, 31, 7), (1, 0, 0), (255, 0, 0)] {
            select(&mut pci, bus, dev, func, 0x00);
            let mut id = [0u8; 4];
            pci.io_read(PCI_CONFIG_DATA, &mut id);
            assert_eq!(
                u32::from_le_bytes(id),
                0xffff_ffff,
                "{bus:02x}:{dev:02x}.{func} must read as absent"
            );
        }
    }

    #[test]
    fn host_bridge_class_and_header_are_sane() {
        let mut pci = PciConfigSpace::new();
        select(&mut pci, 0, 0, 0, 0x08);
        let mut class = [0u8; 4];
        pci.io_read(PCI_CONFIG_DATA, &mut class);
        let value = u32::from_le_bytes(class);
        assert_eq!(value >> 24, 0x06, "base class: bridge");
        assert_eq!((value >> 16) & 0xff, 0x00, "sub class: host bridge");

        select(&mut pci, 0, 0, 0, 0x0c);
        let mut header = [0u8; 1];
        pci.io_read(PCI_CONFIG_DATA + 2, &mut header);
        assert_eq!(header[0], 0, "header type 0, single function");
    }

    #[test]
    fn config_address_reads_back_and_data_writes_are_dropped() {
        let mut pci = PciConfigSpace::new();
        select(&mut pci, 0, 0, 0, 0x00);
        let mut addr = [0u8; 4];
        pci.io_read(PCI_CONFIG_ADDRESS, &mut addr);
        assert_eq!(u32::from_le_bytes(addr), 0x8000_0000);

        // Try to overwrite the device ID; it must not stick.
        pci.io_write(PCI_CONFIG_DATA, &0xdead_beefu32.to_le_bytes());
        let mut id = [0u8; 4];
        pci.io_read(PCI_CONFIG_DATA, &mut id);
        assert_eq!(u32::from_le_bytes(id), 0x0d57_8086);
    }

    /// A disabled CONFIG_ADDRESS (bit 31 clear) must not expose the bridge.
    #[test]
    fn disabled_config_address_decodes_nothing() {
        let mut pci = PciConfigSpace::new();
        pci.io_write(PCI_CONFIG_ADDRESS, &0u32.to_le_bytes());
        let mut id = [0u8; 4];
        pci.io_read(PCI_CONFIG_DATA, &mut id);
        assert_eq!(u32::from_le_bytes(id), 0xffff_ffff);
    }

    #[test]
    fn port_ranges_do_not_overlap_the_serial_console() {
        assert!(FirmwarePlatform::contains(0xcf8));
        assert!(FirmwarePlatform::contains(0xcfc));
        assert!(FirmwarePlatform::contains(0xcff));
        assert!(!FirmwarePlatform::contains(0xd00));
        // The ACPI PM block (0x600..0x610) belongs to `crate::acpi::pm` now, in
        // both boot modes; this device must not claim any of it.
        for port in 0x600..=0x60fu16 {
            assert!(!FirmwarePlatform::contains(port), "port {port:#x}");
            assert!(crate::acpi::AcpiPmBlock::contains(port));
        }
        assert!(FirmwarePlatform::contains(0x70));
        assert!(FirmwarePlatform::contains(0x71));
        assert!(!FirmwarePlatform::contains(0x72));
        // COM1 (0x3f8..=0x3ff) must stay with the UART. Literals rather than
        // `serial::SERIAL_PORT_BASE` so this test also runs on non-Linux hosts,
        // where the serial module is gated out.
        for port in 0x3f8..=0x3ffu16 {
            assert!(!FirmwarePlatform::contains(port), "port {port:#x}");
        }
    }

    // The PM timer's own tests moved with it, to `crate::acpi::pm`.
}
