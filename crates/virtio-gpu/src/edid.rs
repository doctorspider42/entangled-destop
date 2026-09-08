//! A minimal, valid EDID 1.4 base block for the virtual display (MVP-811).
//!
//! Why the device needs one at all: with `VIRTIO_GPU_F_EDID` negotiated, the
//! Linux driver asks for the EDID and DRM builds the connector's mode list
//! from it (falling back to `GET_DISPLAY_INFO` only for the current mode).
//! GNOME/mutter then names, sizes and scales its outputs from the same block —
//! an output with no EDID shows up as "Unknown Display" with whatever single
//! mode the driver invented.
//!
//! What is deliberately *not* here: extension blocks, audio, HDR metadata,
//! detailed range limits. One base block with the current scanout resolution
//! as its preferred detailed timing is exactly what a virtual monitor needs,
//! and everything in it is checksummed, so a mistake fails loudly in the
//! guest (`drm: EDID checksum is invalid`) rather than subtly.
//!
//! The timing numbers are CVT-reduced-blanking-shaped (a fixed 160-pixel
//! horizontal and 45-line vertical blank). Nothing scans out a real cable, so
//! the exact porches are cosmetic — what matters is that the pixel clock stays
//! inside the field's 655.35 MHz ceiling, which this blanking satisfies up to
//! 8K-wide modes.
//!
//! The **refresh rate is not cosmetic**, though it looks it: the guest's
//! compositor schedules its frames against the number in this block, and
//! presents in the next advertised period when a frame's work does not fit in
//! one. That is why `[display] refresh_hz` exists and why the frame-pacing
//! counters are defined against the same value (GAME-2105, ADR-0004).

/// One EDID base block.
pub const EDID_BLOCK_LEN: usize = 128;

/// Horizontal blanking added to every mode (pixels): CVT-RB's fixed value.
const H_BLANK: u32 = 160;
/// Horizontal sync offset (front porch) and width, inside [`H_BLANK`].
const H_SYNC_OFFSET: u32 = 48;
const H_SYNC_WIDTH: u32 = 32;
/// Vertical blanking added to every mode (lines).
const V_BLANK: u32 = 45;
/// Vertical sync offset (front porch) and width, inside [`V_BLANK`].
const V_SYNC_OFFSET: u32 = 3;
const V_SYNC_WIDTH: u32 = 5;

/// The refresh rate a profile gets when it does not ask for one — what a
/// physical monitor of this size would report, and what every other VMM
/// advertises.
pub const DEFAULT_REFRESH_HZ: u32 = 60;

/// Bounds on the advertised refresh (`[display] refresh_hz`).
///
/// The floor keeps the number meaningful; the ceiling is where a 1080p mode's
/// pixel clock starts crowding the descriptor's 655.35 MHz field
/// (2080 × 1125 × 240 Hz = 561.6 MHz), so past it [`edid_block`] would start
/// refusing ordinary resolutions. Both ends are checked by
/// `control_api::VmConfig::validate` so the guest never sees a bad block.
pub const MIN_REFRESH_HZ: u32 = 24;
pub const MAX_REFRESH_HZ: u32 = 240;

/// Largest active size a detailed timing descriptor can express: 12 bits per
/// axis. Also comfortably past [`crate::MAX_RESOURCE_PIXELS`]'s 4096-wide cap.
const MAX_DTD_ACTIVE: u32 = 4095;

