//! The ACPI PM register block: the fixed-feature registers the FADT points at,
//! on 16 port-I/O addresses starting at [`layout::ACPI_PM_BASE`].
//!
//! Port map, register semantics and why the two pinned addresses are pinned are
//! documented in [`crate::layout`] (search for "ACPI PM register block"). In
//! short:
//!
//! * **`0x600` sleep control** — ACPI 5.0+ `SLEEP_CONTROL_REG`. EDK2 uses this
//!   one: for a CloudHv host bridge `ResetShutdown()` is literally
//!   `IoWrite8 (0x600, 5 << 2 | 1 << 5)`
//!   (`OvmfPkg/Library/ResetSystemLib/DxeResetShutdown.c`).
//! * **`0x606` PM1a control** — the legacy path. Linux is not in
//!   hardware-reduced mode here, so `acpi_power_off()` ends in ACPICA's
//!   `AcpiHwLegacySleep()`, which writes `SLP_TYP` from `\_S5` plus `SLP_EN`
//!   into `PM1a_CNT`.
//! * **`0x608` PM timer** — the free-running 3.579545 MHz counter EDK2's
//!   `MicroSecondDelay()` spins on. A stuck counter is an unbreakable firmware
//!   hang, which is why it is a real host-clock derivative and not a register.
//!
//! Either shutdown write sets a latch that [`AcpiPmBlock::is_shutdown_requested`]
//! reports; `vmm_core::ExitHandler::shutdown_requested` turns that into
//! `vmm_core::RunOutcome::Shutdown` on the next exit, so no run loop has to
//! know about ACPI.
//!
//! **The guest is untrusted.** Every access is decoded from the port offset and
//! clamped to the register it lands in; a 1-, 2- or 4-byte access at any
//! address in the block is answered, no access can index out of the block, and
//! nothing here can panic. A write with `SLP_TYP != 5` is logged and ignored
//! rather than treated as "some kind of shutdown".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crate::layout;

// ---- register offsets within the block -----------------------------------

const OFF_SLEEP_CONTROL: u16 = 0x00;
const OFF_SLEEP_STATUS: u16 = 0x01;
const OFF_PM1A_STS: u16 = 0x02;
const OFF_PM1A_EN: u16 = 0x04;
const OFF_PM1A_CNT: u16 = 0x06;
const OFF_PM_TIMER: u16 = 0x08;
const OFF_GPE0_STS: u16 = 0x0c;
const OFF_GPE0_EN: u16 = 0x0e;

/// `PM1_EVT_BLK` length (status + enable), as published in the FADT.
pub const PM1_EVT_LEN: u8 = 4;
/// `PM1_CNT_BLK` length.
pub const PM1_CNT_LEN: u8 = 2;
/// `PM_TMR_BLK` length.
pub const PM_TMR_LEN: u8 = 4;
/// `GPE0_BLK` length (status + enable).
pub const GPE0_BLK_LEN: u8 = 4;

/// Guest physical (I/O) address of `PM1a_EVT_BLK`.
pub const PM1A_EVT_PORT: u16 = layout::ACPI_PM_BASE + OFF_PM1A_STS;
/// Guest I/O address of `PM1a_CNT_BLK`.
pub const PM1A_CNT_PORT: u16 = layout::ACPI_PM_BASE + OFF_PM1A_CNT;
/// Guest I/O address of `PM_TMR_BLK` (`CLOUDHV_ACPI_TIMER_IO_ADDRESS`).
pub const PM_TIMER_PORT: u16 = layout::ACPI_PM_BASE + OFF_PM_TIMER;
/// Guest I/O address of `GPE0_BLK`.
pub const GPE0_PORT: u16 = layout::ACPI_PM_BASE + OFF_GPE0_STS;
/// Guest I/O address of `SLEEP_CONTROL_REG`
/// (`CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS`).
pub const SLEEP_CONTROL_PORT: u16 = layout::ACPI_PM_BASE + OFF_SLEEP_CONTROL;
/// Guest I/O address of `SLEEP_STATUS_REG`.
pub const SLEEP_STATUS_PORT: u16 = layout::ACPI_PM_BASE + OFF_SLEEP_STATUS;

