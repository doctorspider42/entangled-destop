//! The machine's reset controls: the three port-I/O mechanisms an x86 guest
//! uses to reboot itself ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md)).
//!
//! A guest never asks its VMM to reboot; it pokes hardware, and it pokes it in
//! a *ladder*, moving on to the next rung when the machine does not restart.
//! Linux's `native_machine_emergency_restart()` walks
//! `BOOT_ACPI -> BOOT_KBD -> BOOT_EFI -> BOOT_CF9_FORCE -> BOOT_TRIPLE`, and
//! EDK2's `ResetSystemLib` for this machine's host bridge does 0xCF9 first and
//! the keyboard controller second. So the reset matrix is not a choice between
//! mechanisms — a machine that implements only one of them still reboots, but
//! only after the guest has spent its way down to it, and on WHP the last rung
//! (a triple fault) is absorbed by the hypervisor and never arrives at all
//! (ADR-0002 phase 4). Implementing the whole ladder is what makes a reboot
//! prompt and host-independent.
//!
//! | Mechanism | Where | Who uses it |
//! |---|---|---|
//! | ACPI `RESET_REG` | this block, via the FADT: I/O 0xCF9, value 0x0E | Linux `acpi_reboot()`, the first rung |
//! | 0xCF9 reset control | [`PORT_RESET_CONTROL`] | Linux `BOOT_CF9_*`, EDK2 `ResetCold`/`ResetWarm` |
//! | keyboard controller pulse | [`PORT_KBD_COMMAND`], value [`KBD_PULSE_RESET`] | Linux `BOOT_KBD` (`reboot=k`), EDK2's fallback |
//! | triple fault | not a device — `KVM_EXIT_SHUTDOWN` | Linux `BOOT_TRIPLE`; **KVM only** |
//!
//! Because the ACPI reset register and 0xCF9 are the same port, one latch
//! serves all three device paths, and [`vmm_core::ExitHandler::reset_requested`]
//! reports it exactly the way the ACPI PM block's S5 latch is reported.
//!
//! **The guest is untrusted.** Every write is a single byte decoded from a
//! constant port, the latch is a `bool`, and nothing here allocates, blocks or
//! can panic. A reset the host cannot serve (no lifecycle attached) becomes a
//! clean stop, never an unbounded loop — see
//! `vmm_core::lifecycle::Lifecycle::request_guest_reset`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The PIIX/ICH **Reset Control Register** (RCR).
///
/// Also the register this machine's FADT names as `RESET_REG`, which is what
/// makes Linux's very first reboot attempt land here instead of walking the
/// ladder. QEMU's q35 publishes the same pair (0xCF9 / 0x0E).
pub const PORT_RESET_CONTROL: u16 = 0x0cf9;

/// The 8042 keyboard controller's command port. Only the reset pulse is served:
/// this machine has no PS/2 controller, the FADT does not claim one, and
/// answering *reads* here would invite a guest to go looking for one.
pub const PORT_KBD_COMMAND: u16 = 0x0064;

/// `outb(0xfe, 0x64)` — "pulse the reset line low". What Linux's `BOOT_KBD`
/// path writes ten times over, and what `reboot=k` selects outright.
pub const KBD_PULSE_RESET: u8 = 0xfe;

/// RCR bit 1, `SYS_RST`: hold the platform in reset.
const RCR_SYSTEM_RESET: u8 = 1 << 1;
/// RCR bit 2, `RST_CPU`: the write that actually pulls the trigger. Reads back
/// as zero on real hardware — it is a self-clearing strobe.
const RCR_CPU_RESET: u8 = 1 << 2;
/// RCR bit 3, `FULL_RST`: a full (cold) reset rather than a warm one.
const RCR_FULL_RESET: u8 = 1 << 3;

/// The value this machine's FADT publishes as `RESET_VALUE`: a cold reset
/// (`FULL_RST | RST_CPU | SYS_RST`), the same value QEMU's q35 FADT carries.
pub const ACPI_RESET_VALUE: u8 = RCR_FULL_RESET | RCR_CPU_RESET | RCR_SYSTEM_RESET;

/// How a guest asked to be restarted, for the log line and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetKind {
    /// 0xCF9 without `FULL_RST`: a warm reset. This machine makes no
    /// distinction — both re-run the whole bring-up — but the guest's intent is
    /// worth recording.
    Warm,
    /// 0xCF9 with `FULL_RST`, which is also the ACPI `RESET_VALUE`.
    Cold,
    /// The keyboard controller's reset pulse.
    KeyboardPulse,
}