/// Builds the 128-byte EDID base block whose preferred (and only) detailed
/// timing is `width`×`height` at `refresh_hz`.
///
/// The refresh rate is not cosmetic: nothing scans out a cable here, but the
/// guest's compositor schedules against it. A guest whose per-frame work does
/// not fit in one advertised period presents in the *next* one, so the
/// advertised rate is the quantum the guest's frame rate is a fraction of
/// (GAME-2105, ADR-0004).
///
/// Returns `None` when the mode cannot be encoded: zero-sized, wider/taller
/// than a detailed timing descriptor's 12-bit fields, a zero refresh, or a
/// pixel clock past the descriptor's 655.35 MHz ceiling. The device answers
/// `ERR_UNSPEC` for those rather than shipping a corrupt block.
pub fn edid_block(width: u32, height: u32, refresh_hz: u32) -> Option<[u8; EDID_BLOCK_LEN]> {
    if width == 0 || height == 0 || width > MAX_DTD_ACTIVE || height > MAX_DTD_ACTIVE {
        return None;
    }
    if refresh_hz == 0 {
        return None;
    }
    // In 10 kHz units, the descriptor's own unit. The u16 ceiling (655.35 MHz)
    // caps the total mode at ~10.9 Mpixels at 60 Hz — comfortably past 4K
    // (3840×2160 needs 529.2 MHz), refused for the degenerate extremes.
    let clock_10khz =
        u64::from(width + H_BLANK) * u64::from(height + V_BLANK) * u64::from(refresh_hz) / 10_000;
    let clock_10khz = u16::try_from(clock_10khz).ok()?;

    let mut e = [0u8; EDID_BLOCK_LEN];

    // Header.
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer "ETG" (Entangled): three letters, A=1, five bits each,
    // packed big-endian into two bytes.
    let mfg = pack_manufacturer(b"ETG");
    e[8] = (mfg >> 8) as u8;
    e[9] = mfg as u8;
    // Product code 0x0001, serial 0.
    e[10] = 0x01;
    // Week 0 (unspecified), year 2024 (byte = year - 1990) — a fixed date, so
    // the block (and any golden screenshot metadata) is reproducible.
    e[17] = 34;
    // EDID 1.4.
    e[18] = 1;
    e[19] = 4;
    // Video input: digital, 8 bits per color, interface undefined.
    e[20] = 0xA0;
    // Screen size unknown (projector/virtual): 0, 0. Gamma 2.2.
    e[23] = 120;
    // Features: sRGB default color space, preferred timing is native.
    e[24] = 0x06;
    // Chromaticity: the sRGB primaries (the same ten bytes every sRGB monitor
    // block carries).
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // No established timings (bytes 35..38 stay 0); standard timings unused.
    for slot in e[38..54].chunks_exact_mut(2) {
        slot.copy_from_slice(&[0x01, 0x01]);
    }

    // Descriptor 1 (bytes 54..72): the preferred detailed timing.
    let h_active = width;
    let v_active = height;
    let d = &mut e[54..72];
    d[0] = clock_10khz as u8;
    d[1] = (clock_10khz >> 8) as u8;
    d[2] = h_active as u8;
    d[3] = H_BLANK as u8;
    d[4] = (((h_active >> 8) as u8) << 4) | ((H_BLANK >> 8) as u8 & 0x0F);
    d[5] = v_active as u8;
    d[6] = V_BLANK as u8;
    d[7] = (((v_active >> 8) as u8) << 4) | ((V_BLANK >> 8) as u8 & 0x0F);
    d[8] = H_SYNC_OFFSET as u8;
    d[9] = H_SYNC_WIDTH as u8;
    d[10] = (((V_SYNC_OFFSET & 0x0F) as u8) << 4) | (V_SYNC_WIDTH & 0x0F) as u8;
    d[11] = (((H_SYNC_OFFSET >> 8) as u8 & 0x3) << 6)
        | (((H_SYNC_WIDTH >> 8) as u8 & 0x3) << 4)
        | (((V_SYNC_OFFSET >> 4) as u8 & 0x3) << 2)
        | ((V_SYNC_WIDTH >> 4) as u8 & 0x3);
    // Image size in mm: derived at ~96 DPI so DRM reports a plausible
    // physical size (some desktops divide by it for scaling decisions).
    let mm = |px: u32| px * 254 / 960;
    let (w_mm, h_mm) = (mm(width), mm(height));
    d[12] = w_mm as u8;
    d[13] = h_mm as u8;
    d[14] = (((w_mm >> 8) as u8 & 0x0F) << 4) | ((h_mm >> 8) as u8 & 0x0F);
    // No borders; digital separate sync, both polarities positive.
    d[17] = 0x1E;

    // Descriptor 2 (72..90): display product name, 0xFC.
    write_text_descriptor(&mut e[72..90], 0xFC, b"Entangled");
    // Descriptors 3 and 4 (90..126): dummy descriptors (tag 0x10).
    e[90..108].copy_from_slice(&dummy_descriptor());
    e[108..126].copy_from_slice(&dummy_descriptor());

    // No extension blocks; checksum makes the block sum to 0 mod 256.
    e[126] = 0;
    let sum: u8 = e[..127].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    e[127] = sum.wrapping_neg();
    Some(e)
}

/// Packs a three-letter EISA manufacturer id (A=1 … Z=26, five bits each).
fn pack_manufacturer(letters: &[u8; 3]) -> u16 {
    let code = |c: u8| u16::from(c.saturating_sub(b'A' - 1)) & 0x1F;
    (code(letters[0]) << 10) | (code(letters[1]) << 5) | code(letters[2])
}

/// An 18-byte display descriptor carrying ASCII text (name/serial tags): the
/// text is terminated with `\n` and padded with spaces, per the standard.
fn write_text_descriptor(slot: &mut [u8], tag: u8, text: &[u8]) {
    slot.fill(0);
    slot[3] = tag;
    let body = &mut slot[5..18];
    body.fill(b' ');
    let len = text.len().min(12);
    body[..len].copy_from_slice(&text[..len]);
    body[len] = b'\n';
}

