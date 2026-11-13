//! The VGA's registers, and text mode read out of them (ADR-0012).
//!
//! A guest that boots through a BIOS expects a VGA: Windows XP's `vga.sys`
//! probes the graphics controller before it believes there is a display,
//! then programs every register for the mode it wants, loads its own font
//! and moves the cursor through the CRT controller. This is the register
//! file that answers it — the sequencer, CRT controller, graphics
//! controller, attribute controller and DAC at `0x3c0..0x3df` — and what a
//! presenter needs to draw **text mode** from it: the geometry, the cursor,
//! the sixteen colours through the attribute palette and the DAC, and the
//! font.
//!
//! # Display memory
//!
//! Text mode's character cells are ordinary guest RAM at `B800:0000`, read
//! as they are. Anything else — a font load into plane 2, the 16-colour
//! graphics modes, mode 13h — goes through the planes and the graphics
//! controller's read and write logic (`crate::vga_planes`), which needs
//! every access: [`Vga::wants_trap`] says when, and the machine then takes
//! the guest's mapping of `0xa0000..0xbffff` away so that the accesses
//! arrive at [`Vga::mem_read`] and [`Vga::mem_write`]. On a host that
//! cannot do that, a font load lands in the RAM at `A000:0000` and is read
//! back from there, and graphics modes show nothing.
//!
//! The two views are not kept in step: text written before a graphics mode
//! is still in RAM after it, and what a graphics mode left in planes 0 and
//! 1 is not the text. Guests clear the screen when they change modes.
//!
//! Not modelled: the line compare (split screen), pixel panning, the CGA
//! shift-register modes, and the 9-dot character clock (cells are 8 wide).
//!
//! The guest is untrusted: every index is masked to its register file, and
//! the geometry a presenter gets is clamped.

/// The ports the VGA answers: the mono and colour CRT controller, the
/// attribute controller through the DAC, and the two status ports.
pub fn contains(port: u16) -> bool {
    matches!(
        port,
        0x3b4 | 0x3b5 | 0x3ba | 0x3c0..=0x3cf | 0x3d4 | 0x3d5 | 0x3da
    )
}

/// What a presenter needs to draw the text screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMode {
    pub columns: usize,
    pub rows: usize,
    /// Scan lines per character cell, 1..=32.
    pub cell_height: usize,
    /// Guest physical address of the first character cell.
    pub cells_at: u64,
    /// The cursor's cell and its first and last scan line, when it is on.
    pub cursor: Option<Cursor>,
    /// The sixteen colours, as B, G, R.
    pub palette: [[u8; 3]; 16],
    /// Whether attribute bit 7 means "blink" (the background then has three
    /// bits) rather than a bright background.
    pub blink: bool,
    /// The guest has loaded a font of its own ([`Vga::font_plane`]).
    pub guest_font: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub column: usize,
    pub row: usize,
    pub first_line: usize,
    pub last_line: usize,
}

/// The register file.
#[derive(Debug, Clone)]
pub struct Vga {
    misc: u8,
    sequencer_index: u8,
    sequencer: [u8; 8],
    crtc_index: u8,
    crtc: [u8; 32],
    graphics_index: u8,
    graphics: [u8; 16],
    attribute_index: u8,
    /// The attribute controller's flip-flop: the next write to `0x3c0` is
    /// data, not an index.
    attribute_data_next: bool,
    attribute: [u8; 32],
    dac_mask: u8,
    dac_write_index: u8,
    dac_read_index: u8,
    /// Which of red, green, blue the next DAC data access is.
    dac_component: u8,
    dac: [[u8; 3]; 256],
    /// Toggled on every read of the status register, so a guest waiting for
    /// a retrace to start or to end sees both.
    retrace: bool,
    /// The guest has programmed the CRT controller: its registers, not the
    /// BIOS data area, say where the cursor is.
    programmed: bool,
    /// The guest has written plane 2 through the sequencer: it loaded a font.
    font_loaded: bool,
    planes: crate::vga_planes::Planes,
    /// Something a frame depends on changed since [`Vga::take_dirty`].
    dirty: bool,
}

impl Default for Vga {
    fn default() -> Self {
        Self::new()
    }
}

