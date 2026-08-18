//! Device status field bits (VirtIO spec 1.2, section 2.1).

pub const ACKNOWLEDGE: u32 = 1;
pub const DRIVER: u32 = 2;
pub const DRIVER_OK: u32 = 4;
pub const FEATURES_OK: u32 = 8;
pub const DEVICE_NEEDS_RESET: u32 = 64;
pub const FAILED: u32 = 128;

/// Checks that a guest status write only adds bits in the legal order
/// (ACKNOWLEDGE → DRIVER → FEATURES_OK → DRIVER_OK). Writing 0 requests a
/// device reset and is always legal.
pub fn write_is_valid(current: u32, new: u32) -> bool {
    if new == 0 {
        return true; // reset request
    }
    // The driver must never clear bits other than via reset.
    if current & !new != 0 {
        return false;
    }
    let added = new & !current;
    // Bits may only be added on top of all previous stages.
    let stages = [ACKNOWLEDGE, DRIVER, FEATURES_OK, DRIVER_OK];
    let mut required = 0;
    for stage in stages {
        if added & stage != 0 && current & required != required {
            return false;
        }
        required |= stage;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_bringup_sequence() {
        let mut s = 0;
        for next in [
            ACKNOWLEDGE,
            ACKNOWLEDGE | DRIVER,
            ACKNOWLEDGE | DRIVER | FEATURES_OK,
            ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK,
        ] {
            assert!(write_is_valid(s, next), "{s:#b} -> {next:#b}");
            s = next;
        }
    }

    #[test]
    fn reset_always_allowed() {
        assert!(write_is_valid(ACKNOWLEDGE | DRIVER | DRIVER_OK, 0));
    }

    #[test]
    fn cannot_skip_stages_or_clear_bits() {
        assert!(!write_is_valid(0, DRIVER_OK));
        assert!(!write_is_valid(ACKNOWLEDGE, ACKNOWLEDGE | DRIVER_OK));
        assert!(!write_is_valid(ACKNOWLEDGE | DRIVER, ACKNOWLEDGE));
    }
}