/// The all-but-empty descriptor EDID uses to say "nothing here".
fn dummy_descriptor() -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = 0x10;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(w: u32, h: u32) -> [u8; EDID_BLOCK_LEN] {
        edid_block(w, h, DEFAULT_REFRESH_HZ).expect("valid mode")
    }

    /// Decodes a detailed timing descriptor back to (h_active, v_active, Hz).
    fn timing(e: &[u8; EDID_BLOCK_LEN]) -> (u32, u32, u64) {
        let d = &e[54..72];
        let clock = u32::from(d[0]) | (u32::from(d[1]) << 8);
        let h_active = u32::from(d[2]) | ((u32::from(d[4]) >> 4) << 8);
        let h_blank = u32::from(d[3]) | ((u32::from(d[4]) & 0x0F) << 8);
        let v_active = u32::from(d[5]) | ((u32::from(d[7]) >> 4) << 8);
        let v_blank = u32::from(d[6]) | ((u32::from(d[7]) & 0x0F) << 8);
        let refresh = u64::from(clock) * 10_000
            / (u64::from(h_active + h_blank) * u64::from(v_active + v_blank));
        (h_active, v_active, refresh)
    }

    /// The two invariants every EDID consumer checks before anything else.
    #[test]
    fn header_and_checksum_are_valid() {
        for (w, h) in [(640, 480), (1280, 800), (1920, 1080), (3840, 2160)] {
            let e = block(w, h);
            assert_eq!(
                &e[0..8],
                &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
                "{w}x{h}: header"
            );
            let sum: u8 = e.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
            assert_eq!(sum, 0, "{w}x{h}: block must sum to 0 mod 256");
            assert_eq!(e[126], 0, "no extension blocks");
        }
    }

    /// The preferred detailed timing must decode back to the requested mode —
    /// this is the field DRM turns into the connector's preferred mode.
    #[test]
    fn detailed_timing_encodes_the_requested_mode() {
        let e = block(1920, 1080);
        let d = &e[54..72];
        let h_blank = u32::from(d[3]) | ((u32::from(d[4]) & 0x0F) << 8);
        let v_blank = u32::from(d[6]) | ((u32::from(d[7]) & 0x0F) << 8);
        assert_eq!((h_blank, v_blank), (H_BLANK, V_BLANK));
        // 60 Hz within the 10 kHz unit's truncation (exact for 1920x1080:
        // 2080 x 1125 x 60 = 140.40 MHz, a whole multiple of 10 kHz).
        assert_eq!(timing(&e), (1920, 1080, 60));
        // A pixel clock of zero would be a "no descriptor" marker by accident.
        assert!(u32::from(d[0]) | (u32::from(d[1]) << 8) > 0);
    }

    /// The advertised refresh is what the guest's compositor schedules
    /// against (GAME-2105), so it has to survive the descriptor's 10 kHz
    /// quantisation exactly at the rates a profile can ask for.
    #[test]
    fn the_advertised_refresh_is_the_one_asked_for() {
        for hz in [MIN_REFRESH_HZ, 30, 50, 60, 75, 120, 144, MAX_REFRESH_HZ] {
            let e = edid_block(1920, 1080, hz).expect("1080p is encodable at every allowed rate");
            assert_eq!(timing(&e), (1920, 1080, u64::from(hz)), "{hz} Hz");
            let sum: u8 = e.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
            assert_eq!(sum, 0, "{hz} Hz: checksum");
        }
    }

    /// 12-bit active fields and the 655.35 MHz pixel-clock ceiling: the modes
    /// past either are refused rather than wrapped.
    #[test]
    fn absurd_modes_are_refused_not_wrapped() {
        // 4K is the biggest mode anyone asks this device for, and it encodes.
        assert!(edid_block(3840, 2160, DEFAULT_REFRESH_HZ).is_some());
        // Wide-and-short / narrow-and-tall extremes inside the clock budget.
        assert!(edid_block(4095, 64, DEFAULT_REFRESH_HZ).is_some());
        assert!(edid_block(64, 4095, DEFAULT_REFRESH_HZ).is_some());
        // Past the 12-bit active fields.
        assert!(edid_block(4096, 1080, DEFAULT_REFRESH_HZ).is_none());
        assert!(edid_block(1920, 4096, DEFAULT_REFRESH_HZ).is_none());
        // Inside the fields but past the pixel-clock ceiling at 60 Hz.
        assert!(edid_block(4095, 4095, DEFAULT_REFRESH_HZ).is_none());
        // Degenerate.
        assert!(edid_block(0, 1080, DEFAULT_REFRESH_HZ).is_none());
        assert!(edid_block(1920, 0, DEFAULT_REFRESH_HZ).is_none());
        assert!(edid_block(1920, 1080, 0).is_none());
    }

    #[test]
    fn manufacturer_and_name_are_ours() {
        let e = block(1024, 768);
        // "ETG": E=5, T=20, G=7 → 0b00101_10100_00111.
        assert_eq!(pack_manufacturer(b"ETG"), 0b00101_10100_00111);
        assert_eq!(e[8], 0b0001_0110);
        assert_eq!(e[9], 0b1000_0111);
        // Descriptor 2 carries the display name tag and the text.
        assert_eq!(e[72 + 3], 0xFC);
        assert_eq!(&e[72 + 5..72 + 5 + 9], b"Entangled");
        assert_eq!(e[72 + 5 + 9], b'\n');
    }
}