/// The register values of one standard mode, as a video BIOS's mode table
/// holds them: the miscellaneous output register, sequencer 0-4, CRT
/// controller 0-24, graphics controller 0-8 and attribute controller 0-20.
struct ModeTable {
    misc: u8,
    sequencer: [u8; 5],
    crtc: [u8; 25],
    graphics: [u8; 9],
    attribute: [u8; 21],
}

/// The sixteen-colour attribute palette every standard mode starts with.
const EGA_PALETTE: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
];

const fn attributes(tail: [u8; 5], identity: bool) -> [u8; 21] {
    let mut out = [0u8; 21];
    let mut i = 0;
    while i < 16 {
        out[i] = if identity { i as u8 } else { EGA_PALETTE[i] };
        i += 1;
    }
    let mut i = 0;
    while i < 5 {
        out[16 + i] = tail[i];
        i += 1;
    }
    out
}

/// Mode 03h: 80x25 text, 16-line cells.
const MODE_03: ModeTable = ModeTable {
    misc: 0x67,
    sequencer: [0x03, 0x00, 0x03, 0x00, 0x02],
    crtc: [
        0x5f, 0x4f, 0x50, 0x82, 0x55, 0x81, 0xbf, 0x1f, 0x00, 0x4f, 0x0d, 0x0e, 0x00, 0x00, 0x00,
        0x00, 0x9c, 0x8e, 0x8f, 0x28, 0x1f, 0x96, 0xb9, 0xa3, 0xff,
    ],
    graphics: [0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x0e, 0x00, 0xff],
    attribute: attributes([0x0c, 0x00, 0x0f, 0x08, 0x00], false),
};

/// Mode 12h: 640x480, sixteen colours, planar.
const MODE_12: ModeTable = ModeTable {
    misc: 0xe3,
    sequencer: [0x03, 0x01, 0x0f, 0x00, 0x06],
    crtc: [
        0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0x0b, 0x3e, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xea, 0x8c, 0xdf, 0x28, 0x00, 0xe7, 0x04, 0xe3, 0xff,
    ],
    graphics: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x0f, 0xff],
    attribute: attributes([0x01, 0x00, 0x0f, 0x00, 0x00], false),
};

/// Mode 13h: 320x200, 256 colours, one byte a pixel.
const MODE_13: ModeTable = ModeTable {
    misc: 0x63,
    sequencer: [0x03, 0x01, 0x0f, 0x00, 0x0e],
    crtc: [
        0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0xbf, 0x1f, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x9c, 0x8e, 0x8f, 0x28, 0x40, 0x96, 0xb9, 0xa3, 0xff,
    ],
    graphics: [0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x05, 0x0f, 0xff],
    attribute: attributes([0x41, 0x00, 0x0f, 0x00, 0x00], true),
};

/// 6-bit DAC component to 8 bits.
fn expand(component: u8) -> u8 {
    let c = component & 0x3f;
    c << 2 | c >> 4
}