// ---- register bits -------------------------------------------------------

/// `SCI_EN` in `PM1a_CNT`. Always reads back set: this machine has no SMI port
/// (`FADT.SMI_CMD` is 0), so ACPICA must conclude the platform is *already* in
/// ACPI mode. `AcpiHwGetMode()` decides that by reading exactly this bit.
const PM1_CNT_SCI_EN: u16 = 1 << 0;
/// `SLP_TYP` field of `PM1a_CNT`, bits 12:10.
const PM1_CNT_SLP_TYP_SHIFT: u32 = 10;
const PM1_CNT_SLP_TYP_MASK: u16 = 0x7 << PM1_CNT_SLP_TYP_SHIFT;
/// `SLP_EN` of `PM1a_CNT`: write-only, reads back zero.
const PM1_CNT_SLP_EN: u16 = 1 << 13;

/// `SLP_TYP` field of `SLEEP_CONTROL_REG`, bits 4:2.
const SLEEP_CTL_SLP_TYP_SHIFT: u32 = 2;
const SLEEP_CTL_SLP_TYP_MASK: u8 = 0x7 << SLEEP_CTL_SLP_TYP_SHIFT;
/// `SLP_EN` of `SLEEP_CONTROL_REG`: write-only, reads back zero.
const SLEEP_CTL_SLP_EN: u8 = 1 << 5;

/// `WAK_STS` of `SLEEP_STATUS_REG`, write-1-to-clear.
const SLEEP_STS_WAK: u8 = 1 << 7;

/// The sleep state that means "off". Must match the first element of the
/// DSDT's `\_S5` package, or ACPICA writes a different `SLP_TYP` than the block
/// watches for; `super::dsdt` builds that package from this constant.
pub const SLP_TYP_S5: u8 = 5;

/// The architectural ACPI PM timer frequency, 3.579545 MHz.
pub const ACPI_PM_TIMER_HZ: u64 = 3_579_545;

/// Width of the counter. ACPI allows 24 or 32 bits; 24 is what we advertise
/// (FADT `TMR_VAL_EXT` clear), so the counter must actually wrap there.
const ACPI_PM_TIMER_MASK: u32 = 0x00ff_ffff;

/// A free-running ACPI power-management timer, derived from host monotonic
/// time. Read-only: the guest cannot set it, only observe it advance.
#[derive(Debug)]
pub struct AcpiPmTimer {
    origin: Instant,
}

impl Default for AcpiPmTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpiPmTimer {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// Ticks since the VM started, wrapped to the counter width.
    pub fn ticks(&self) -> u32 {
        let micros = self.origin.elapsed().as_micros();
        // 3.579545 ticks per microsecond, in integer arithmetic. u128 keeps the
        // product exact for any plausible uptime; the mask does the wrapping,
        // which is what a real 24-bit counter does too.
        let ticks = micros.saturating_mul(u128::from(ACPI_PM_TIMER_HZ)) / 1_000_000;
        (ticks as u32) & ACPI_PM_TIMER_MASK
    }
}

/// The block's mutable registers. Small enough to sit behind one mutex; the
/// shutdown latch is deliberately *outside* it so the run loop's
/// `shutdown_requested()` never has to take a lock.
#[derive(Debug, Default)]
struct PmRegisters {
    pm1a_sts: u16,
    pm1a_en: u16,
    /// Only `SLP_TYP` is retained; `SLP_EN` is write-only and `SCI_EN` is
    /// synthesised on read.
    pm1a_cnt: u16,
    gpe0_sts: u16,
    gpe0_en: u16,
    sleep_control: u8,
    sleep_status: u8,
}

