//! The machine's own device state, beside every `reset()`
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! ADR-0005's reset table said, device by device, what goes back to power-on.
//! This is the same table read the other way: for each of those pieces of
//! state, what a suspend has to write down. Everything here is **plain data**
//! with no I/O and no hypervisor — it builds and is unit-tested on both hosts,
//! and `vm-snapshot` is the only crate that knows how any of it is spelled on
//! disk.
//!
//! # The two things that are not registers
//!
//! **Time.** The 8254's counters and the ACPI PM timer are anchored to an
//! `Instant` taken when the VM started, and a restored VM's `Instant::now()`
//! has nothing to do with the one the snapshot was taken against. So neither is
//! saved as an absolute reading: the PIT records *how many ticks ago* each
//! channel was armed and when its next edge is due, and the PM timer records
//! the count the guest last would have seen. Putting them back is then a matter
//! of anchoring to the new epoch, and a guest that was 3 ms into a 10 ms delay
//! comes back 3 ms into it.
//!
//! **What is host wiring and stays behind.** The IOAPIC's id, the serial
//! console's output sink and interrupt line, the pflash *contents* (they are
//! the NVRAM file), the diagnostic counters, and every ioeventfd registration.
//! All of them are rebuilt by the machine that loads the snapshot, exactly as
//! they are rebuilt by a reset — and a snapshot that carried them would let a
//! file decide where the host's own plumbing goes.

use thiserror::Error;

/// Why a saved machine state could not be put back.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateError {
    #[error("the snapshot has {snapshot} {what}, this machine has {current}")]
    Count {
        what: &'static str,
        snapshot: usize,
        current: usize,
    },

    #[error("the snapshot {has} a {what}, this machine {does_not}")]
    Presence {
        what: &'static str,
        has: &'static str,
        does_not: &'static str,
    },

    #[error("the snapshot carries the invalid value {value} for {what}")]
    BadValue { what: &'static str, value: u64 },

    #[error("virtio slot {slot}: {source}")]
    Virtio {
        slot: usize,
        #[source]
        source: virtio_core::StateError,
    },

    #[error("a device lock is poisoned; {0} was not restored")]
    Poisoned(&'static str),
}

/// The 16550's register file and whatever the host had typed at it.
///
/// The receive queue goes with it: the installer types into that queue from the
/// host (`MachineBus::push_serial_input`), and a suspend that dropped it would
/// lose keystrokes the guest had not read yet. The *output* sink does not —
/// it is where this process writes, not something the guest owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedSerial {
    pub ier: u8,
    pub lcr: u8,
    pub mcr: u8,
    pub scr: u8,
    pub dll: u8,
    pub dlh: u8,
    pub thre_pending: bool,
    pub rx: Vec<u8>,
}

/// The RTC's index latch and CMOS bytes.
///
/// The time-of-day registers are **not** in here, and cannot be: they are
/// computed from the host clock on every read. So a restored guest sees
/// wall-clock time that has moved on by however long the snapshot sat on disk,
/// which is exactly what a laptop's own suspend does and exactly what
/// `hwclock`/NTP expect to correct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedRtc {
    pub index: u8,
    pub cmos: Vec<u8>,
}

/// The firmware platform stub: the host-bridge config latch and the RTC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPlatform {
    pub config_address: u32,
    pub rtc: SavedRtc,
}

/// The ACPI fixed-feature registers, the PM timer's reading and the shutdown
/// latch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedAcpiPm {
    pub pm1a_sts: u16,
    pub pm1a_en: u16,
    pub pm1a_cnt: u16,
    pub gpe0_sts: u16,
    pub gpe0_en: u16,
    pub sleep_control: u8,
    pub sleep_status: u8,
    /// The counter value the guest would have read at the moment of the
    /// snapshot. Restored by moving the timer's origin back, not by starting
    /// from zero: a firmware spinning in `MicroSecondDelay()` must come back
    /// where it was.
    pub timer_ticks: u32,
    pub shutdown_requested: bool,
}

/// The CFI flash command state machine. The contents are the NVRAM file and
/// are not in the snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedPflash {
    /// 0 read-array, 1 read-status, 2 program, 3 erase.
    pub command: u8,
    pub status: u8,
}

/// One PCI function's register file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPciFunction {
    pub device: u8,
    /// The 64 dwords of the type-0 header and its capability list, as the guest
    /// left them — BAR addresses, command register, MSI-X message control.
    pub regs: Vec<u32>,
}