impl Vga {
    /// A VGA in the state a BIOS leaves it in: text mode 3.
    pub fn new() -> Self {
        let mut vga = Self {
            // Colour emulation (the CRT controller at 0x3d4), RAM enabled.
            misc: 0x67,
            sequencer_index: 0,
            sequencer: [0x03, 0x00, 0x03, 0x00, 0x02, 0, 0, 0],
            crtc_index: 0,
            crtc: [0; 32],
            graphics_index: 0,
            graphics: [0; 16],
            attribute_index: 0,
            attribute_data_next: false,
            attribute: [0; 32],
            dac_mask: 0xff,
            dac_write_index: 0,
            dac_read_index: 0,
            dac_component: 0,
            dac: [[0; 3]; 256],
            retrace: false,
            programmed: false,
            font_loaded: false,
            planes: crate::vga_planes::Planes::default(),
            dirty: true,
        };
        // 80x25 cells of 16 lines: 400 lines, the cursor on lines 13-14.
        vga.crtc[0x01] = 79;
        vga.crtc[0x07] = 0x1f;
        vga.crtc[0x09] = 0x4f;
        vga.crtc[0x0a] = 0x0d;
        vga.crtc[0x0b] = 0x0e;
        vga.crtc[0x12] = 0x8f;
        // Text, odd/even addressing, memory at B8000.
        vga.graphics[0x05] = 0x10;
        vga.graphics[0x06] = 0x0e;
        vga.graphics[0x08] = 0xff;
        // The EGA palette, and blink on.
        let palette = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d,
            0x3e, 0x3f,
        ];
        vga.attribute[..16].copy_from_slice(&palette);
        vga.attribute[0x10] = 0x0c;
        vga.attribute[0x12] = 0x0f;
        // The 64 EGA colours: two bits a component, the low one worth 1/3.
        for (index, entry) in vga.dac.iter_mut().take(64).enumerate() {
            let level = |high: usize, low: usize| {
                (((index >> high) & 1) * 0x2a + ((index >> low) & 1) * 0x15) as u8
            };
            *entry = [level(2, 5), level(1, 4), level(0, 3)];
        }
        vga
    }

    /// Power-on (ADR-0005).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The CRT controller is at `0x3d4` in colour emulation and `0x3b4` in
    /// mono; the other pair does not answer.
    fn crtc_decoded(&self, port: u16) -> bool {
        let colour = self.misc & 1 != 0;
        matches!(port, 0x3d4 | 0x3d5 | 0x3da) == colour
    }

    /// A guest read of one port.
    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            0x3c0 => self.attribute_index,
            0x3c1 => self.attribute[usize::from(self.attribute_index & 0x1f)],
            // Input status 0: the switch sense bit a colour monitor sets.
            0x3c2 => 0x10,
            0x3c4 => self.sequencer_index,
            0x3c5 => self.sequencer[usize::from(self.sequencer_index & 0x07)],
            0x3c6 => self.dac_mask,
            // DAC state: last access was a write (3) or a read (0).
            0x3c7 => 0x03,
            0x3c8 => self.dac_write_index,
            0x3c9 => {
                let value =
                    self.dac[usize::from(self.dac_read_index)][usize::from(self.dac_component % 3)];
                self.dac_component += 1;
                if self.dac_component >= 3 {
                    self.dac_component = 0;
                    self.dac_read_index = self.dac_read_index.wrapping_add(1);
                }
                value
            }
            0x3ca => 0,
            0x3cc => self.misc,
            0x3ce => self.graphics_index,
            0x3cf => self.graphics[usize::from(self.graphics_index & 0x0f)],
            0x3b4 | 0x3d4 if self.crtc_decoded(port) => self.crtc_index,
            0x3b5 | 0x3d5 if self.crtc_decoded(port) => {
                self.crtc[usize::from(self.crtc_index & 0x1f)]
            }
            0x3ba | 0x3da if self.crtc_decoded(port) => {
                // Reading the status resets the attribute flip-flop; bits 0
                // and 3 are "in retrace".
                self.attribute_data_next = false;
                self.retrace = !self.retrace;
                if self.retrace {
                    0x09
                } else {
                    0x00
                }
            }
            _ => 0xff,
        }
    }

    /// A guest write of one port.
    pub fn write(&mut self, port: u16, value: u8) {
        self.dirty = true;
        match port {
            0x3c0 => {
                if self.attribute_data_next {
                    self.attribute[usize::from(self.attribute_index & 0x1f)] = value;
                } else {
                    self.attribute_index = value;
                }
                self.attribute_data_next = !self.attribute_data_next;
            }
            0x3c2 => self.misc = value,
            0x3c4 => self.sequencer_index = value,
            0x3c5 => {
                self.sequencer[usize::from(self.sequencer_index & 0x07)] = value;
                // Plane 2 alone, addressed sequentially: a font load.
                if self.sequencer[2] & 0x0f == 0x04 && self.sequencer[4] & 0x04 != 0 {
                    self.font_loaded = true;
                }
            }
            0x3c6 => self.dac_mask = value,
            0x3c7 => {
                self.dac_read_index = value;
                self.dac_component = 0;
            }
            0x3c8 => {
                self.dac_write_index = value;
                self.dac_component = 0;
            }
            0x3c9 => {
                self.dac[usize::from(self.dac_write_index)][usize::from(self.dac_component % 3)] =
                    value & 0x3f;
                self.dac_component += 1;
                if self.dac_component >= 3 {
                    self.dac_component = 0;
                    self.dac_write_index = self.dac_write_index.wrapping_add(1);
                }
            }
            0x3ce => self.graphics_index = value,
            0x3cf => self.graphics[usize::from(self.graphics_index & 0x0f)] = value,
            0x3b4 | 0x3d4 if self.crtc_decoded(port) => self.crtc_index = value,
            0x3b5 | 0x3d5 if self.crtc_decoded(port) => {
                let index = usize::from(self.crtc_index & 0x1f);
                // Registers 0-7 are write-protected by bit 7 of register
                // 0x11, except the line-compare bit of the overflow.
                if index <= 7 && self.crtc[0x11] & 0x80 != 0 {
                    if index == 7 {
                        self.crtc[7] = (self.crtc[7] & !0x10) | (value & 0x10);
                    }
                } else {
                    self.crtc[index] = value;
                }
                self.programmed = true;
            }
            _ => {}
        }
    }

    /// Sets a standard mode the way a video BIOS does, from its mode
    /// table: every register of the mode, the default palette, and — unless
    /// `clear` is off — empty display memory. `false` for a mode with no
    /// table here (only 03h, 12h and 13h have one).
    ///
    /// A text mode set this way leaves the screen the BIOS's to describe
    /// ([`Self::programmed`] is false again): the BIOS keeps its cursor in
    /// its own data area, not in the CRT controller.
    pub fn set_bios_mode(&mut self, mode: u8, clear: bool) -> bool {
        let table = match mode {
            0x03 => &MODE_03,
            0x12 => &MODE_12,
            0x13 => &MODE_13,
            _ => return false,
        };
        self.misc = table.misc;
        self.sequencer[..5].copy_from_slice(&table.sequencer);
        self.crtc[..25].copy_from_slice(&table.crtc);
        self.graphics[..9].copy_from_slice(&table.graphics);
        self.attribute[..21].copy_from_slice(&table.attribute);
        self.attribute_data_next = false;
        self.dac_mask = 0xff;
        let fresh = Self::new();
        self.dac[..64].copy_from_slice(&fresh.dac[..64]);
        if clear && mode != 0x03 {
            self.planes.clear();
        }
        self.programmed = mode != 0x03;
        self.dirty = true;
        true
    }

    /// Whether display memory needs its logic now: a graphics mode, or
    /// sequential addressing (a font load). In plain text mode — odd/even
    /// addressing, alphanumeric — the window is RAM.
    pub fn wants_trap(&self) -> bool {
        self.graphics[0x06] & 1 != 0 || self.sequencer[0x04] & 0x04 != 0
    }

    fn controls(&self) -> crate::vga_planes::Controls {
        crate::vga_planes::Controls {
            map_mask: self.sequencer[0x02],
            memory_mode: self.sequencer[0x04],
            set_reset: self.graphics[0x00],
            enable_set_reset: self.graphics[0x01],
            colour_compare: self.graphics[0x02],
            rotate_function: self.graphics[0x03],
            read_map: self.graphics[0x04],
            mode: self.graphics[0x05],
            colour_dont_care: self.graphics[0x07],
            bit_mask: self.graphics[0x08],
        }
    }

    /// Where `addr` falls in the window the memory map select decodes, if
    /// it does.
    fn window_offset(&self, addr: u64) -> Option<usize> {
        let (base, len) = match self.graphics[0x06] >> 2 & 3 {
            0 => (0x000a_0000u64, 0x2_0000u64),
            1 => (0x000a_0000, 0x1_0000),
            2 => (0x000b_0000, 0x8000),
            _ => (0x000b_8000, 0x8000),
        };
        addr.checked_sub(base)
            .filter(|offset| *offset < len)
            .map(|offset| offset as usize)
    }

    /// A guest read of display memory, while it is trapped.
    pub fn mem_read(&mut self, addr: u64) -> u8 {
        let controls = self.controls();
        match self.window_offset(addr) {
            Some(offset) => self.planes.read(&controls, offset),
            None => 0xff,
        }
    }

    /// A guest write of display memory, while it is trapped.
    pub fn mem_write(&mut self, addr: u64, value: u8) {
        let controls = self.controls();
        if let Some(offset) = self.window_offset(addr) {
            self.planes.write(&controls, offset, value);
            self.dirty = true;
        }
    }

    /// The font the guest loaded into plane 2, in the layout
    /// `pc_bios::font::render_frame` takes: 256 glyphs of 32 bytes. `None`
    /// until a load went through the planes.
    pub fn font_plane(&self) -> Option<&[u8]> {
        let plane = &self.planes.plane(2)[..256 * 32];
        (self.font_loaded && plane.iter().any(|&b| b != 0)).then_some(plane)
    }

    /// The registers and how much of each plane is non-zero, on one line:
    /// for a log, when a screen stays black and the question is which mode
    /// the guest thinks it set.
    pub fn summary(&self) -> String {
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let used: Vec<usize> = (0..4)
            .map(|p| self.planes.plane(p).iter().filter(|&&b| b != 0).count())
            .collect();
        format!(
            "misc={:02x} seq={} crtc={} gc={} ac={} mask={:02x} dac0-15={} planes-nonzero={used:?}",
            self.misc,
            hex(&self.sequencer[..5]),
            hex(&self.crtc[..25]),
            hex(&self.graphics[..9]),
            hex(&self.attribute[..21]),
            self.dac_mask,
            self.dac[..16]
                .iter()
                .map(|c| hex(c))
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    /// Whether anything changed since the last call, and forgets it.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Draws the graphics mode the registers describe as BGRA into `out`
    /// and returns its width and height; `None` in text mode.
    pub fn render_graphics(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        if self.text_mode().is_some() {
            return None;
        }
        let overflow = usize::from(self.crtc[0x07]);
        let lines =
            (usize::from(self.crtc[0x12]) | (overflow & 0x02) << 7 | (overflow & 0x40) << 3) + 1;
        // A row of pixels is repeated: the scan-line count, and double scan.
        let repeat = (usize::from(self.crtc[0x09] & 0x1f) + 1)
            * if self.crtc[0x09] & 0x80 != 0 { 2 } else { 1 };
        let height = (lines / repeat).clamp(1, 1024);
        let characters = (usize::from(self.crtc[0x01]) + 1).clamp(1, 160);
        let start = usize::from(self.crtc[0x0c]) << 8 | usize::from(self.crtc[0x0d]);
        let row_bytes = match usize::from(self.crtc[0x13]) * 2 {
            0 => characters,
            n => n,
        };
        let wrap = crate::vga_planes::PLANE - 1;
        let packed = self.graphics[0x05] & 0x40 != 0;
        let width = if packed {
            characters * 4
        } else {
            characters * 8
        };
        out.clear();
        out.resize(width * height * 4, 0xff);
        if packed {
            // 256 colours: one byte a pixel, spread over the planes.
            for y in 0..height {
                for x in 0..width {
                    let address = start * 4 + y * row_bytes * 4 + x;
                    let index = self.planes.plane(address & 3)[address & !3 & wrap];
                    let [r, g, b] = self.dac[usize::from(index & self.dac_mask)];
                    let at = (y * width + x) * 4;
                    out[at..at + 3].copy_from_slice(&[expand(b), expand(g), expand(r)]);
                }
            }
        } else {
            let mut palette = [[0u8; 3]; 16];
            for (index, entry) in palette.iter_mut().enumerate() {
                *entry = self.colour(index);
            }
            let planes: [&[u8]; 4] = std::array::from_fn(|p| self.planes.plane(p));
            for y in 0..height {
                let row = start + y * row_bytes;
                for byte in 0..characters {
                    let offset = (row + byte) & wrap;
                    let bits = [
                        planes[0][offset],
                        planes[1][offset],
                        planes[2][offset],
                        planes[3][offset],
                    ];
                    for bit in 0..8 {
                        let shift = 7 - bit;
                        let colour = usize::from(
                            (bits[0] >> shift & 1)
                                | (bits[1] >> shift & 1) << 1
                                | (bits[2] >> shift & 1) << 2
                                | (bits[3] >> shift & 1) << 3,
                        );
                        let at = (y * width + byte * 8 + bit) * 4;
                        out[at..at + 3].copy_from_slice(&palette[colour]);
                    }
                }
            }
        }
        Some((width as u32, height as u32))
    }

    /// Whether the guest has programmed the display itself; until then the
    /// BIOS's idea of the screen (its data area) is the one to draw.
    pub fn programmed(&self) -> bool {
        self.programmed
    }

    /// One of the sixteen colours a text attribute names, as B, G, R.
    fn colour(&self, index: usize) -> [u8; 3] {
        let mode = self.attribute[0x10];
        let select = self.attribute[0x14];
        let mut entry = self.attribute[index & 0x0f] & 0x3f;
        if mode & 0x80 != 0 {
            entry = (entry & 0x0f) | (select & 0x03) << 4;
        }
        let entry = (entry | (select & 0x0c) << 4) & self.dac_mask;
        let [r, g, b] = self.dac[usize::from(entry)];
        [expand(b), expand(g), expand(r)]
    }

    /// The text screen as the registers describe it, or `None` in a graphics
    /// mode.
    pub fn text_mode(&self) -> Option<TextMode> {
        if self.graphics[0x06] & 1 != 0 || self.attribute[0x10] & 1 != 0 {
            return None;
        }
        let cell_height = usize::from(self.crtc[0x09] & 0x1f) + 1;
        let overflow = usize::from(self.crtc[0x07]);
        let lines =
            (usize::from(self.crtc[0x12]) | (overflow & 0x02) << 7 | (overflow & 0x40) << 3) + 1;
        let columns = (usize::from(self.crtc[0x01]) + 1).clamp(1, 132);
        let rows = (lines / cell_height).clamp(1, 60);
        // The memory map select: where the cells are.
        let base = match self.graphics[0x06] >> 2 & 3 {
            0 | 1 => 0x000a_0000u64,
            2 => 0x000b_0000,
            _ => 0x000b_8000,
        };
        let start = usize::from(self.crtc[0x0c]) << 8 | usize::from(self.crtc[0x0d]);
        let location = usize::from(self.crtc[0x0e]) << 8 | usize::from(self.crtc[0x0f]);
        let cursor_off = self.crtc[0x0a] & 0x20 != 0;
        let cursor = location
            .checked_sub(start)
            .filter(|cell| !cursor_off && *cell < columns * rows)
            .map(|cell| Cursor {
                column: cell % columns,
                row: cell / columns,
                first_line: usize::from(self.crtc[0x0a] & 0x1f),
                last_line: usize::from(self.crtc[0x0b] & 0x1f),
            });
        let mut palette = [[0u8; 3]; 16];
        for (index, entry) in palette.iter_mut().enumerate() {
            *entry = self.colour(index);
        }
        Some(TextMode {
            columns,
            rows,
            cell_height,
            // The start address counts cells; a cell is two bytes.
            cells_at: base + (start as u64 * 2).min(0x7fff),
            cursor,
            palette,
            blink: self.attribute[0x10] & 0x08 != 0,
            guest_font: self.font_loaded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_indexed(vga: &mut Vga, port: u16, index: u8, value: u8) {
        vga.write(port, index);
        vga.write(port + 1, value);
    }

    #[test]
    fn the_power_on_state_is_text_mode_three() {
        let vga = Vga::new();
        let text = vga.text_mode().expect("text mode");
        assert_eq!((text.columns, text.rows, text.cell_height), (80, 25, 16));
        assert_eq!(text.cells_at, 0xb8000);
        assert!(!text.guest_font);
        assert!(text.blink);
        // Colour 1 is blue, colour 15 white, as B, G, R.
        assert_eq!(text.palette[1], [0xaa, 0x00, 0x00]);
        assert_eq!(text.palette[15], [0xff, 0xff, 0xff]);
        assert_eq!(text.palette[6], [0x00, 0x55, 0xaa]);
        assert!(!vga.programmed());
    }

    #[test]
    fn the_graphics_controller_reads_back_what_a_probe_writes() {
        // What a driver does before it believes in a VGA.
        let mut vga = Vga::new();
        for (index, value) in [(0x04u8, 0x03u8), (0x08, 0x55), (0x08, 0xaa)] {
            write_indexed(&mut vga, 0x3ce, index, value);
            assert_eq!(vga.read(0x3ce), index);
            assert_eq!(vga.read(0x3cf), value);
        }
        write_indexed(&mut vga, 0x3c4, 0x04, 0x06);
        assert_eq!(vga.read(0x3c5), 0x06);
        assert_eq!(vga.read(0x3cc), 0x67);
    }

    #[test]
    fn the_cursor_follows_the_crt_controller() {
        let mut vga = Vga::new();
        // Cell 80 * 3 + 7.
        write_indexed(&mut vga, 0x3d4, 0x0e, 0);
        write_indexed(&mut vga, 0x3d4, 0x0f, 247);
        assert!(vga.programmed());
        let cursor = vga.text_mode().unwrap().cursor.expect("a cursor");
        assert_eq!((cursor.column, cursor.row), (7, 3));
        assert_eq!((cursor.first_line, cursor.last_line), (13, 14));
        // Bit 5 of the cursor start register turns it off.
        write_indexed(&mut vga, 0x3d4, 0x0a, 0x20);
        assert_eq!(vga.text_mode().unwrap().cursor, None);
        // The mono ports do not answer in colour emulation.
        assert_eq!(vga.read(0x3b5), 0xff);
    }

    #[test]
    fn fifty_lines_and_a_loaded_font_are_reported() {
        let mut vga = Vga::new();
        // Unlock registers 0-7, then 8-line cells.
        write_indexed(&mut vga, 0x3d4, 0x11, 0x00);
        write_indexed(&mut vga, 0x3d4, 0x09, 0x47);
        assert_eq!(vga.text_mode().unwrap().rows, 50);
        // The font load: plane 2, sequential addressing.
        write_indexed(&mut vga, 0x3c4, 0x02, 0x04);
        write_indexed(&mut vga, 0x3c4, 0x04, 0x07);
        write_indexed(&mut vga, 0x3c4, 0x02, 0x03);
        write_indexed(&mut vga, 0x3c4, 0x04, 0x03);
        assert!(vga.text_mode().unwrap().guest_font);
    }

    #[test]
    fn the_attribute_flip_flop_and_the_dac_take_their_sequences() {
        let mut vga = Vga::new();
        // Palette entry 1 becomes DAC colour 0x3f; the status read resets
        // the flip-flop first.
        vga.read(0x3da);
        vga.write(0x3c0, 0x01);
        vga.write(0x3c0, 0x3f);
        assert_eq!(vga.text_mode().unwrap().palette[1], [0xff, 0xff, 0xff]);
        // DAC colour 0x3f becomes pure red.
        vga.write(0x3c8, 0x3f);
        for component in [0x3f, 0x00, 0x00] {
            vga.write(0x3c9, component);
        }
        assert_eq!(vga.text_mode().unwrap().palette[1], [0x00, 0x00, 0xff]);
        vga.write(0x3c7, 0x3f);
        assert_eq!(
            [vga.read(0x3c9), vga.read(0x3c9), vga.read(0x3c9)],
            [0x3f, 0, 0]
        );
    }

    /// The registers of mode 12h that matter here: 640x480, planar.
    fn mode_12h() -> Vga {
        let mut vga = Vga::new();
        write_indexed(&mut vga, 0x3d4, 0x11, 0x00);
        for (index, value) in [
            (0x01u8, 0x4fu8),
            (0x07, 0x3e),
            (0x09, 0x40),
            (0x12, 0xdf),
            (0x13, 0x28),
        ] {
            write_indexed(&mut vga, 0x3d4, index, value);
        }
        write_indexed(&mut vga, 0x3c4, 0x02, 0x0f);
        write_indexed(&mut vga, 0x3c4, 0x04, 0x06);
        write_indexed(&mut vga, 0x3ce, 0x05, 0x00);
        write_indexed(&mut vga, 0x3ce, 0x06, 0x05);
        write_indexed(&mut vga, 0x3ce, 0x08, 0xff);
        vga.read(0x3da);
        vga.write(0x3c0, 0x10);
        vga.write(0x3c0, 0x01);
        vga
    }

    #[test]
    fn mode_12h_draws_what_was_written_through_the_planes() {
        let mut vga = mode_12h();
        assert!(vga.wants_trap());
        assert_eq!(vga.text_mode(), None);
        // Write mode 2: the byte is a colour. Red (4) in the first eight
        // pixels of the second row, through the window at A0000.
        write_indexed(&mut vga, 0x3ce, 0x05, 0x02);
        vga.mem_read(0xa0000 + 80);
        vga.mem_write(0xa0000 + 80, 0x04);
        assert!(vga.take_dirty());
        let mut out = Vec::new();
        assert_eq!(vga.render_graphics(&mut out), Some((640, 480)));
        let pixel = |x: usize, y: usize| &out[(y * 640 + x) * 4..(y * 640 + x) * 4 + 4];
        assert_eq!(pixel(0, 1), &[0x00, 0x00, 0xaa, 0xff]);
        assert_eq!(pixel(7, 1), &[0x00, 0x00, 0xaa, 0xff]);
        assert_eq!(pixel(8, 1), &[0x00, 0x00, 0x00, 0xff]);
        assert_eq!(pixel(0, 0), &[0x00, 0x00, 0x00, 0xff]);
        // An address outside the decoded window (B0000 with map 1) is not
        // display memory.
        assert_eq!(vga.mem_read(0xb0000), 0xff);
        assert!(!vga.take_dirty());
    }

    #[test]
    fn the_bios_mode_tables_give_the_standard_modes() {
        let mut vga = Vga::new();
        assert!(vga.set_bios_mode(0x12, true));
        assert!(vga.wants_trap() && vga.programmed());
        assert_eq!(vga.render_graphics(&mut Vec::new()), Some((640, 480)));
        // Something drawn survives a mode set that asks not to clear, and
        // not one that does.
        vga.mem_write(0xa0000, 0xff);
        assert!(vga.set_bios_mode(0x12, false));
        assert_eq!(vga.mem_read(0xa0000), 0xff);
        assert!(vga.set_bios_mode(0x12, true));
        assert_eq!(vga.mem_read(0xa0000), 0x00);
        assert!(vga.set_bios_mode(0x13, true));
        assert_eq!(vga.render_graphics(&mut Vec::new()), Some((320, 200)));
        // Back to text: RAM again, and the BIOS's screen to describe.
        assert!(vga.set_bios_mode(0x03, true));
        assert!(!vga.wants_trap() && !vga.programmed());
        let text = vga.text_mode().expect("text");
        assert_eq!((text.columns, text.rows, text.cell_height), (80, 25, 16));
        assert!(!vga.set_bios_mode(0x6a, true));
    }

    #[test]
    fn a_font_load_goes_to_plane_two_and_text_mode_is_ram_again_after() {
        let mut vga = Vga::new();
        assert!(!vga.wants_trap());
        assert_eq!(vga.font_plane(), None);
        // What a driver does to load a font: plane 2, sequential, map A0000.
        write_indexed(&mut vga, 0x3c4, 0x02, 0x04);
        write_indexed(&mut vga, 0x3c4, 0x04, 0x07);
        write_indexed(&mut vga, 0x3ce, 0x05, 0x00);
        write_indexed(&mut vga, 0x3ce, 0x06, 0x04);
        assert!(vga.wants_trap());
        vga.mem_write(0xa0000 + 65 * 32, 0x18);
        // And back to text addressing.
        write_indexed(&mut vga, 0x3c4, 0x02, 0x03);
        write_indexed(&mut vga, 0x3c4, 0x04, 0x03);
        write_indexed(&mut vga, 0x3ce, 0x05, 0x10);
        write_indexed(&mut vga, 0x3ce, 0x06, 0x0e);
        assert!(!vga.wants_trap());
        assert_eq!(vga.font_plane().expect("the font")[65 * 32], 0x18);
        assert_eq!(vga.render_graphics(&mut Vec::new()), None);
    }

    #[test]
    fn a_graphics_mode_is_not_text_and_wild_registers_stay_in_bounds() {
        let mut vga = Vga::new();
        write_indexed(&mut vga, 0x3ce, 0x06, 0x05);
        assert_eq!(vga.text_mode(), None);
        let mut vga = Vga::new();
        write_indexed(&mut vga, 0x3d4, 0x11, 0x00);
        for index in 0..=0xffu8 {
            write_indexed(&mut vga, 0x3d4, index, 0xff);
            write_indexed(&mut vga, 0x3c4, index, 0xff);
            write_indexed(&mut vga, 0x3ce, index, 0xff);
        }
        // The graphics controller now says "graphics": back to text.
        write_indexed(&mut vga, 0x3ce, 0x06, 0x0e);
        vga.read(0x3da);
        vga.write(0x3c0, 0x10);
        vga.write(0x3c0, 0x00);
        // Every wild register, in a graphics mode: the picture is bounded.
        let mut wild = vga.clone();
        write_indexed(&mut wild, 0x3ce, 0x06, 0x05);
        let mut out = Vec::new();
        let (width, height) = wild.render_graphics(&mut out).expect("graphics");
        assert!(width <= 160 * 8 && height <= 1024);
        assert_eq!(out.len(), (width * height * 4) as usize);
        wild.mem_write(0xbffff, 0xff);
        wild.mem_read(u64::MAX);
        let text = vga.text_mode().expect("text");
        assert!(text.columns <= 132 && text.rows <= 60 && text.cell_height <= 32);
        assert!(text.cells_at < 0xc0000);
    }
}
