//! The host-side mirror of the guest scanout (backlog MVP-703/704/707).
//!
//! The guest owns the pixels; the host keeps one CPU-side copy in the guest's
//! own format (`B8G8R8A8_UNORM`, see [`virtio_gpu::FORMAT_B8G8R8A8_UNORM`]) plus
//! a dirty rect. The renderer uploads only the dirty rect into its `wgpu`
//! texture, and screenshots (MVP-707) are encoded straight from this mirror —
//! no GPU readback, so `--headless` acceptance tests and the windowed path use
//! exactly the same bytes.
//!
//! All rects are validated before any copy: a malformed request is rejected
//! with a [`DisplayError`], never a panic (workspace hard rule — the guest is
//! untrusted).

use std::io::BufWriter;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use virtio_gpu::{Rect, BYTES_PER_PIXEL};

use crate::DisplayError;

/// Bytes per scanout pixel, as a `usize` for slice arithmetic.
const BPP: usize = BYTES_PER_PIXEL as usize;

/// Copy statistics for the periodic diagnostics event (backlog MVP-708).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanoutStats {
    /// Accepted [`Scanout::update`] calls.
    pub updates: u64,
    /// Rejected update calls (bad rect or short data).
    pub rejected: u64,
    /// Pixel bytes accepted from the guest.
    pub bytes: u64,
    /// Resolution changes (each one reallocates the GPU texture).
    pub resolutions: u64,
}

/// The guest's hardware-cursor plane (MVP-812): a small premultiplied-BGRA
/// image composited over the scanout at present time, never written into the
/// guest-pixel mirror — so a cursor move costs two small dirty rects, not a
/// stale saved-under-region.
#[derive(Debug, Clone)]
struct CursorPlane {
    width: u32,
    height: u32,
    /// `width * height * 4` bytes, BGRA, premultiplied alpha (the DRM
    /// cursor-plane convention Linux' virtio_gpu driver follows).
    pixels: Vec<u8>,
    /// Top-left corner in scanout coordinates: hotspot position minus the
    /// hotspot offset, so it can be negative when the pointer hugs an edge.
    x: i64,
    y: i64,
    hot_x: u32,
    hot_y: u32,
}

impl CursorPlane {
    /// The plane clipped to a `w`×`h` scanout, or `None` when it is entirely
    /// off screen.
    fn clipped_rect(&self, w: u32, h: u32) -> Option<Rect> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + i64::from(self.width)).min(i64::from(w));
        let y1 = (self.y + i64::from(self.height)).min(i64::from(h));
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        // All four values are in 0..=u32::MAX after the clamps above.
        Some(Rect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }
}

/// The host copy of one guest scanout.
#[derive(Debug)]
pub struct Scanout {
    width: u32,
    height: u32,
    /// `width * height * 4` bytes, BGRA, row stride `width * 4`.
    pixels: Vec<u8>,
    /// Union of all rects written since the renderer last uploaded.
    dirty: Option<Rect>,
    /// Bumped on every resolution change so the renderer knows to reallocate.
    generation: u64,
    /// The cursor plane, when the guest is showing one.
    cursor: Option<CursorPlane>,
    stats: ScanoutStats,
}

impl Scanout {
    /// Allocates a black scanout of the given size.
    pub fn new(width: u32, height: u32) -> Result<Self, DisplayError> {
        let len = Self::checked_len(width, height)?;
        Ok(Self {
            width,
            height,
            pixels: vec![0; len],
            dirty: Some(Rect {
                x: 0,
                y: 0,
                width,
                height,
            }),
            generation: 0,
            cursor: None,
            stats: ScanoutStats::default(),
        })
    }

    fn checked_len(width: u32, height: u32) -> Result<usize, DisplayError> {
        let pixels = u64::from(width) * u64::from(height);
        if width == 0 || height == 0 || pixels > crate::MAX_SCANOUT_PIXELS {
            return Err(DisplayError::InvalidResolution { width, height });
        }
        usize::try_from(pixels * u64::from(BYTES_PER_PIXEL))
            .map_err(|_| DisplayError::InvalidResolution { width, height })
    }

