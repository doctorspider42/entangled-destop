//! MC146818-compatible RTC/CMOS at the classic index/data ports 0x70/0x71
//! (backlog UEFI-1802).
//!
//! A UEFI firmware must implement `EFI_RUNTIME_SERVICES.GetTime()`, and on a PC
//! platform that is `PcAtChipsetPkg/PcatRealTimeClockRuntimeDxe` talking to this
//! device. Without it `PcRtcInit()` spins in `RtcWaitToUpdate()` until it times
//! out and returns `EFI_DEVICE_ERROR`, which is a hard failure in the driver's
//! entry point.
//!
//! Two details of EDK2's accessor are load-bearing and easy to get wrong:
//!
//! * `IoRtcRead()` *reads* the index port to preserve its NMI-disable bit
//!   before OR-ing in the register number, so port 0x70 must read back the
//!   value last written. A floating `0xff` there turns every subsequent
//!   register number into `reg | 0x80` and the driver reads nonsense.
//! * Register A's UIP bit must be clear (we are never mid-update — the time is
//!   computed on demand) and register D's VRT bit must be set, or the driver
//!   concludes the battery is dead.
//!
//! The clock is read-only towards the guest: guest writes to the time
//! registers are accepted and dropped rather than being allowed to reach the
//! host clock. The guest can freely use the CMOS scratch RAM above the
//! register block, which is what firmware expects of it.

use std::time::{SystemTime, UNIX_EPOCH};

/// CMOS/RTC index port. Bit 7 is the NMI-disable line, not part of the address.
pub const RTC_INDEX_PORT: u16 = 0x70;
/// CMOS/RTC data port.
pub const RTC_DATA_PORT: u16 = 0x71;

/// Bit 7 of the index port: NMI disable, not part of the register number.
const NMI_DISABLE: u8 = 0x80;
/// Mask selecting the register number out of the index port.
const INDEX_MASK: u8 = !NMI_DISABLE;

// Register numbers (MC146818).
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY_OF_WEEK: u8 = 0x06;
const REG_DAY_OF_MONTH: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_A: u8 = 0x0a;
const REG_B: u8 = 0x0b;
const REG_C: u8 = 0x0c;
const REG_D: u8 = 0x0d;
/// De-facto standard century register (as used by ACPI's FADT century field —
/// `crate::acpi` publishes this index there, so the two cannot drift).
pub const REG_CENTURY: u8 = 0x32;

/// Register B bits.
const REG_B_DSE: u8 = 1 << 0; // daylight saving enable
const REG_B_24H: u8 = 1 << 1; // 1 = 24-hour mode
const REG_B_BINARY: u8 = 1 << 2; // 1 = binary, 0 = BCD

/// Register D bit 7: valid RAM and time. Must be set or firmware treats the
/// clock as dead.
const REG_D_VRT: u8 = 1 << 7;

/// Power-on register B: 24-hour mode, BCD encoding — the combination every PC
/// firmware assumes when it has no NVRAM to remember otherwise.
const DEFAULT_REG_B: u8 = REG_B_24H;

/// Size of the CMOS register file.
const CMOS_SIZE: usize = 0x80;

/// A broken-down UTC time, the way the RTC reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 1 = Sunday, per the MC146818 convention.
    pub weekday: u8,
}

/// Converts Unix time to a UTC calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole range of
/// `i64` days and needs no lookup tables or leap-year special cases.
pub fn civil_from_unix(unix_seconds: i64) -> RtcTime {
    let days = unix_seconds.div_euclid(86_400);
    let secs_of_day = unix_seconds.rem_euclid(86_400);

    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..=365, March-based
    let mp = (5 * doy + 2) / 153; // 0..=11, 0 = March
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    // 1970-01-01 was a Thursday; the MC146818 numbers Sunday as 1.
    let weekday = ((days + 4).rem_euclid(7) + 1) as u8;

    RtcTime {
        year: year.clamp(0, u16::MAX as i64) as u16,
        month: m as u8,
        day: d as u8,
        hour: (secs_of_day / 3600) as u8,
        minute: ((secs_of_day % 3600) / 60) as u8,
        second: (secs_of_day % 60) as u8,
        weekday,
    }
}

fn to_bcd(value: u8) -> u8 {
    ((value / 10) << 4) | (value % 10)
}

