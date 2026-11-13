//! Code page 437 glyphs and the text screen drawn with them.
//!
//! The glyphs are `font8x8`'s 8x8 bitmaps with every row doubled, which is
//! what an 8x16 cell needs and involves no font of anyone else's. A guest
//! that loads its own font into the VGA (Windows Setup does) is a matter for
//! the VGA device, not for this table.

use font8x8::UnicodeFonts;

/// Code page 437 as Unicode, in code order.
const CP437: &str = "\
\u{0}☺☻♥♦♣♠•◘○◙♂♀♪♫☼►◄↕‼¶§▬↨↑↓→←∟↔▲▼ \
!\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~⌂\
ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»\
░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀\
αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";

/// The Unicode character code page 437 has at `code`.
pub fn cp437(code: u8) -> char {
    CP437.chars().nth(usize::from(code)).unwrap_or(' ')
}

fn glyph_8x8(c: char) -> Option<[u8; 8]> {
    font8x8::BASIC_FONTS
        .get(c)
        .or_else(|| font8x8::BOX_FONTS.get(c))
        .or_else(|| font8x8::BLOCK_FONTS.get(c))
        .or_else(|| font8x8::LATIN_FONTS.get(c))
        .or_else(|| font8x8::GREEK_FONTS.get(c))
        .or_else(|| font8x8::MISC_FONTS.get(c))
}

/// The 8x16 glyph for a code page 437 byte: 16 rows, most significant bit
/// leftmost (the VGA's order). A character the source font lacks is blank.
pub fn glyph(code: u8) -> [u8; 16] {
    let mut rows = [0u8; 16];
    if code == 0 || code == 0xff {
        return rows;
    }
    let Some(small) = glyph_8x8(cp437(code)) else {
        return rows;
    };
    for (y, row) in small.iter().enumerate() {
        // font8x8 puts the leftmost pixel in bit 0.
        let row = row.reverse_bits();
        rows[y * 2] = row;
        rows[y * 2 + 1] = row;
    }
    rows
}

/// Text mode 3: 80 columns of 25 rows.
pub const COLUMNS: usize = 80;
pub const ROWS: usize = 25;
/// Bytes in one page of text memory: a character and an attribute per cell.
pub const PAGE_BYTES: usize = COLUMNS * ROWS * 2;
/// The picture [`render`] makes.
pub const WIDTH: u32 = (COLUMNS * 8) as u32;
pub const HEIGHT: u32 = (ROWS * 16) as u32;

/// The sixteen CGA colours, as B, G, R.
const PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xaa, 0x00, 0x00],
    [0x00, 0xaa, 0x00],
    [0xaa, 0xaa, 0x00],
    [0x00, 0x00, 0xaa],
    [0xaa, 0x00, 0xaa],
    [0x00, 0x55, 0xaa],
    [0xaa, 0xaa, 0xaa],
    [0x55, 0x55, 0x55],
    [0xff, 0x55, 0x55],
    [0x55, 0xff, 0x55],
    [0xff, 0xff, 0x55],
    [0x55, 0x55, 0xff],
    [0xff, 0x55, 0xff],
    [0x55, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];

/// Bytes of a font laid out as a VGA keeps it in plane 2: 256 glyphs of 32
/// rows, of which a mode uses the first `cell height`.
pub const GLYPH_PLANE_BYTES: usize = 256 * 32;

/// The glyphs of all 256 codes, built once by whoever draws.
pub struct Font {
    glyphs: Box<[[u8; 16]; 256]>,
    plane: Vec<u8>,
}

impl Default for Font {
    fn default() -> Self {
        let mut glyphs = Box::new([[0u8; 16]; 256]);
        let mut plane = vec![0u8; GLYPH_PLANE_BYTES];
        for (code, slot) in glyphs.iter_mut().enumerate() {
            *slot = glyph(code as u8);
            plane[code * 32..code * 32 + 16].copy_from_slice(slot);
        }
        Self { glyphs, plane }
    }
}