    /// Current guest resolution.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Row stride in bytes.
    pub fn stride(&self) -> usize {
        self.width as usize * BPP
    }

    /// The raw BGRA mirror.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Texture generation; changes when the scanout was reallocated.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Copy statistics since start.
    pub fn stats(&self) -> ScanoutStats {
        self.stats
    }

    /// Copies a `width`×`height` block of tightly packed BGRA pixels to
    /// (`x`, `y`) (backlog MVP-704). `data` may be longer than the rect needs;
    /// trailing bytes are ignored.
    pub fn update(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<(), DisplayError> {
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        if !rect.fits_within(self.width, self.height) {
            self.stats.rejected += 1;
            return Err(DisplayError::RectOutOfBounds {
                x,
                y,
                width,
                height,
                scanout_width: self.width,
                scanout_height: self.height,
            });
        }
        // fits_within() already bounded width/height by the scanout, so these
        // products cannot overflow usize on a 64-bit host.
        let row_bytes = width as usize * BPP;
        let needed = row_bytes * height as usize;
        if data.len() < needed {
            self.stats.rejected += 1;
            return Err(DisplayError::ShortPixelData {
                expected: needed,
                actual: data.len(),
            });
        }

        let stride = self.stride();
        let x_off = x as usize * BPP;
        let rows = self
            .pixels
            .chunks_mut(stride)
            .skip(y as usize)
            .take(height as usize);
        for (dst_row, src_row) in rows.zip(data.chunks_exact(row_bytes)) {
            let Some(dst) = dst_row.get_mut(x_off..x_off + row_bytes) else {
                // Unreachable given the validation above; still no panic.
                self.stats.rejected += 1;
                return Err(DisplayError::RectOutOfBounds {
                    x,
                    y,
                    width,
                    height,
                    scanout_width: self.width,
                    scanout_height: self.height,
                });
            };
            dst.copy_from_slice(src_row);
        }

        self.dirty = Some(match self.dirty {
            Some(existing) => union(existing, rect),
            None => rect,
        });
        self.stats.updates += 1;
        self.stats.bytes += needed as u64;
        Ok(())
    }

    /// Reallocates the scanout for a new guest mode, clearing it to black and
    /// marking everything dirty.
    pub fn set_resolution(&mut self, width: u32, height: u32) -> Result<(), DisplayError> {
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        let len = Self::checked_len(width, height)?;
        self.pixels.clear();
        self.pixels.resize(len, 0);
        self.width = width;
        self.height = height;
        self.generation = self.generation.wrapping_add(1);
        self.stats.resolutions += 1;
        self.dirty = Some(Rect {
            x: 0,
            y: 0,
            width,
            height,
        });
        Ok(())
    }

    /// Takes the accumulated dirty rect, leaving the scanout clean.
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take()
    }

    // ---------------------------------------------------- the cursor plane

    /// Shows (or replaces) the cursor plane (MVP-812). `data` is tightly
    /// packed premultiplied BGRA; extra trailing bytes are ignored. The
    /// hotspot lands on scanout position (`x`, `y`).
    #[allow(clippy::too_many_arguments)]
    pub fn set_cursor(
        &mut self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        x: u32,
        y: u32,
        data: &[u8],
    ) -> Result<(), DisplayError> {
        // The device already bounds cursor geometry; this re-check keeps the
        // display safe on its own terms (it is a public API).
        if width == 0
            || height == 0
            || u64::from(width) * u64::from(height) > crate::MAX_CURSOR_PIXELS
        {
            return Err(DisplayError::InvalidResolution { width, height });
        }
        let needed = width as usize * height as usize * BPP;
        if data.len() < needed {
            return Err(DisplayError::ShortPixelData {
                expected: needed,
                actual: data.len(),
            });
        }
        self.dirty_under_cursor();
        self.cursor = Some(CursorPlane {
            width,
            height,
            pixels: data[..needed].to_vec(),
            x: i64::from(x) - i64::from(hot_x),
            y: i64::from(y) - i64::from(hot_y),
            hot_x,
            hot_y,
        });
        self.dirty_under_cursor();
        Ok(())
    }

