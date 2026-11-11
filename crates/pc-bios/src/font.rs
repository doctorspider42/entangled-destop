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

/// The glyphs of all 256 codes, built once by whoever draws.
pub struct Font(Box<[[u8; 16]; 256]>);

impl Default for Font {
    fn default() -> Self {
        let mut glyphs = Box::new([[0u8; 16]; 256]);
        for (code, slot) in glyphs.iter_mut().enumerate() {
            *slot = glyph(code as u8);
        }
        Self(glyphs)
    }
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
        let glyph = &font.0[usize::from(cell[0])];
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