/// The PCI root bus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPciRoot {
    /// The latched `CONFIG_ADDRESS`. A guest suspended between writing it and
    /// reading `CONFIG_DATA` must find it still latched.
    pub address: u32,
    pub functions: Vec<SavedPciFunction>,
}

/// One 8259A.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedPicChip {
    pub imr: u8,
    pub irr: u8,
    pub isr: u8,
    pub vector_base: u8,
    pub cascade: u8,
    pub icw4: u8,
    /// Where in the ICW1..ICW4 initialisation sequence the chip is: 0 idle,
    /// 1 expecting ICW2, 2 expecting ICW3, 3 expecting ICW4.
    ///
    /// Saving it is not pedantry. A guest suspended half-way through
    /// `init_8259A()` and restored with the chip "idle" would have its next
    /// ICW2 write interpreted as an interrupt mask.
    pub step: u8,
    pub expect_icw4: bool,
    pub expect_icw3: bool,
    pub read_isr: bool,
    pub elcr: u8,
}

/// The master/slave pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedPic {
    pub master: SavedPicChip,
    pub slave: SavedPicChip,
}

/// One 8254 channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedPitChannel {
    pub reload: u16,
    pub mode: u8,
    /// 0 latch-count, 1 lo-both, 2 hi-both, 3 lo-then-hi.
    pub access: u8,
    /// How many 8254 ticks ago this channel was armed — a *relative* number,
    /// because the absolute one is anchored to a host `Instant` the restored
    /// process does not have.
    pub ticks_since_armed: u64,
    pub armed: bool,
    pub write_lo: Option<u8>,
    pub latched: Option<u16>,
    pub read_hi_next: bool,
    pub gate: bool,
}

/// The 8254.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPit {
    pub channels: Vec<SavedPitChannel>,
    pub nmi_control: u8,
    /// Ticks from the moment of the snapshot until channel 0's next output
    /// edge, where one is scheduled. Relative, for the same reason as
    /// `ticks_since_armed`.
    pub ticks_to_next_edge: Option<u64>,
}

/// The I/O APIC's guest-programmed half.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedIoApic {
    pub select: u32,
    pub redirection: Vec<u64>,
    /// One bit per pin: an edge arrived while the pin was masked and is still
    /// owed to the guest. Carried, because losing it loses an interrupt a
    /// device has already been told was delivered.
    pub pending: u32,
}

/// The userspace interrupt controllers, on a host that runs them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedIrqChip {
    pub pic: SavedPic,
    pub pit: SavedPit,
    pub ioapic: SavedIoApic,
}

/// The guest reset controls' latches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedResetControl {
    pub rcr: u32,
    pub requested: bool,
    pub count: u32,
}

/// One virtio slot's saved state, with the slot it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedVirtioSlot {
    pub slot: u32,
    pub state: virtio_core::TransportSaveState,
}

/// Everything on the machine bus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineState {
    pub serial: SavedSerial,
    pub acpi_pm: SavedAcpiPm,
    pub reset: SavedResetControl,
    /// Present only for a UEFI boot.
    pub platform: Option<SavedPlatform>,
    /// Present only for a UEFI boot with an NVRAM file.
    pub pflash: Option<SavedPflash>,
    /// Present only on the pci transport.
    pub pci_root: Option<SavedPciRoot>,
    /// Present only on a host whose hypervisor has no in-kernel chips (WHP).
    pub irqchip: Option<SavedIrqChip>,
    /// Every attached virtio slot, in bus order.
    pub virtio: Vec<SavedVirtioSlot>,
}

/// Checks that two optional halves of the machine agree about existing.
pub(crate) fn require_same_presence<T>(
    what: &'static str,
    snapshot: Option<&T>,
    current: bool,
) -> Result<(), StateError> {
    match (snapshot.is_some(), current) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(StateError::Presence {
            what,
            has: "has",
            does_not: "does not",
        }),
        (false, true) => Err(StateError::Presence {
            what,
            has: "does not have",
            does_not: "does",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_mismatches_read_as_sentences() {
        let missing = require_same_presence("pflash", Some(&1u8), false).unwrap_err();
        assert_eq!(
            missing.to_string(),
            "the snapshot has a pflash, this machine does not"
        );
        let extra = require_same_presence::<u8>("pflash", None, true).unwrap_err();
        assert_eq!(
            extra.to_string(),
            "the snapshot does not have a pflash, this machine does"
        );
        assert!(require_same_presence(" ", Some(&1u8), true).is_ok());
        assert!(require_same_presence::<u8>(" ", None, false).is_ok());
    }
}