    /// Moves the cursor plane's hotspot to (`x`, `y`). A move with no plane
    /// shown is a no-op (the guest moves a hidden cursor freely).
    pub fn move_cursor(&mut self, x: u32, y: u32) {
        self.dirty_under_cursor();
        if let Some(cursor) = &mut self.cursor {
            cursor.x = i64::from(x) - i64::from(cursor.hot_x);
            cursor.y = i64::from(y) - i64::from(cursor.hot_y);
        }
        self.dirty_under_cursor();
    }

    /// Hides the cursor plane.
    pub fn hide_cursor(&mut self) {
        self.dirty_under_cursor();
        self.cursor = None;
    }

    /// Whether a cursor plane is currently shown.
    pub fn cursor_visible(&self) -> bool {
        self.cursor.is_some()
    }

    /// The visible part of the cursor composited over the guest pixels:
    /// the clipped scanout rect it covers, and that rect's pixels with the
    /// cursor blended in (premultiplied source-over). `None` when no cursor is
    /// shown or it is entirely off screen.
    ///
    /// The guest-pixel mirror itself is never touched: the renderer uploads
    /// this block *after* the dirty base rect, and screenshots apply it onto
    /// their copy, so the plane behaves like the hardware overlay it models.
    pub fn cursor_overlay(&self) -> Option<(Rect, Vec<u8>)> {
        let cursor = self.cursor.as_ref()?;
        let rect = cursor.clipped_rect(self.width, self.height)?;
        let mut out = vec![0u8; rect.width as usize * rect.height as usize * BPP];
        let stride = self.stride();
        for row in 0..rect.height as usize {
            let src_start = (rect.y as usize + row) * stride + rect.x as usize * BPP;
            let dst_start = row * rect.width as usize * BPP;
            let width_bytes = rect.width as usize * BPP;
            let (Some(src), Some(dst)) = (
                self.pixels.get(src_start..src_start + width_bytes),
                out.get_mut(dst_start..dst_start + width_bytes),
            ) else {
                // Unreachable given clipped_rect; still no panic.
                return None;
            };
            dst.copy_from_slice(src);
        }
        // Blend the overlapping part of the cursor image. The offsets of the
        // clipped rect inside the cursor image are non-negative by clipping.
        let cur_x0 = (i64::from(rect.x) - cursor.x) as usize;
        let cur_y0 = (i64::from(rect.y) - cursor.y) as usize;
        for row in 0..rect.height as usize {
            for col in 0..rect.width as usize {
                let src_at = ((cur_y0 + row) * cursor.width as usize + cur_x0 + col) * BPP;
                let dst_at = (row * rect.width as usize + col) * BPP;
                let (Some(src), Some(dst)) = (
                    cursor.pixels.get(src_at..src_at + BPP),
                    out.get_mut(dst_at..dst_at + BPP),
                ) else {
                    return None;
                };
                let alpha = u16::from(src[3]);
                for channel in 0..BPP {
                    // Premultiplied source-over: dst = src + dst * (255 - a).
                    let blended = u16::from(src[channel])
                        + (u16::from(dst[channel]) * (255 - alpha) + 127) / 255;
                    dst[channel] = blended.min(255) as u8;
                }
            }
        }
        Some((rect, out))
    }

    /// Marks the scanout area under the cursor dirty, so the renderer restores
    /// the base pixels there when the cursor moves away or changes shape.
    fn dirty_under_cursor(&mut self) {
        let Some(rect) = self
            .cursor
            .as_ref()
            .and_then(|c| c.clipped_rect(self.width, self.height))
        else {
            return;
        };
        self.dirty = Some(match self.dirty {
            Some(existing) => union(existing, rect),
            None => rect,
        });
    }