/// The ACPI fixed-feature register block.
#[derive(Debug)]
pub struct AcpiPmBlock {
    timer: AcpiPmTimer,
    regs: Mutex<PmRegisters>,
    shutdown: AtomicBool,
}

impl Default for AcpiPmBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpiPmBlock {
    pub fn new() -> Self {
        Self {
            timer: AcpiPmTimer::new(),
            regs: Mutex::new(PmRegisters::default()),
            shutdown: AtomicBool::new(false),
        }
    }

    /// True when `port` belongs to this block.
    pub fn contains(port: u16) -> bool {
        (layout::ACPI_PM_BASE..layout::ACPI_PM_BASE + layout::ACPI_PM_SIZE).contains(&port)
    }

    /// The PM timer behind this block, for `entangled doctor` and tests.
    pub fn ticks(&self) -> u32 {
        self.timer.ticks()
    }

    /// True once the guest has asked to power off through either sleep register.
    /// Latching: once set it stays set, so a vCPU that is not the one that did
    /// the write still sees it.
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// A poisoned lock means some other thread panicked while holding it; the
    /// register values are still structurally valid `u16`s, so recovering is
    /// strictly better than propagating a panic into a guest exit path.
    fn regs(&self) -> MutexGuard<'_, PmRegisters> {
        self.regs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Guest read. `data` may be any width; bytes past the end of the register
    /// the access starts in read as zero, which is what an unimplemented ACPI
    /// register does.
    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        data.fill(0);
        if !Self::contains(port) {
            return;
        }
        let offset = port - layout::ACPI_PM_BASE;
        for (i, byte) in data.iter_mut().enumerate() {
            let Ok(at) = u16::try_from(usize::from(offset) + i) else {
                return;
            };
            if at >= layout::ACPI_PM_SIZE {
                return;
            }
            *byte = self.read_byte(at);
        }
    }

    /// Guest write. Wider-than-a-register writes spill into the following
    /// register, exactly as a real I/O port block behaves.
    pub fn io_write(&self, port: u16, data: &[u8]) {
        if !Self::contains(port) {
            return;
        }
        let offset = port - layout::ACPI_PM_BASE;
        for (i, byte) in data.iter().enumerate() {
            let Ok(at) = u16::try_from(usize::from(offset) + i) else {
                return;
            };
            if at >= layout::ACPI_PM_SIZE {
                return;
            }
            self.write_byte(at, *byte);
        }
    }

    fn read_byte(&self, offset: u16) -> u8 {
        // The PM timer is the only register not behind the mutex.
        if (OFF_PM_TIMER..OFF_PM_TIMER + u16::from(PM_TMR_LEN)).contains(&offset) {
            let bytes = self.timer.ticks().to_le_bytes();
            return bytes[usize::from(offset - OFF_PM_TIMER)];
        }
        let regs = self.regs();
        let (value, base) = match offset {
            OFF_SLEEP_CONTROL => (u16::from(regs.sleep_control & !SLEEP_CTL_SLP_EN), offset),
            OFF_SLEEP_STATUS => (u16::from(regs.sleep_status), offset),
            OFF_PM1A_STS | 0x03 => (regs.pm1a_sts, OFF_PM1A_STS),
            OFF_PM1A_EN | 0x05 => (regs.pm1a_en, OFF_PM1A_EN),
            OFF_PM1A_CNT | 0x07 => (
                (regs.pm1a_cnt & !PM1_CNT_SLP_EN) | PM1_CNT_SCI_EN,
                OFF_PM1A_CNT,
            ),
            OFF_GPE0_STS | 0x0d => (regs.gpe0_sts, OFF_GPE0_STS),
            OFF_GPE0_EN | 0x0f => (regs.gpe0_en, OFF_GPE0_EN),
            _ => (0, offset),
        };
        value.to_le_bytes()[usize::from(offset - base)]
    }

    fn write_byte(&self, offset: u16, value: u8) {
        let mut regs = self.regs();
        match offset {
            OFF_SLEEP_CONTROL => {
                regs.sleep_control = value;
                if value & SLEEP_CTL_SLP_EN != 0 {
                    let slp_typ = (value & SLEEP_CTL_SLP_TYP_MASK) >> SLEEP_CTL_SLP_TYP_SHIFT;
                    drop(regs);
                    self.request_sleep(slp_typ, "SLEEP_CONTROL_REG");
                }
            }
            // WAK_STS is write-1-to-clear; every other bit is reserved.
            OFF_SLEEP_STATUS => regs.sleep_status &= !(value & SLEEP_STS_WAK),
            // Status registers are write-1-to-clear.
            OFF_PM1A_STS => regs.pm1a_sts &= !u16::from(value),
            0x03 => regs.pm1a_sts &= !(u16::from(value) << 8),
            OFF_PM1A_EN => regs.pm1a_en = (regs.pm1a_en & 0xff00) | u16::from(value),
            0x05 => regs.pm1a_en = (regs.pm1a_en & 0x00ff) | (u16::from(value) << 8),
            OFF_PM1A_CNT | 0x07 => {
                let shift = if offset == OFF_PM1A_CNT { 0 } else { 8 };
                let mask = 0xffu16 << shift;
                let composed = (regs.pm1a_cnt & !mask) | (u16::from(value) << shift);
                regs.pm1a_cnt = composed;
                if composed & PM1_CNT_SLP_EN != 0 {
                    let slp_typ =
                        ((composed & PM1_CNT_SLP_TYP_MASK) >> PM1_CNT_SLP_TYP_SHIFT) as u8;
                    drop(regs);
                    self.request_sleep(slp_typ, "PM1a_CNT");
                }
            }
            OFF_GPE0_STS => regs.gpe0_sts &= !u16::from(value),
            0x0d => regs.gpe0_sts &= !(u16::from(value) << 8),
            OFF_GPE0_EN => regs.gpe0_en = (regs.gpe0_en & 0xff00) | u16::from(value),
            0x0f => regs.gpe0_en = (regs.gpe0_en & 0x00ff) | (u16::from(value) << 8),
            // The PM timer is read-only; swallow writes rather than letting
            // them float off to an unclaimed port.
            _ => {}
        }
    }

    /// `SLP_EN` was written. Only S5 (soft off) is implemented; anything else is
    /// a sleep state this machine does not support and is refused loudly rather
    /// than silently treated as a power-off.
    fn request_sleep(&self, slp_typ: u8, via: &'static str) {
        if slp_typ == SLP_TYP_S5 {
            tracing::info!(via, "guest requested ACPI S5 (soft off)");
            self.shutdown.store(true, Ordering::Release);
        } else {
            tracing::warn!(
                via,
                slp_typ,
                "ignoring an unsupported ACPI sleep state (only S5 is implemented)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact write EDK2's `ResetShutdown()` performs for a CloudHv host
    /// bridge. If this stops meaning "power off", UEFI guests hang on exit.
    #[test]
    fn edk2_cloudhv_shutdown_write_powers_off() {
        let pm = AcpiPmBlock::new();
        assert!(!pm.is_shutdown_requested());
        pm.io_write(SLEEP_CONTROL_PORT, &[(5 << 2) | (1 << 5)]);
        assert!(pm.is_shutdown_requested());
    }

    /// The exact write ACPICA's `AcpiHwLegacySleep()` performs: `SLP_TYP` from
    /// `\_S5` in bits 12:10 plus `SLP_EN`, as one 16-bit access.
    #[test]
    fn linux_pm1a_cnt_shutdown_write_powers_off() {
        let pm = AcpiPmBlock::new();
        let value: u16 = (u16::from(SLP_TYP_S5) << 10) | (1 << 13);
        pm.io_write(PM1A_CNT_PORT, &value.to_le_bytes());
        assert!(pm.is_shutdown_requested());
    }

    /// ACPICA sets `SLP_TYP` first and `SLP_EN` in a second write; a byte-wide
    /// pair of writes must compose into the same request.
    #[test]
    fn byte_wide_writes_compose_into_one_request() {
        let pm = AcpiPmBlock::new();
        let value: u16 = (u16::from(SLP_TYP_S5) << 10) | (1 << 13);
        pm.io_write(PM1A_CNT_PORT, &[value as u8]);
        assert!(!pm.is_shutdown_requested(), "SLP_EN is in the high byte");
        pm.io_write(PM1A_CNT_PORT + 1, &[(value >> 8) as u8]);
        assert!(pm.is_shutdown_requested());
    }

    #[test]
    fn sleep_types_other_than_s5_are_refused() {
        for slp_typ in [0u16, 1, 3, 4, 6, 7] {
            let pm = AcpiPmBlock::new();
            let value = (slp_typ << 10) | (1 << 13);
            pm.io_write(PM1A_CNT_PORT, &value.to_le_bytes());
            assert!(
                !pm.is_shutdown_requested(),
                "S{slp_typ} must not power the VM off"
            );
        }
        // …and neither does SLP_TYP=5 without SLP_EN.
        let pm = AcpiPmBlock::new();
        pm.io_write(PM1A_CNT_PORT, &(5u16 << 10).to_le_bytes());
        assert!(!pm.is_shutdown_requested());
    }

    /// With no SMI command port, ACPICA decides "already in ACPI mode" purely
    /// from `SCI_EN`. If this reads back zero, `AcpiEnable()` fails with
    /// "No SMI_CMD in FADT, mode transition failed" and ACPI never comes up.
    #[test]
    fn pm1a_cnt_reads_back_sci_en_and_never_slp_en() {
        let pm = AcpiPmBlock::new();
        let mut data = [0u8; 2];
        pm.io_read(PM1A_CNT_PORT, &mut data);
        assert_eq!(u16::from_le_bytes(data) & 1, 1, "SCI_EN must read set");

        pm.io_write(PM1A_CNT_PORT, &((5u16 << 10) | (1 << 13)).to_le_bytes());
        pm.io_read(PM1A_CNT_PORT, &mut data);
        let value = u16::from_le_bytes(data);
        assert_eq!(value & (1 << 13), 0, "SLP_EN is write-only");
        assert_eq!(value & (0x7 << 10), 5 << 10, "SLP_TYP is retained");
    }

    #[test]
    fn pm1a_status_is_write_one_to_clear_and_enable_is_readwrite() {
        let pm = AcpiPmBlock::new();
        // Nothing sets status bits in this machine yet, so start from zero and
        // prove a clear cannot *set* anything.
        pm.io_write(PM1A_EVT_PORT, &0xffffu16.to_le_bytes());
        let mut data = [0xffu8; 2];
        pm.io_read(PM1A_EVT_PORT, &mut data);
        assert_eq!(u16::from_le_bytes(data), 0);

        pm.io_write(PM1A_EVT_PORT + 2, &0x0301u16.to_le_bytes());
        pm.io_read(PM1A_EVT_PORT + 2, &mut data);
        assert_eq!(u16::from_le_bytes(data), 0x0301, "PM1a_EN is read/write");
    }

    #[test]
    fn gpe0_registers_round_trip() {
        let pm = AcpiPmBlock::new();
        pm.io_write(GPE0_PORT + 2, &0x00ffu16.to_le_bytes());
        let mut data = [0u8; 2];
        pm.io_read(GPE0_PORT + 2, &mut data);
        assert_eq!(u16::from_le_bytes(data), 0x00ff);
    }

    /// `MicroSecondDelay()` spins until the timer has advanced far enough — a
    /// stuck counter hangs the firmware forever, so this is load-bearing.
    #[test]
    fn acpi_pm_timer_advances_monotonically() {
        let pm = AcpiPmBlock::new();
        let first = pm.ticks();
        let mut last = first;
        let deadline = Instant::now() + std::time::Duration::from_millis(50);
        while Instant::now() < deadline {
            let now = pm.ticks();
            assert!(now >= last, "timer went backwards: {last} -> {now}");
            last = now;
        }
        assert!(
            last > first,
            "timer did not advance in 50 ms (expected ~{} ticks)",
            ACPI_PM_TIMER_HZ / 20
        );
        assert_eq!(last & !ACPI_PM_TIMER_MASK, 0, "never wider than 24 bits");
    }

    #[test]
    fn acpi_pm_timer_supports_partial_reads_and_ignores_writes() {
        let pm = AcpiPmBlock::new();
        let mut dword = [0u8; 4];
        pm.io_read(PM_TIMER_PORT, &mut dword);
        assert_eq!(dword[3], 0, "24-bit counter: top byte is always zero");
        let mut byte = [0xffu8; 1];
        pm.io_read(PM_TIMER_PORT + 3, &mut byte);
        assert_eq!(byte[0], 0);
        // A write must neither stick nor request a shutdown.
        pm.io_write(PM_TIMER_PORT, &0xffff_ffffu32.to_le_bytes());
        assert!(!pm.is_shutdown_requested());
    }

    /// A guest can issue any width at any port; nothing may index outside the
    /// block or panic.
    #[test]
    fn out_of_range_and_oversized_accesses_are_harmless() {
        let pm = AcpiPmBlock::new();
        assert!(!AcpiPmBlock::contains(layout::ACPI_PM_BASE - 1));
        assert!(AcpiPmBlock::contains(layout::ACPI_PM_BASE));
        assert!(AcpiPmBlock::contains(
            layout::ACPI_PM_BASE + layout::ACPI_PM_SIZE - 1
        ));
        assert!(!AcpiPmBlock::contains(
            layout::ACPI_PM_BASE + layout::ACPI_PM_SIZE
        ));

        let mut wide = [0xaau8; 32];
        pm.io_read(layout::ACPI_PM_BASE + layout::ACPI_PM_SIZE - 1, &mut wide);
        assert!(
            wide.iter().all(|&b| b == 0),
            "reads past the block are zero"
        );
        pm.io_write(layout::ACPI_PM_BASE + layout::ACPI_PM_SIZE - 1, &wide);
        pm.io_read(0x0000, &mut wide);
        pm.io_write(0xffff, &[0xff]);
        assert!(!pm.is_shutdown_requested());
    }

    /// The whole block must fit between its base and the next claimed port
    /// range, and the two EDK2-pinned addresses must land where EDK2 expects.
    #[test]
    fn port_map_matches_the_edk2_contract() {
        assert_eq!(
            SLEEP_CONTROL_PORT, 0x0600,
            "CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS"
        );
        assert_eq!(PM_TIMER_PORT, 0x0608, "CLOUDHV_ACPI_TIMER_IO_ADDRESS");
        assert_eq!(layout::ACPI_PM_SIZE, 0x10);
        // No overlap between the sub-registers.
        let mut claimed = [0u8; 0x10];
        for (offset, len) in [
            (OFF_SLEEP_CONTROL, 1u16),
            (OFF_SLEEP_STATUS, 1),
            (OFF_PM1A_STS, 2),
            (OFF_PM1A_EN, 2),
            (OFF_PM1A_CNT, 2),
            (OFF_PM_TIMER, 4),
            (OFF_GPE0_STS, 2),
            (OFF_GPE0_EN, 2),
        ] {
            for i in 0..len {
                let at = usize::from(offset + i);
                assert_eq!(claimed[at], 0, "offset {at:#x} claimed twice");
                claimed[at] = 1;
            }
        }
        assert!(claimed.iter().all(|&c| c == 1), "gap in the block");
    }
}
