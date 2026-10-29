//! Cursor plane metadata and pixels, independently of a live GPU resource.
use super::{put32, Cursor, GpuStateError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedCursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Raw virtio coordinate bits, including signed off-screen positions.
    pub x: u32,
    pub y: u32,
    pub pixels: Vec<u8>,
}

pub(super) fn encode(out: &mut Vec<u8>, cursor: Option<&SavedCursor>) {
    put32(out, u32::from(cursor.is_some()));
    if let Some(cursor) = cursor {
        for value in [
            cursor.width,
            cursor.height,
            cursor.hot_x,
            cursor.hot_y,
            cursor.x,
            cursor.y,
            cursor.pixels.len() as u32,
        ] {
            put32(out, value);
        }
        out.extend_from_slice(&cursor.pixels);
    }
}

pub(super) fn decode(r: &mut Cursor<'_>) -> Result<Option<SavedCursor>, GpuStateError> {
    if !r.bool("cursor present")? {
        return Ok(None);
    }
    let width = r.u32("cursor width")?;
    let height = r.u32("cursor height")?;
    for (what, value) in [("cursor width", width), ("cursor height", height)] {
        if value == 0 || value > crate::MAX_CURSOR_DIM {
            return Err(GpuStateError::BadValue { what, value });
        }
    }
    let hot_x = r.u32("cursor hotspot x")?;
    let hot_y = r.u32("cursor hotspot y")?;
    let x = r.u32("cursor x")?;
    let y = r.u32("cursor y")?;
    let len = r.count(
        "cursor pixels",
        crate::MAX_CURSOR_DIM * crate::MAX_CURSOR_DIM * 4,
        1,
    )?;
    if len != width * height * 4 {
        return Err(GpuStateError::BadValue {
            what: "cursor pixel count",
            value: len,
        });
    }
    let pixels = r
        .bytes
        .get(r.at..r.at.saturating_add(len as usize))
        .ok_or(GpuStateError::Truncated {
            what: "cursor pixels",
            need: len as usize,
            have: r.left(),
        })?
        .to_vec();
    r.at += len as usize;
    Ok(Some(SavedCursor {
        width,
        height,
        hot_x,
        hot_y,
        x,
        y,
        pixels,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::save::GpuState;

    fn sample() -> GpuState {
        GpuState {
            cursor: Some(SavedCursor {
                width: 2,
                height: 2,
                hot_x: 1,
                hot_y: 1,
                x: (-4i32) as u32,
                y: 37,
                pixels: vec![0x51; 16],
            }),
            ..GpuState::default()
        }
    }

    #[test]
    fn cursor_pixels_hotspot_and_signed_position_survive_without_the_old_resource() {
        let state = sample();
        assert_eq!(GpuState::decode(&state.encode()).unwrap(), state);
    }

    #[test]
    fn malformed_cursor_geometry_lengths_and_flags_are_refused() {
        let bytes = sample().encode();
        for end in 0..bytes.len() {
            assert!(GpuState::decode(&bytes[..end]).is_err());
        }
        // Empty resource and scanout prefixes occupy 24 bytes.
        for (offset, value) in [
            (24, 2u32),
            (28, 0),
            (28, u32::MAX),
            (32, 0),
            (32, u32::MAX),
            (52, 3),
            (52, u32::MAX),
        ] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(GpuState::decode(&invalid).is_err());
        }
    }

    #[test]
    fn version_2_snapshots_have_no_cursor_plane_and_migrate_to_version_3() {
        let mut legacy = GpuState::default().encode();
        legacy.truncate(legacy.len() - 4);
        legacy[..4].copy_from_slice(&2u32.to_le_bytes());
        let state = GpuState::decode(&legacy).unwrap();
        assert!(state.cursor.is_none());
        assert_eq!(&state.encode()[..4], &3u32.to_le_bytes());
    }
}