    /// Marks the whole scanout dirty (used after a texture reallocation).
    pub fn mark_all_dirty(&mut self) {
        self.dirty = Some(Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
    }

    /// Fills the whole scanout with one BGRA color. Test/demo helper.
    pub fn fill(&mut self, bgra: [u8; 4]) {
        for px in self.pixels.chunks_exact_mut(BPP) {
            px.copy_from_slice(&bgra);
        }
        self.mark_all_dirty();
    }

    /// Encodes the current scanout as a PNG (backlog MVP-707).
    ///
    /// The guest's alpha channel is ignored (Linux fbdev and the DRM
    /// `XRGB8888` mapping leave it zero, which would make the screenshot fully
    /// transparent), so every pixel is written opaque.
    pub fn to_png(&self) -> Result<Vec<u8>, DisplayError> {
        let mut out = Vec::new();
        self.encode_png(&mut out)?;
        Ok(out)
    }

    /// Encodes the current scanout as a PNG straight to `path`.
    pub fn write_png(&self, path: &Path) -> Result<(), DisplayError> {
        let file = std::fs::File::create(path)?;
        self.encode_png(BufWriter::new(file))
    }

    fn encode_png<W: std::io::Write>(&self, writer: W) -> Result<(), DisplayError> {
        let mut encoder = png::Encoder::new(writer, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut rgba = Vec::with_capacity(self.pixels.len());
        for px in self.pixels.chunks_exact(BPP) {
            match *px {
                [b, g, r, _] => rgba.extend_from_slice(&[r, g, b, 0xff]),
                _ => return Err(DisplayError::Config("scanout mirror is not BGRA-aligned")),
            }
        }
        writer.write_image_data(&rgba)?;
        writer.finish()?;
        Ok(())
    }
}

/// Smallest rect covering both inputs.
fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

/// The scanout as shared between the device threads (writers) and the event
/// loop (reader). The critical section is a `memcpy`, never GPU work — the
/// event loop must not block on guest state.
pub type SharedScanout = Arc<Mutex<Scanout>>;

/// Locks the shared scanout, recovering from a poisoned mutex instead of
/// panicking. Nothing inside the critical section can panic (all indexing is
/// checked), so a poisoned lock means an unrelated thread died mid-frame; the
/// pixels are still structurally valid.
pub fn lock_scanout(shared: &Mutex<Scanout>) -> MutexGuard<'_, Scanout> {
    crate::sync::lock(shared, "scanout")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(s: &Scanout, x: u32, y: u32) -> [u8; 4] {
        let off = y as usize * s.stride() + x as usize * BPP;
        let mut out = [0u8; 4];
        out.copy_from_slice(&s.pixels()[off..off + 4]);
        out
    }

    #[test]
    fn new_scanout_is_black_and_fully_dirty() {
        let mut s = Scanout::new(4, 2).unwrap();
        assert_eq!(s.size(), (4, 2));
        assert_eq!(s.stride(), 16);
        assert_eq!(s.pixels().len(), 32);
        assert_eq!(
            s.take_dirty(),
            Some(Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 2
            })
        );
        assert_eq!(s.take_dirty(), None);
    }

    #[test]
    fn zero_and_huge_resolutions_are_rejected() {
        assert!(Scanout::new(0, 100).is_err());
        assert!(Scanout::new(100, 0).is_err());
        assert!(Scanout::new(100_000, 100_000).is_err());
    }