impl Font {
    /// The font in plane-2 layout ([`GLYPH_PLANE_BYTES`]), for
    /// [`render_frame`].
    pub fn plane(&self) -> &[u8] {
        &self.plane
    }
}

/// One text screen of any geometry, as a VGA describes it.
pub struct TextFrame<'a> {
    /// A character and an attribute per cell, row by row.
    pub cells: &'a [u8],
    pub columns: usize,
    pub rows: usize,
    /// Scan lines per cell; at most 32.
    pub cell_height: usize,
    /// The font in plane-2 layout: 32 bytes a glyph.
    pub glyphs: &'a [u8],
    /// The sixteen colours, as B, G, R.
    pub palette: &'a [[u8; 3]; 16],
    /// Attribute bit 7 is blink, so the background has three bits.
    pub blink: bool,
    /// The cursor: column, row, first and last scan line.
    pub cursor: Option<(usize, usize, usize, usize)>,
}

/// Draws `frame` as BGRA into `out`; returns the picture's width and height.
/// Geometry beyond what a VGA can show is clamped, and cells or glyphs the
/// slices do not hold draw blank.
pub fn render_frame(frame: &TextFrame<'_>, out: &mut Vec<u8>) -> (u32, u32) {
    let columns = frame.columns.clamp(1, 132);
    let rows = frame.rows.clamp(1, 60);
    let cell_height = frame.cell_height.clamp(1, 32);
    let (width, height) = (columns * 8, rows * cell_height);
    let stride = width * 4;
    out.clear();
    out.resize(stride * height, 0);
    for pixel in out.chunks_exact_mut(4) {
        pixel[3] = 0xff;
    }
    for (index, cell) in frame.cells.chunks_exact(2).take(columns * rows).enumerate() {
        let (col, row) = (index % columns, index / columns);
        let glyph = usize::from(cell[0]) * 32;
        let fg = frame.palette[usize::from(cell[1] & 0x0f)];
        let background = if frame.blink { 0x07 } else { 0x0f };
        let bg = frame.palette[usize::from(cell[1] >> 4 & background)];
        let cursor = frame
            .cursor
            .filter(|&(c, r, _, _)| (c, r) == (col, row))
            .map(|(_, _, first, last)| first..=last);
        for y in 0..cell_height {
            let in_cursor = cursor.as_ref().is_some_and(|lines| lines.contains(&y));
            let bits = if in_cursor {
                0xff
            } else {
                frame.glyphs.get(glyph + y).copied().unwrap_or(0)
            };
            let at = (row * cell_height + y) * stride + col * 8 * 4;
            for x in 0..8 {
                let colour = if bits & (0x80 >> x) != 0 { fg } else { bg };
                out[at + x * 4..at + x * 4 + 3].copy_from_slice(&colour);
            }
        }
    }
    (width as u32, height as u32)
}

