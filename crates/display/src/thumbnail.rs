//! A small picture of the guest's screen for the manager's machine cards
//! (2026-10-12).
//!
//! The control channel's `thumbnail <path>` asks for one; the manager asks
//! every few seconds while a machine runs and shows the answer on its card.
//! It is the screenshot path (the CPU mirror, or the read-back of a shared
//! frame, cursor composited) scaled down here with a box filter, so a 1080p
//! desktop costs one ~200 KiB PNG at most rather than a 6 MiB one.
//!
//! The file is written beside its destination and renamed into place, so a
//! reader polling the path never sees half a PNG.

use std::io::{BufWriter, Write as _};
use std::path::Path;

use crate::DisplayError;

/// The widest thumbnail the control channel makes; a card shows it at about
/// a third of this, and HiDPI needs the rest.
pub const MAX_WIDTH: u32 = 480;

/// Scales tightly packed BGRA down to at most `max_width` pixels wide (aspect
/// kept, never scaled up) and returns opaque RGBA: `(width, height, pixels)`.
/// Each output pixel is the mean of the source pixels its box covers.
pub fn downscale(
    width: u32,
    height: u32,
    bgra: &[u8],
    max_width: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if width == 0 || height == 0 || max_width == 0 {
        return None;
    }
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if bgra.len() < len {
        return None;
    }
    let out_w = width.min(max_width);
    let out_h = ((u64::from(height) * u64::from(out_w)) / u64::from(width)).max(1) as u32;
    let mut out = Vec::with_capacity(out_w as usize * out_h as usize * 4);
    for oy in 0..out_h {
        let y0 = (u64::from(oy) * u64::from(height) / u64::from(out_h)) as usize;
        let y1 = ((u64::from(oy + 1) * u64::from(height) / u64::from(out_h)) as usize).max(y0 + 1);
        for ox in 0..out_w {
            let x0 = (u64::from(ox) * u64::from(width) / u64::from(out_w)) as usize;
            let x1 =
                ((u64::from(ox + 1) * u64::from(width) / u64::from(out_w)) as usize).max(x0 + 1);
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                let row = y * width as usize * 4;
                for x in x0..x1 {
                    let at = row + x * 4;
                    sum[0] += u32::from(bgra[at + 2]);
                    sum[1] += u32::from(bgra[at + 1]);
                    sum[2] += u32::from(bgra[at]);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            out.extend_from_slice(&[
                ((sum[0] + n / 2) / n) as u8,
                ((sum[1] + n / 2) / n) as u8,
                ((sum[2] + n / 2) / n) as u8,
                0xff,
            ]);
        }
    }
    Some((out_w, out_h, out))
}

/// Encodes RGBA as a PNG at `path`, through a temporary file renamed into
/// place.
pub fn write_png_atomically(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), DisplayError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let file = std::fs::File::create(&tmp)?;
        let mut writer = BufWriter::new(file);
        let mut encoder = png::Encoder::new(&mut writer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // Speed over size: this runs every few seconds on a running VM.
        encoder.set_compression(png::Compression::Fast);
        let mut png = encoder.write_header()?;
        png.write_image_data(rgba)?;
        png.finish()?;
        writer.flush()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_averaged_down_and_keeps_its_aspect() {
        // 4×2, left half blue, right half red (BGRA).
        let mut bgra = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                bgra.extend_from_slice(if x < 2 {
                    &[255, 0, 0, 0]
                } else {
                    &[0, 0, 255, 0]
                });
            }
        }
        let (w, h, rgba) = downscale(4, 2, &bgra, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(&rgba[..4], &[0, 0, 255, 255], "blue, opaque");
        assert_eq!(&rgba[4..], &[255, 0, 0, 255], "red, opaque");

        // Mixed boxes average.
        let (_, _, rgba) = downscale(4, 2, &bgra, 1).unwrap();
        assert_eq!(&rgba[..], &[128, 0, 128, 255]);
    }

    #[test]
    fn small_frames_are_not_scaled_up_and_short_buffers_are_refused() {
        let bgra = vec![10u8; 3 * 2 * 4];
        let (w, h, _) = downscale(3, 2, &bgra, MAX_WIDTH).unwrap();
        assert_eq!((w, h), (3, 2));
        assert!(downscale(3, 2, &bgra[..20], MAX_WIDTH).is_none());
        assert!(downscale(0, 2, &bgra, MAX_WIDTH).is_none());
    }

    #[test]
    fn a_1080p_desktop_becomes_480_by_270() {
        let bgra = vec![0u8; 1920 * 1080 * 4];
        let (w, h, rgba) = downscale(1920, 1080, &bgra, MAX_WIDTH).unwrap();
        assert_eq!((w, h), (480, 270));
        assert_eq!(rgba.len(), 480 * 270 * 4);
    }

    #[test]
    fn the_png_lands_whole() {
        let dir = std::env::temp_dir().join(format!("display-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vm.png");
        write_png_atomically(&path, 1, 1, &[1, 2, 3, 255]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        assert!(!dir.join("vm.png.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
