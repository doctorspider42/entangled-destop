//! Live pictures of each machine's screen on its card.
//!
//! While a machine runs from here, every [`ASK_EVERY`] the manager sends its
//! engine `thumbnail` over the control channel; the engine writes a small PNG
//! to `control_api::control::thumbnail_path` beside the profile (atomically,
//! so a half-written file is never read here). This module only watches those
//! files: a new modification time means a new picture to decode and upload.
//! A stopped machine keeps its last picture, which the card shows dimmed — the
//! screen as it was left.
//!
//! The file is read on the egui thread: a 480-pixel PNG decodes in about a
//! millisecond, and only when it changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often a running machine is asked for a new picture.
pub const ASK_EVERY: Duration = Duration::from_secs(3);
/// How often the files are looked at (a cheap `stat` each).
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// Larger files are not pictures this engine wrote.
const MAX_FILE: u64 = 4 << 20;

struct Entry {
    texture: egui::TextureHandle,
    modified: SystemTime,
}

#[derive(Default)]
pub struct Thumbs {
    entries: HashMap<String, Entry>,
    asked_at: Option<Instant>,
    looked_at: Option<Instant>,
    /// How many machines the last look covered: a scan that changes the list
    /// is looked at at once, not a second later.
    looked_count: usize,
}

impl Thumbs {
    /// The machine's latest picture, if there is one.
    pub fn get(&self, vm: &str) -> Option<&egui::TextureHandle> {
        self.entries.get(vm).map(|e| &e.texture)
    }

    /// Whether it is time to ask the running machines again; resets the clock
    /// when it answers yes.
    pub fn due_to_ask(&mut self, now: Instant) -> bool {
        if self
            .asked_at
            .is_some_and(|at| now.duration_since(at) < ASK_EVERY)
        {
            return false;
        }
        self.asked_at = Some(now);
        true
    }

    /// Picks up every picture that changed since the last look. `vms` is
    /// `(name, profile path)` for every machine on the dashboard.
    pub fn refresh<'a>(
        &mut self,
        ctx: &egui::Context,
        now: Instant,
        vms: impl ExactSizeIterator<Item = (&'a str, &'a Path)>,
    ) {
        let count = vms.len();
        if count == self.looked_count
            && self
                .looked_at
                .is_some_and(|at| now.duration_since(at) < LOOK_EVERY)
        {
            return;
        }
        self.looked_at = Some(now);
        self.looked_count = count;
        let mut seen: Vec<String> = Vec::new();
        for (name, profile) in vms {
            seen.push(name.to_string());
            let path = path_for(profile, name);
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if meta.len() > MAX_FILE
                || self
                    .entries
                    .get(name)
                    .is_some_and(|e| e.modified == modified)
            {
                continue;
            }
            let Some(image) = std::fs::read(&path).ok().and_then(|b| decode(&b)) else {
                continue;
            };
            let texture =
                ctx.load_texture(format!("thumb-{name}"), image, egui::TextureOptions::LINEAR);
            self.entries
                .insert(name.to_string(), Entry { texture, modified });
        }
        // A machine that left the dashboard takes its texture with it.
        self.entries.retain(|name, _| seen.contains(name));
    }
}

/// Where the engine writes `name`'s picture for this profile.
pub fn path_for(profile: &Path, name: &str) -> PathBuf {
    control_api::control::thumbnail_path(profile.parent().unwrap_or(Path::new(".")), name)
}

/// An 8-bit RGBA PNG (what the engine writes) as an egui image; anything else
/// is refused rather than converted.
pub fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let (w, h) = (info.width as usize, info.height as usize);
    let pixels = buf.get(..w.checked_mul(h)?.checked_mul(4)?)?;
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn an_rgba_png_decodes_and_anything_else_is_refused() {
        let image = decode(&png(
            2,
            1,
            png::ColorType::Rgba,
            &[1, 2, 3, 255, 4, 5, 6, 255],
        ))
        .expect("decodes");
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(4, 5, 6));
        assert!(decode(&png(1, 1, png::ColorType::Rgb, &[1, 2, 3])).is_none());
        assert!(decode(b"not a png").is_none());
    }

    #[test]
    fn the_manager_and_the_engine_agree_on_the_path() {
        let path = path_for(Path::new("/vms/ubuntu.toml"), "ubuntu");
        assert_eq!(path, PathBuf::from("/vms/.thumbnails/ubuntu.png"));
    }

    #[test]
    fn asking_is_paced() {
        let mut thumbs = Thumbs::default();
        let t0 = Instant::now();
        assert!(thumbs.due_to_ask(t0));
        assert!(!thumbs.due_to_ask(t0 + Duration::from_secs(1)));
        assert!(thumbs.due_to_ask(t0 + ASK_EVERY));
    }
}