/// Draws one page of text memory as [`WIDTH`]x[`HEIGHT`] BGRA into `out`.
///
/// `cells` is the page as the guest wrote it; a short slice draws what it
/// has. `cursor` is the (column, row) to underline, if it is visible.
pub fn render(font: &Font, cells: &[u8], cursor: Option<(usize, usize)>, out: &mut Vec<u8>) {
    let stride = WIDTH as usize * 4;
    out.clear();
    out.resize(stride * HEIGHT as usize, 0);
    for pixel in out.chunks_exact_mut(4) {
        pixel[3] = 0xff;
    }
    for (index, cell) in cells.chunks_exact(2).take(COLUMNS * ROWS).enumerate() {
        let (col, row) = (index % COLUMNS, index / COLUMNS);
        let glyph = &font.glyphs[usize::from(cell[0])];
        let fg = PALETTE[usize::from(cell[1] & 0x0f)];
        // Bit 7 is blink on a VGA; drawn as the plain background colour.
        let bg = PALETTE[usize::from((cell[1] >> 4) & 0x07)];
        let underline = cursor == Some((col, row));
        for (y, bits) in glyph.iter().enumerate() {
            let bits = if underline && y >= 14 { 0xff } else { *bits };
            let at = (row * 16 + y) * stride + col * 8 * 4;
            for x in 0..8 {
                let colour = if bits & (0x80 >> x) != 0 { fg } else { bg };
                out[at + x * 4..at + x * 4 + 3].copy_from_slice(&colour);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_page_437_has_256_characters_in_order() {
        assert_eq!(CP437.chars().count(), 256);
        assert_eq!(cp437(b'A'), 'A');
        assert_eq!(cp437(0x7f), '⌂');
        assert_eq!(cp437(0x80), 'Ç');
        assert_eq!(cp437(0xb3), '│');
        assert_eq!(cp437(0xc4), '─');
        assert_eq!(cp437(0xcd), '═');
        assert_eq!(cp437(0xdb), '█');
        assert_eq!(cp437(0xfe), '■');
    }

    #[test]
    fn letters_and_the_frame_characters_have_glyphs() {
        for code in [b'A', b'z', b'0', 0xb3, 0xc4, 0xcd, 0xba, 0xc9, 0xdb, 0xb0] {
            assert!(
                glyph(code).iter().any(|&row| row != 0),
                "{code:#x} is blank"
            );
        }
        assert_eq!(glyph(b' '), [0; 16]);
    }

    #[test]
    fn a_frame_of_any_geometry_is_drawn_with_its_own_font_and_palette() {
        // Two columns of one 8-line row; glyph 1 is a solid top line.
        let mut glyphs = vec![0u8; GLYPH_PLANE_BYTES];
        glyphs[32] = 0xff;
        let mut palette = [[0u8; 3]; 16];
        palette[2] = [1, 2, 3];
        palette[9] = [7, 8, 9];
        let mut out = Vec::new();
        let frame = TextFrame {
            cells: &[1, 0x92, 0, 0x92],
            columns: 2,
            rows: 1,
            cell_height: 8,
            glyphs: &glyphs,
            palette: &palette,
            blink: false,
            cursor: Some((1, 0, 6, 7)),
        };
        assert_eq!(render_frame(&frame, &mut out), (16, 8));
        // The glyph's top line in the foreground, the next in the bright
        // background (no blink: four background bits).
        assert_eq!(&out[..4], &[1, 2, 3, 0xff]);
        assert_eq!(&out[16 * 4..16 * 4 + 4], &[7, 8, 9, 0xff]);
        // The cursor fills lines 6-7 of the second cell.
        let at = (6 * 16 + 8) * 4;
        assert_eq!(&out[at..at + 3], &[1, 2, 3]);
        // With blink the background keeps three bits: colour 1, black here.
        let mut blinking = Vec::new();
        render_frame(
            &TextFrame {
                blink: true,
                ..frame
            },
            &mut blinking,
        );
        assert_eq!(&blinking[16 * 4..16 * 4 + 3], &[0, 0, 0]);
        // A frame with no cells and wild geometry draws blank, in bounds.
        let wild = TextFrame {
            cells: &[],
            columns: 9999,
            rows: 9999,
            cell_height: 9999,
            glyphs: &[],
            palette: &palette,
            blink: false,
            cursor: Some((5000, 5000, 0, 9999)),
        };
        assert_eq!(render_frame(&wild, &mut out), (132 * 8, 60 * 32));
    }

    #[test]
    fn a_cell_is_drawn_in_its_colours() {
        let font = Font::default();
        // A full block, white on blue, in the top-left cell.
        let mut out = Vec::new();
        render(&font, &[0xdb, 0x1f, b' ', 0x1f], None, &mut out);
        assert_eq!(out.len(), (WIDTH * HEIGHT * 4) as usize);
        assert_eq!(&out[..4], &[0xff, 0xff, 0xff, 0xff]);
        // The space beside it is the blue background.
        assert_eq!(&out[8 * 4..8 * 4 + 4], &[0xaa, 0x00, 0x00, 0xff]);
    }
}
