//! How many bytes OpenGL will read or write for a call — computed here, from
//! the arguments the call is about to be given, never taken from the guest.
//!
//! Every answer assumes the pixel-store state this crate fixes on each
//! context: alignment 1, no row length, no skips, no bound pixel buffer. A
//! format or type that is not in these tables has no size, and a call
//! without a size is not made.

pub const BYTE: u32 = 0x1400;
pub const UNSIGNED_BYTE: u32 = 0x1401;
pub const SHORT: u32 = 0x1402;
pub const UNSIGNED_SHORT: u32 = 0x1403;
pub const INT: u32 = 0x1404;
pub const UNSIGNED_INT: u32 = 0x1405;
pub const FLOAT: u32 = 0x1406;
pub const DOUBLE: u32 = 0x140a;

pub const COLOR_INDEX: u32 = 0x1900;
pub const STENCIL_INDEX: u32 = 0x1901;
pub const DEPTH_COMPONENT: u32 = 0x1902;
pub const RED: u32 = 0x1903;
pub const GREEN: u32 = 0x1904;
pub const BLUE: u32 = 0x1905;
pub const ALPHA: u32 = 0x1906;
pub const RGB: u32 = 0x1907;
pub const RGBA: u32 = 0x1908;
pub const LUMINANCE: u32 = 0x1909;
pub const LUMINANCE_ALPHA: u32 = 0x190a;
pub const BGR: u32 = 0x80e0;
pub const BGRA: u32 = 0x80e1;

/// The most vertices one draw can carry.
pub const MAX_VERTICES: u32 = 1 << 22;
/// The largest image side a pixel call can name.
pub const MAX_IMAGE_SIDE: u32 = 16384;

/// Components per pixel of a pixel format.
pub fn components(format: u32) -> Option<u32> {
    Some(match format {
        COLOR_INDEX | STENCIL_INDEX | DEPTH_COMPONENT | RED | GREEN | BLUE | ALPHA | LUMINANCE => 1,
        LUMINANCE_ALPHA => 2,
        RGB | BGR => 3,
        RGBA | BGRA => 4,
        _ => return None,
    })
}

/// Bytes per component of a pixel type. `GL_BITMAP` and the packed types
/// are deliberately absent.
pub fn pixel_type_bytes(ty: u32) -> Option<u32> {
    Some(match ty {
        BYTE | UNSIGNED_BYTE => 1,
        SHORT | UNSIGNED_SHORT => 2,
        INT | UNSIGNED_INT | FLOAT => 4,
        _ => return None,
    })
}

/// Bytes per element of a vertex-array type.
pub fn array_type_bytes(ty: u32) -> Option<u32> {
    match ty {
        DOUBLE => Some(8),
        other => pixel_type_bytes(other),
    }
}

fn side(value: u32) -> Option<u64> {
    // The guest's GLsizei arrives as a word; a negative one is huge here.
    (value <= MAX_IMAGE_SIDE).then_some(u64::from(value))
}

/// Bytes of a `width`×`height` image.
pub fn image_bytes(width: u32, height: u32, format: u32, ty: u32) -> Option<usize> {
    let bytes = side(width)?
        * side(height)?
        * u64::from(components(format)?)
        * u64::from(pixel_type_bytes(ty)?);
    usize::try_from(bytes).ok()
}

/// Bytes of a `width`×`height` bitmap: one bit a pixel, rows padded to bytes.
pub fn bitmap_bytes(width: u32, height: u32) -> Option<usize> {
    usize::try_from(side(width)?.div_ceil(8) * side(height)?).ok()
}

/// Bytes of `count` vertices of `size` components of type `ty`.
pub fn array_bytes(count: u32, size: u32, ty: u32) -> Option<usize> {
    if count > MAX_VERTICES || size > 4 {
        return None;
    }
    usize::try_from(u64::from(count) * u64::from(size) * u64::from(array_type_bytes(ty)?)).ok()
}

/// Words that hold `bytes` bytes.
pub fn words_for(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_tightly_packed() {
        assert_eq!(image_bytes(3, 5, RGB, UNSIGNED_BYTE), Some(45));
        assert_eq!(image_bytes(64, 64, RGBA, UNSIGNED_BYTE), Some(16384));
        assert_eq!(image_bytes(2, 2, LUMINANCE_ALPHA, FLOAT), Some(32));
        assert_eq!(image_bytes(0, 9, RGBA, UNSIGNED_BYTE), Some(0));
    }

    #[test]
    fn what_has_no_size_is_refused() {
        // A negative GLsizei, as the word it arrives as.
        assert_eq!(image_bytes(-1i32 as u32, 1, RGBA, UNSIGNED_BYTE), None);
        assert_eq!(
            image_bytes(MAX_IMAGE_SIDE + 1, 1, RGBA, UNSIGNED_BYTE),
            None
        );
        // GL_BITMAP as a type, a packed type, and a format from a later
        // OpenGL whose size this table does not know.
        assert_eq!(image_bytes(8, 8, COLOR_INDEX, 0x1a00), None);
        assert_eq!(image_bytes(8, 8, RGB, 0x8363), None);
        assert_eq!(image_bytes(8, 8, 0x84f9, UNSIGNED_INT), None);
        assert_eq!(array_bytes(MAX_VERTICES + 1, 3, FLOAT), None);
        assert_eq!(array_bytes(3, 5, FLOAT), None);
        assert_eq!(array_bytes(3, 3, 0x1a00), None);
    }

    #[test]
    fn bitmaps_and_arrays() {
        assert_eq!(bitmap_bytes(9, 3), Some(6));
        assert_eq!(bitmap_bytes(32, 32), Some(128));
        assert_eq!(array_bytes(3, 3, FLOAT), Some(36));
        assert_eq!(array_bytes(2, 4, DOUBLE), Some(64));
        assert_eq!(words_for(5), 2);
        assert_eq!(words_for(8), 2);
    }
}