impl ResetKind {
    /// The name that appears in the log line, so a reboot that took the wrong
    /// rung of the ladder is visible without a debugger.
    pub fn as_str(self) -> &'static str {
        match self {
            ResetKind::Warm => "0xcf9 warm reset",
            ResetKind::Cold => "0xcf9 cold reset (also the ACPI reset register)",
            ResetKind::KeyboardPulse => "keyboard controller reset pulse (port 0x64 <- 0xfe)",
        }
    }
}

/// The machine's reset control register and keyboard-controller reset pulse,
/// behind one latch.
///
/// Shared by every vCPU's clone of the bus, like the ACPI PM block: the latch is
/// an atomic so the check on the exit path takes no lock.
#[derive(Debug, Default)]
pub struct ResetControl {
    /// The last value written to 0xCF9, minus the self-clearing strobe.
    rcr: AtomicU32,
    requested: AtomicBool,
    /// How many resets this machine has served, for the run report.
    count: AtomicU32,
}

impl ResetControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when `port` is one this block *reads and writes* — only 0xCF9. The
    /// keyboard command port is write-only here (see [`Self::claims_write`]).
    pub fn claims_port(port: u16) -> bool {
        port == PORT_RESET_CONTROL
    }

    /// True when `port` is one this block takes writes for.
    pub fn claims_write(port: u16) -> bool {
        Self::claims_port(port) || port == PORT_KBD_COMMAND
    }

    /// True once the guest has asked to restart. Latching, like the ACPI S5
    /// latch: a vCPU other than the one that did the write still sees it.
    pub fn is_reset_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// How many guest reset requests this machine has latched since it started.
    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }

    /// Consumes the latch, so the next boot starts with a clean one. Called as
    /// part of the machine reset.
    pub fn clear(&self) {
        self.requested.store(false, Ordering::Release);
        self.rcr.store(0, Ordering::Release);
    }

    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        data.fill(0);
        if !Self::claims_port(port) {
            return;
        }
        // Only the byte at 0xCF9 exists; a wider access reads zeroes above it,
        // which is what an unimplemented register does.
        if let Some(first) = data.first_mut() {
            *first = self.rcr.load(Ordering::Acquire) as u8 & !RCR_CPU_RESET;
        }
    }

    pub fn io_write(&self, port: u16, data: &[u8]) {
        let Some(&value) = data.first() else {
            return;
        };
        match port {
            PORT_RESET_CONTROL => {
                self.rcr.store(u32::from(value), Ordering::Release);
                if value & RCR_CPU_RESET != 0 {
                    self.request(if value & RCR_FULL_RESET != 0 {
                        ResetKind::Cold
                    } else {
                        ResetKind::Warm
                    });
                }
            }
            PORT_KBD_COMMAND if value == KBD_PULSE_RESET => {
                self.request(ResetKind::KeyboardPulse);
            }
            // Any other keyboard-controller command: this machine has no 8042,
            // and dropping the write is what an absent controller does.
            _ => {}
        }
    }

    fn request(&self, kind: ResetKind) {
        // Only the first request of a boot is worth a line; the ladder means a
        // guest often pokes two of these before the host has restarted it.
        if !self.requested.swap(true, Ordering::AcqRel) {
            self.count.fetch_add(1, Ordering::AcqRel);
            tracing::info!(via = kind.as_str(), "guest requested a machine reset");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact write EDK2's `ResetCold()`/`ResetWarm()` performs
    /// (`OvmfPkg/Library/ResetSystemLib`): `IoWrite8 (0xCF9, BIT2 | BIT1)`.
    /// A UEFI guest's `reboot` arrives as EFI `ResetSystem`, which is this.
    #[test]
    fn the_edk2_reset_write_requests_a_reset() {
        let reset = ResetControl::new();
        assert!(!reset.is_reset_requested());
        reset.io_write(PORT_RESET_CONTROL, &[(1 << 2) | (1 << 1)]);
        assert!(reset.is_reset_requested());
        assert_eq!(reset.count(), 1);
    }

    /// Linux's `BOOT_CF9_*` path: read, mask, write `cf9|2`, then write
    /// `cf9|reboot_code` (0x0E cold / 0x06 warm). The first write must *not*
    /// trigger — `RST_CPU` is not in it — and the second must.
    #[test]
    fn the_linux_cf9_sequence_triggers_only_on_the_strobe() {
        for code in [0x0eu8, 0x06] {
            let reset = ResetControl::new();
            let mut probe = [0xaau8; 1];
            reset.io_read(PORT_RESET_CONTROL, &mut probe);
            let cf9 = probe[0] & !code;
            reset.io_write(PORT_RESET_CONTROL, &[cf9 | 2]);
            assert!(
                !reset.is_reset_requested(),
                "the arming write must not reset"
            );
            reset.io_write(PORT_RESET_CONTROL, &[cf9 | code]);
            assert!(reset.is_reset_requested(), "code {code:#x}");
        }
    }

    /// The ACPI `RESET_VALUE` the FADT publishes must be a value this register
    /// actually acts on, or `acpi_reboot()` writes into the void and Linux
    /// silently falls through to the next rung.
    #[test]
    fn the_published_acpi_reset_value_resets() {
        let reset = ResetControl::new();
        reset.io_write(PORT_RESET_CONTROL, &[ACPI_RESET_VALUE]);
        assert!(reset.is_reset_requested());
    }

    /// `reboot=k`, and the second half of EDK2's `ResetCold`.
    #[test]
    fn the_keyboard_controller_pulse_resets() {
        let reset = ResetControl::new();
        reset.io_write(PORT_KBD_COMMAND, &[KBD_PULSE_RESET]);
        assert!(reset.is_reset_requested());
        // Ten pulses in a row is what Linux actually writes; it is still one
        // reset.
        for _ in 0..9 {
            reset.io_write(PORT_KBD_COMMAND, &[KBD_PULSE_RESET]);
        }
        assert_eq!(reset.count(), 1);
    }

    /// Any other 8042 command is not a reset — a guest probing the controller
    /// must not reboot the machine by accident.
    #[test]
    fn other_keyboard_commands_do_nothing() {
        let reset = ResetControl::new();
        for command in [0x20u8, 0x60, 0xaa, 0xab, 0xad, 0xae, 0xd1, 0xff, 0x00] {
            reset.io_write(PORT_KBD_COMMAND, &[command]);
        }
        assert!(!reset.is_reset_requested());
    }

    /// The strobe is self-clearing: it must never read back set, or a guest
    /// that reads-modifies-writes the register resets itself on the next write.
    #[test]
    fn the_cpu_reset_strobe_reads_back_clear() {
        let reset = ResetControl::new();
        reset.io_write(PORT_RESET_CONTROL, &[0x0e]);
        let mut data = [0xffu8; 1];
        reset.io_read(PORT_RESET_CONTROL, &mut data);
        assert_eq!(data[0] & (1 << 2), 0, "RST_CPU must read back clear");
        assert_eq!(data[0], 0x0a, "SYS_RST and FULL_RST are retained");
    }

    /// The latch survives until the machine clears it, and clearing it puts the
    /// register back where a fresh boot finds it.
    #[test]
    fn clearing_the_latch_returns_the_register_to_power_on() {
        let reset = ResetControl::new();
        reset.io_write(PORT_RESET_CONTROL, &[ACPI_RESET_VALUE]);
        assert!(reset.is_reset_requested());
        reset.clear();
        assert!(!reset.is_reset_requested());
        let mut data = [0xffu8; 4];
        reset.io_read(PORT_RESET_CONTROL, &mut data);
        assert_eq!(data, [0, 0, 0, 0]);
        // The counter is history, not state: it survives the reset it caused.
        assert_eq!(reset.count(), 1);
    }

    /// A guest may issue any width at any port; nothing may index out of range
    /// or panic, and no port but the two claimed ones may do anything.
    #[test]
    fn out_of_range_and_oversized_accesses_are_harmless() {
        let reset = ResetControl::new();
        assert!(ResetControl::claims_port(PORT_RESET_CONTROL));
        assert!(!ResetControl::claims_port(PORT_KBD_COMMAND));
        assert!(ResetControl::claims_write(PORT_KBD_COMMAND));
        for port in [0x3f8u16, 0x600, 0x60, 0xcf8, 0xcfc, 0x70, 0xcfa, 0xcf8] {
            assert!(!ResetControl::claims_write(port), "{port:#x}");
        }
        let mut wide = [0xaau8; 32];
        reset.io_read(PORT_RESET_CONTROL, &mut wide);
        assert!(wide.iter().skip(1).all(|&b| b == 0));
        reset.io_write(PORT_RESET_CONTROL, &[]);
        reset.io_write(PORT_KBD_COMMAND, &[]);
        reset.io_read(0x0000, &mut wide);
        assert!(!reset.is_reset_requested());
    }
}