    #[test]
    fn partial_update_lands_at_the_right_offset() {
        let mut s = Scanout::new(4, 4).unwrap();
        let _ = s.take_dirty();
        // 2x2 red block at (1,1).
        let block = [
            0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0xff, 0xff, // row 0
            0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0xff, 0xff, // row 1
        ];
        s.update(1, 1, 2, 2, &block).unwrap();
        assert_eq!(pixel(&s, 1, 1), [0, 0, 0xff, 0xff]);
        assert_eq!(pixel(&s, 2, 2), [0, 0, 0xff, 0xff]);
        assert_eq!(pixel(&s, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&s, 3, 3), [0, 0, 0, 0]);
        assert_eq!(
            s.take_dirty(),
            Some(Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 2
            })
        );
        assert_eq!(s.stats().updates, 1);
        assert_eq!(s.stats().bytes, 16);
    }

    #[test]
    fn dirty_rects_accumulate_as_a_union() {
        let mut s = Scanout::new(8, 8).unwrap();
        let _ = s.take_dirty();
        let px = [0xffu8; 4];
        s.update(1, 1, 1, 1, &px).unwrap();
        s.update(5, 6, 1, 1, &px).unwrap();
        assert_eq!(
            s.take_dirty(),
            Some(Rect {
                x: 1,
                y: 1,
                width: 5,
                height: 6
            })
        );
    }

    #[test]
    fn out_of_bounds_and_short_updates_are_rejected() {
        let mut s = Scanout::new(4, 4).unwrap();
        let data = [0xffu8; 4 * 4 * 4];
        assert!(matches!(
            s.update(3, 0, 2, 1, &data),
            Err(DisplayError::RectOutOfBounds { .. })
        ));
        assert!(matches!(
            s.update(0, 0, 0, 1, &data),
            Err(DisplayError::RectOutOfBounds { .. })
        ));
        assert!(matches!(
            s.update(u32::MAX, 0, 2, 2, &data),
            Err(DisplayError::RectOutOfBounds { .. })
        ));
        assert!(matches!(
            s.update(0, 0, 2, 2, &data[..8]),
            Err(DisplayError::ShortPixelData {
                expected: 16,
                actual: 8
            })
        ));
        // Nothing was written and nothing became dirty.
        assert_eq!(s.stats().rejected, 4);
        assert_eq!(pixel(&s, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn extra_trailing_bytes_are_ignored() {
        let mut s = Scanout::new(2, 2).unwrap();
        let data = [0x11u8; 64];
        s.update(0, 0, 2, 2, &data).unwrap();
        assert_eq!(pixel(&s, 1, 1), [0x11; 4]);
    }

    #[test]
    fn set_resolution_reallocates_and_bumps_generation() {
        let mut s = Scanout::new(4, 4).unwrap();
        s.fill([1, 2, 3, 4]);
        let gen0 = s.generation();
        s.set_resolution(4, 4).unwrap();
        assert_eq!(s.generation(), gen0, "same size is a no-op");
        s.set_resolution(8, 2).unwrap();
        assert_eq!(s.size(), (8, 2));
        assert_eq!(s.pixels().len(), 8 * 2 * 4);
        assert_eq!(s.generation(), gen0 + 1);
        assert_eq!(pixel(&s, 0, 0), [0, 0, 0, 0], "cleared to black");
        assert_eq!(
            s.take_dirty(),
            Some(Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 2
            })
        );
        assert!(s.set_resolution(0, 0).is_err());
        assert_eq!(s.size(), (8, 2), "a rejected mode change changes nothing");
    }

    #[test]
    fn png_is_rgba_opaque_and_round_trips() {
        let mut s = Scanout::new(2, 1).unwrap();
        // BGRA with alpha 0 — the guest's typical XRGB framebuffer.
        s.update(0, 0, 2, 1, &[10, 20, 30, 0, 40, 50, 60, 0])
            .unwrap();
        let png_bytes = s.to_png().unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        // BGRA (10,20,30) -> RGBA (30,20,10,255).
        assert_eq!(&buf[..8], &[30, 20, 10, 0xff, 60, 50, 40, 0xff]);
    }

    #[test]
    fn shared_scanout_recovers_from_poisoning() {
        let shared: SharedScanout = Arc::new(Mutex::new(Scanout::new(2, 2).unwrap()));
        let clone = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = lock_scanout(&clone);
            panic!("poison the mutex");
        })
        .join();
        assert!(shared.is_poisoned());
        assert_eq!(lock_scanout(&shared).size(), (2, 2));
    }

    /// One opaque premultiplied-BGRA pixel.
    fn cursor_px(bgra: [u8; 4]) -> Vec<u8> {
        bgra.to_vec()
    }

    #[test]
    fn cursor_overlay_composites_over_the_base_without_touching_it() {
        let mut s = Scanout::new(4, 4).unwrap();
        s.fill([10, 20, 30, 0xff]);
        let _ = s.take_dirty();

        // Opaque red 1x1 cursor at (2,1), hotspot 0.
        s.set_cursor(1, 1, 0, 0, 2, 1, &cursor_px([0, 0, 0xff, 0xff]))
            .unwrap();
        let (rect, pixels) = s.cursor_overlay().expect("visible cursor");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (2, 1, 1, 1));
        assert_eq!(pixels, vec![0, 0, 0xff, 0xff]);
        // The base mirror still holds the guest pixels underneath.
        assert_eq!(pixel(&s, 2, 1), [10, 20, 30, 0xff]);
        // Showing the cursor dirtied its rect so the renderer redraws there.
        assert_eq!(
            s.take_dirty(),
            Some(Rect {
                x: 2,
                y: 1,
                width: 1,
                height: 1
            })
        );

        // 50% premultiplied gray over the base blends both halves.
        s.set_cursor(1, 1, 0, 0, 0, 0, &cursor_px([0x40, 0x40, 0x40, 0x80]))
            .unwrap();
        let (_, pixels) = s.cursor_overlay().expect("visible cursor");
        // dst = src + base * (255-128)/255: 0x40 + round(10*127/255) = 0x45 …
        assert_eq!(pixels[0], 0x40 + ((10 * 127 + 127) / 255) as u8);
        assert_eq!(pixels[3], 0x80 + ((255 * 127 + 127) / 255) as u8);
    }

    #[test]
    fn cursor_moves_mark_both_positions_dirty_and_clip_at_edges() {
        let mut s = Scanout::new(8, 8).unwrap();
        let _ = s.take_dirty();
        let image = vec![0xffu8; 2 * 2 * 4];
        // Hotspot (1,1) at (0,0): the image's top-left hangs off screen.
        s.set_cursor(2, 2, 1, 1, 0, 0, &image).unwrap();
        let (rect, pixels) = s.cursor_overlay().expect("clipped but visible");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (0, 0, 1, 1));
        assert_eq!(pixels.len(), 4);

        let _ = s.take_dirty();
        s.move_cursor(6, 6);
        // The dirty union covers the vacated corner and the new position.
        let dirty = s.take_dirty().expect("move dirties");
        assert_eq!((dirty.x, dirty.y), (0, 0));
        assert!(dirty.width >= 7 && dirty.height >= 7, "{dirty:?}");
        let (rect, _) = s.cursor_overlay().expect("visible");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (5, 5, 2, 2));

        // Entirely off screen: no overlay, and hiding cleans up.
        s.move_cursor(20, 20);
        assert!(s.cursor_overlay().is_none());
        s.hide_cursor();
        assert!(!s.cursor_visible());
        assert!(s.cursor_overlay().is_none());
    }

    #[test]
    fn absurd_cursors_are_refused() {
        let mut s = Scanout::new(8, 8).unwrap();
        let image = vec![0u8; 16];
        assert!(s.set_cursor(0, 2, 0, 0, 0, 0, &image).is_err());
        assert!(s.set_cursor(2, 0, 0, 0, 0, 0, &image).is_err());
        assert!(
            s.set_cursor(2, 2, 0, 0, 0, 0, &image[..8]).is_err(),
            "short data"
        );
        assert!(
            s.set_cursor(4096, 4096, 0, 0, 0, 0, &image).is_err(),
            "over the plane cap"
        );
        assert!(!s.cursor_visible());
    }

    #[test]
    fn union_covers_both_rects() {
        let a = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let b = Rect {
            x: 10,
            y: 4,
            width: 1,
            height: 1,
        };
        assert_eq!(
            union(a, b),
            Rect {
                x: 0,
                y: 0,
                width: 11,
                height: 5
            }
        );
        assert_eq!(union(b, a), union(a, b));
    }
}