/// The RTC/CMOS device.
#[derive(Debug)]
pub struct Rtc {
    /// Last value written to the index port, NMI bit included.
    index: u8,
    /// Register file for everything that is plain CMOS RAM.
    cmos: [u8; CMOS_SIZE],
    /// Source of the wall clock, replaceable in tests.
    now: fn() -> i64,
}

impl Default for Rtc {
    fn default() -> Self {
        Self::new()
    }
}

impl Rtc {
    pub fn new() -> Self {
        Self::with_clock(unix_now)
    }

    /// Test constructor with a fixed clock.
    pub fn with_clock(now: fn() -> i64) -> Self {
        let mut cmos = [0u8; CMOS_SIZE];
        cmos[REG_A as usize] = 0x26; // divider on, 1024 Hz rate; UIP clear
        cmos[REG_B as usize] = DEFAULT_REG_B;
        cmos[REG_D as usize] = REG_D_VRT;
        Self {
            index: 0,
            cmos,
            now,
        }
    }

    pub fn contains(port: u16) -> bool {
        port == RTC_INDEX_PORT || port == RTC_DATA_PORT
    }

    fn register(&self) -> u8 {
        self.index & INDEX_MASK
    }

    fn reg_b(&self) -> u8 {
        self.cmos[REG_B as usize]
    }

    /// Encodes a time field per register B: binary or BCD.
    fn encode(&self, value: u8) -> u8 {
        if self.reg_b() & REG_B_BINARY != 0 {
            value
        } else {
            to_bcd(value)
        }
    }

    /// Encodes the hour, honouring both the encoding and 12/24-hour mode.
    fn encode_hour(&self, hour24: u8) -> u8 {
        if self.reg_b() & REG_B_24H != 0 {
            return self.encode(hour24);
        }
        let pm = hour24 >= 12;
        let hour12 = match hour24 % 12 {
            0 => 12,
            h => h,
        };
        // In 12-hour mode bit 7 of the hour register marks PM, and it sits
        // outside the BCD digits.
        self.encode(hour12) | if pm { 0x80 } else { 0 }
    }

    /// Value the guest reads from the currently selected register.
    fn read_register(&self) -> u8 {
        let time = civil_from_unix((self.now)());
        match self.register() {
            REG_SECONDS => self.encode(time.second),
            REG_MINUTES => self.encode(time.minute),
            REG_HOURS => self.encode_hour(time.hour),
            REG_DAY_OF_WEEK => self.encode(time.weekday),
            REG_DAY_OF_MONTH => self.encode(time.day),
            REG_MONTH => self.encode(time.month),
            REG_YEAR => self.encode((time.year % 100) as u8),
            REG_CENTURY => self.encode((time.year / 100) as u8),
            // UIP (bit 7) is always clear: the time is computed at read time,
            // so there is no update window to be inside of.
            REG_A => self.cmos[REG_A as usize] & 0x7f,
            REG_C => 0, // no periodic/alarm/update interrupts are generated
            REG_D => self.cmos[REG_D as usize] | REG_D_VRT,
            other => self.cmos[other as usize],
        }
    }

    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        let value = if port == RTC_INDEX_PORT {
            // EDK2 reads this back to preserve the NMI-disable bit.
            self.index
        } else {
            self.read_register()
        };
        // Byte port: a wider access reads the same value, as an 8-bit device
        // on a 16/32-bit bus does for the low lane and floats the rest.
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = if i == 0 { value } else { 0xff };
        }
    }

    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        let Some(&value) = data.first() else {
            return;
        };
        if port == RTC_INDEX_PORT {
            self.index = value;
            return;
        }
        match self.register() {
            // The clock itself is not settable: the guest is untrusted and the
            // host clock is not its to change. Accepted and dropped, which is
            // what the firmware's "set the time" fallback path tolerates.
            REG_SECONDS | REG_MINUTES | REG_HOURS | REG_DAY_OF_WEEK | REG_DAY_OF_MONTH
            | REG_MONTH | REG_YEAR | REG_CENTURY => {
                tracing::trace!(
                    register = format_args!("{:#02x}", self.register()),
                    "dropping guest RTC time write"
                );
            }
            // Register B selects the encoding and 12/24-hour mode; honour it,
            // but never let the guest enable interrupts we do not deliver.
            REG_B => {
                self.cmos[REG_B as usize] = value & (REG_B_DSE | REG_B_24H | REG_B_BINARY);
            }
            REG_A => {
                // Rate-select and divider bits are storage; UIP is read-only.
                self.cmos[REG_A as usize] = value & 0x7f;
            }
            REG_C => {} // read-only flag register
            REG_D => self.cmos[REG_D as usize] = value | REG_D_VRT,
            other => self.cmos[other as usize] = value,
        }
    }
}

fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Before 1970: the host clock is nonsense, but the guest still needs a
        // valid-looking date rather than a panic.
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-19T01:02:03Z — a Wednesday.
    const FIXED: i64 = 1_787_101_323;

    fn fixed_clock() -> i64 {
        FIXED
    }

    fn rtc() -> Rtc {
        Rtc::with_clock(fixed_clock)
    }

    /// Reads a register the way EDK2's `IoRtcRead()` does: read the index port,
    /// OR in the register number keeping bit 7, write it back, read data.
    fn edk2_read(rtc: &mut Rtc, register: u8) -> u8 {
        let mut index = [0u8; 1];
        rtc.io_read(RTC_INDEX_PORT, &mut index);
        rtc.io_write(RTC_INDEX_PORT, &[register | (index[0] & NMI_DISABLE)]);
        let mut data = [0u8; 1];
        rtc.io_read(RTC_DATA_PORT, &mut data);
        data[0]
    }

    #[test]
    fn civil_conversion_matches_known_dates() {
        let epoch = civil_from_unix(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!(epoch.weekday, 5, "1970-01-01 was a Thursday (Sunday = 1)");
        assert_eq!((epoch.hour, epoch.minute, epoch.second), (0, 0, 0));

        let t = civil_from_unix(FIXED);
        assert_eq!((t.year, t.month, t.day), (2026, 8, 19));
        assert_eq!((t.hour, t.minute, t.second), (1, 2, 3));
        assert_eq!(t.weekday, 4, "2026-08-19 is a Wednesday");

        // Leap day, and the day after a century non-leap-year boundary.
        let leap = civil_from_unix(951_782_400); // 2000-02-29T00:00:00Z
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
        let mar = civil_from_unix(1_583_020_800); // 2020-03-01T00:00:00Z
        assert_eq!((mar.year, mar.month, mar.day), (2020, 3, 1));
        // Pre-epoch times must not panic or wrap.
        let old = civil_from_unix(-1);
        assert_eq!((old.year, old.month, old.day), (1969, 12, 31));
        assert_eq!((old.hour, old.minute, old.second), (23, 59, 59));
    }

    /// The exact read sequence PcRtcInit performs, in BCD 24-hour mode.
    #[test]
    fn reports_a_valid_bcd_time_to_edk2() {
        let mut rtc = rtc();
        assert_eq!(edk2_read(&mut rtc, REG_B), DEFAULT_REG_B, "24-hour BCD");
        assert_eq!(edk2_read(&mut rtc, REG_SECONDS), 0x03);
        assert_eq!(edk2_read(&mut rtc, REG_MINUTES), 0x02);
        assert_eq!(edk2_read(&mut rtc, REG_HOURS), 0x01);
        assert_eq!(edk2_read(&mut rtc, REG_DAY_OF_MONTH), 0x19);
        assert_eq!(edk2_read(&mut rtc, REG_MONTH), 0x08);
        assert_eq!(edk2_read(&mut rtc, REG_YEAR), 0x26);
        assert_eq!(edk2_read(&mut rtc, REG_CENTURY), 0x20);
    }

    /// `RtcWaitToUpdate()` loops until UIP is clear *and* VRT is set; both are
    /// the difference between a working clock and EFI_DEVICE_ERROR.
    #[test]
    fn uip_is_always_clear_and_vrt_always_set() {
        let mut rtc = rtc();
        assert_eq!(edk2_read(&mut rtc, REG_A) & 0x80, 0, "UIP must be clear");
        assert_ne!(edk2_read(&mut rtc, REG_D) & REG_D_VRT, 0, "VRT must be set");
        // Even if the guest tries to clear VRT.
        rtc.io_write(RTC_INDEX_PORT, &[REG_D]);
        rtc.io_write(RTC_DATA_PORT, &[0x00]);
        assert_ne!(edk2_read(&mut rtc, REG_D) & REG_D_VRT, 0);
        // Or set UIP.
        rtc.io_write(RTC_INDEX_PORT, &[REG_A]);
        rtc.io_write(RTC_DATA_PORT, &[0xff]);
        assert_eq!(edk2_read(&mut rtc, REG_A) & 0x80, 0);
    }

    /// The index port must read back, or EDK2's `Address | (IoRead8 (index) &
    /// 0x80)` corrupts every register number.
    #[test]
    fn index_port_reads_back_and_preserves_the_nmi_bit() {
        let mut rtc = rtc();
        rtc.io_write(RTC_INDEX_PORT, &[NMI_DISABLE | REG_YEAR]);
        let mut index = [0u8; 1];
        rtc.io_read(RTC_INDEX_PORT, &mut index);
        assert_eq!(index[0], NMI_DISABLE | REG_YEAR);
        // With NMI disabled, the EDK2 sequence still selects the right register.
        assert_eq!(edk2_read(&mut rtc, REG_MONTH), 0x08);
        rtc.io_read(RTC_INDEX_PORT, &mut index);
        assert_eq!(index[0] & NMI_DISABLE, NMI_DISABLE, "NMI bit must survive");
    }

    #[test]
    fn binary_and_twelve_hour_modes_are_honoured() {
        let mut rtc = rtc();
        // Binary, 24-hour.
        rtc.io_write(RTC_INDEX_PORT, &[REG_B]);
        rtc.io_write(RTC_DATA_PORT, &[REG_B_24H | REG_B_BINARY]);
        assert_eq!(edk2_read(&mut rtc, REG_YEAR), 26);
        assert_eq!(edk2_read(&mut rtc, REG_DAY_OF_MONTH), 19);

        // BCD, 12-hour: 01:02 is AM, so no PM bit.
        rtc.io_write(RTC_INDEX_PORT, &[REG_B]);
        rtc.io_write(RTC_DATA_PORT, &[0]);
        assert_eq!(edk2_read(&mut rtc, REG_HOURS), 0x01);

        // 13:00 in 12-hour BCD is 0x01 with the PM bit.
        let mut afternoon = Rtc::with_clock(|| 1_787_144_400); // 2026-08-19T13:00:00Z
        afternoon.io_write(RTC_INDEX_PORT, &[REG_B]);
        afternoon.io_write(RTC_DATA_PORT, &[0]);
        assert_eq!(edk2_read(&mut afternoon, REG_HOURS), 0x81);
        // And 0x13 in 24-hour BCD.
        afternoon.io_write(RTC_INDEX_PORT, &[REG_B]);
        afternoon.io_write(RTC_DATA_PORT, &[REG_B_24H]);
        assert_eq!(edk2_read(&mut afternoon, REG_HOURS), 0x13);
    }

    #[test]
    fn guest_cannot_move_the_clock_but_can_use_cmos_ram() {
        let mut rtc = rtc();
        rtc.io_write(RTC_INDEX_PORT, &[REG_YEAR]);
        rtc.io_write(RTC_DATA_PORT, &[0x99]);
        assert_eq!(edk2_read(&mut rtc, REG_YEAR), 0x26, "clock must not move");

        // Scratch RAM above the register block is ordinary storage.
        for reg in [0x0e, 0x20, 0x7f] {
            rtc.io_write(RTC_INDEX_PORT, &[reg]);
            rtc.io_write(RTC_DATA_PORT, &[0x5a]);
            assert_eq!(edk2_read(&mut rtc, reg), 0x5a, "CMOS RAM {reg:#x}");
        }
    }

    /// A real MC146818 advances; a stuck clock breaks firmware stall loops.
    #[test]
    fn clock_follows_the_host() {
        let mut early = Rtc::with_clock(|| FIXED);
        let mut late = Rtc::with_clock(|| FIXED + 3600);
        assert_eq!(edk2_read(&mut early, REG_HOURS), 0x01);
        assert_eq!(edk2_read(&mut late, REG_HOURS), 0x02);
    }

    #[test]
    fn only_the_two_cmos_ports_are_claimed() {
        assert!(Rtc::contains(0x70));
        assert!(Rtc::contains(0x71));
        for port in [0x6f, 0x72, 0x3f8, 0x608, 0xcf8] {
            assert!(!Rtc::contains(port), "port {port:#x}");
        }
    }
}
