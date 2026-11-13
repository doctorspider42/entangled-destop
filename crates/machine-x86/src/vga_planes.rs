//! The VGA's display memory: four planes of 64 KiB behind the graphics
//! controller's read and write logic (ADR-0012).
//!
//! A byte the guest writes at `A000:xxxx` does not go to one place. It is
//! rotated, combined with the latches the last read loaded, masked bit by
//! bit, and stored in whichever of the four planes the sequencer's map mask
//! enables; a read loads the latches and returns one plane, or a colour
//! comparison across all four. The 16-colour modes Windows' boot screen and
//! GUI-mode Setup draw in are made of exactly this, so it cannot be plain
//! RAM: the machine takes the guest's mapping of the window away while a
//! mode needs the logic (`vmm_core::hv::RamTrap`) and every access lands
//! here.
//!
//! Modelled: write modes 0–3 with rotate, the four logical functions,
//! set/reset, the bit mask and the map mask; read modes 0 and 1; chain-4
//! (mode 13h) and odd/even addressing. Not modelled: the CGA shift
//! register modes.

/// Bytes in one plane.
pub const PLANE: usize = 0x1_0000;

/// The registers an access depends on.
#[derive(Debug, Clone, Copy)]
pub struct Controls {
    /// Sequencer 2: which planes a write reaches.
    pub map_mask: u8,
    /// Sequencer 4: bit 3 chain-4, bit 2 sequential (not odd/even).
    pub memory_mode: u8,
    /// Graphics controller 0–8.
    pub set_reset: u8,
    pub enable_set_reset: u8,
    pub colour_compare: u8,
    pub rotate_function: u8,
    pub read_map: u8,
    pub mode: u8,
    pub colour_dont_care: u8,
    pub bit_mask: u8,
}

/// The four planes and the latches.
#[derive(Clone)]
pub struct Planes {
    data: Vec<u8>,
    latch: [u8; 4],
}

impl std::fmt::Debug for Planes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planes")
            .field("latch", &self.latch)
            .finish_non_exhaustive()
    }
}

impl Default for Planes {
    fn default() -> Self {
        Self {
            data: vec![0; PLANE * 4],
            latch: [0; 4],
        }
    }
}

/// 0xff for a set bit, 0 for a clear one: one colour bit spread over a byte.
fn spread(bits: u8, plane: usize) -> u8 {
    if bits >> plane & 1 != 0 {
        0xff
    } else {
        0
    }
}

impl Planes {
    /// Zeroes every plane and the latches: a mode set's clear.
    pub fn clear(&mut self) {
        self.data.fill(0);
        self.latch = [0; 4];
    }

    /// One plane's bytes.
    pub fn plane(&self, plane: usize) -> &[u8] {
        let at = (plane & 3) * PLANE;
        &self.data[at..at + PLANE]
    }

    fn cell(&mut self, plane: usize, offset: usize) -> &mut u8 {
        &mut self.data[(plane & 3) * PLANE + (offset & (PLANE - 1))]
    }

    /// A guest read at `offset` into the window.
    pub fn read(&mut self, controls: &Controls, offset: usize) -> u8 {
        let offset = offset & (PLANE - 1);
        if controls.memory_mode & 0x08 != 0 {
            // Chain-4: the low two address bits pick the plane.
            return *self.cell(offset & 3, offset & !3);
        }
        for plane in 0..4 {
            self.latch[plane] = *self.cell(plane, offset);
        }
        if controls.mode & 0x08 != 0 {
            // Read mode 1: a bit is set where every plane that matters has
            // the compare colour's bit.
            let mut same = 0xffu8;
            for (plane, latch) in self.latch.iter().enumerate() {
                if controls.colour_dont_care >> plane & 1 != 0 {
                    same &= !(latch ^ spread(controls.colour_compare, plane));
                }
            }
            return same;
        }
        let mut plane = usize::from(controls.read_map & 3);
        if controls.memory_mode & 0x04 == 0 {
            // Odd/even: the address's low bit is the plane's low bit.
            plane = (plane & 2) | (offset & 1);
        }
        self.latch[plane]
    }

    /// A guest write at `offset` into the window.
    pub fn write(&mut self, controls: &Controls, offset: usize, value: u8) {
        let offset = offset & (PLANE - 1);
        let map_mask = controls.map_mask & 0x0f;
        if controls.memory_mode & 0x08 != 0 {
            let plane = offset & 3;
            if map_mask >> plane & 1 != 0 {
                *self.cell(plane, offset & !3) = value;
            }
            return;
        }
        if controls.memory_mode & 0x04 == 0 {
            // Odd/even: even addresses reach planes 0 and 2, odd 1 and 3.
            for plane in [offset & 1, (offset & 1) | 2] {
                if map_mask >> plane & 1 != 0 {
                    *self.cell(plane, offset & !1) = value;
                }
            }
            return;
        }
        let rotated = value.rotate_right(u32::from(controls.rotate_function & 7));
        let function = controls.rotate_function >> 3 & 3;
        let bit_mask = controls.bit_mask;
        let latch = self.latch;
        let combine = |source: u8, plane: usize| -> u8 {
            let mixed = match function {
                0 => source,
                1 => source & latch[plane],
                2 => source | latch[plane],
                _ => source ^ latch[plane],
            };
            (mixed & bit_mask) | (latch[plane] & !bit_mask)
        };
        let mut out = [0u8; 4];
        for (plane, slot) in out.iter_mut().enumerate() {
            *slot = match controls.mode & 3 {
                0 => {
                    let source = if controls.enable_set_reset >> plane & 1 != 0 {
                        spread(controls.set_reset, plane)
                    } else {
                        rotated
                    };
                    combine(source, plane)
                }
                // The latches, as they are: a copy.
                1 => latch[plane],
                // The value is a colour: one bit a plane.
                2 => combine(spread(value, plane), plane),
                // Set/reset through the rotated value as a mask.
                _ => {
                    let mask = rotated & bit_mask;
                    (spread(controls.set_reset, plane) & mask) | (latch[plane] & !mask)
                }
            };
        }
        for (plane, value) in out.into_iter().enumerate() {
            if map_mask >> plane & 1 != 0 {
                *self.cell(plane, offset) = value;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mode 12h's defaults: all planes, sequential addressing, write mode 0,
    /// every bit.
    fn planar() -> Controls {
        Controls {
            map_mask: 0x0f,
            memory_mode: 0x06,
            set_reset: 0,
            enable_set_reset: 0,
            colour_compare: 0,
            rotate_function: 0,
            read_map: 0,
            mode: 0,
            colour_dont_care: 0x0f,
            bit_mask: 0xff,
        }
    }

    fn pixel_colours(planes: &Planes, offset: usize) -> [u8; 8] {
        let mut out = [0u8; 8];
        for (bit, colour) in out.iter_mut().enumerate() {
            for plane in 0..4 {
                *colour |= (planes.plane(plane)[offset] >> (7 - bit) & 1) << plane;
            }
        }
        out
    }

    #[test]
    fn write_mode_0_with_set_reset_fills_a_colour_through_the_bit_mask() {
        let mut planes = Planes::default();
        // Colour 5 into the left four pixels of the byte at 3.
        let controls = Controls {
            set_reset: 5,
            enable_set_reset: 0x0f,
            bit_mask: 0xf0,
            ..planar()
        };
        planes.read(&controls, 3);
        planes.write(&controls, 3, 0x00);
        assert_eq!(pixel_colours(&planes, 3), [5, 5, 5, 5, 0, 0, 0, 0]);
        // The other four in colour 10 keep the first four: the latches.
        let controls = Controls {
            set_reset: 10,
            bit_mask: 0x0f,
            ..controls
        };
        planes.read(&controls, 3);
        planes.write(&controls, 3, 0x00);
        assert_eq!(pixel_colours(&planes, 3), [5, 5, 5, 5, 10, 10, 10, 10]);
    }

    #[test]
    fn write_mode_2_takes_the_value_as_a_colour_and_the_map_mask_limits_planes() {
        let mut planes = Planes::default();
        let controls = Controls {
            mode: 2,
            bit_mask: 0x81,
            ..planar()
        };
        planes.read(&controls, 0);
        planes.write(&controls, 0, 0x0f);
        assert_eq!(pixel_colours(&planes, 0), [15, 0, 0, 0, 0, 0, 0, 15]);
        // Only plane 2 enabled: colour 15 arrives as 4.
        let controls = Controls {
            map_mask: 0x04,
            bit_mask: 0xff,
            ..controls
        };
        planes.read(&controls, 9);
        planes.write(&controls, 9, 0x0f);
        assert_eq!(pixel_colours(&planes, 9), [4; 8]);
    }

    #[test]
    fn write_mode_1_copies_the_latches_and_mode_3_masks_with_the_value() {
        let mut planes = Planes::default();
        let fill = Controls {
            mode: 2,
            ..planar()
        };
        planes.read(&fill, 0);
        planes.write(&fill, 0, 0x09);
        // Mode 1: read the source, write the destination.
        let copy = Controls {
            mode: 1,
            ..planar()
        };
        planes.read(&copy, 0);
        planes.write(&copy, 100, 0xaa);
        assert_eq!(pixel_colours(&planes, 100), [9; 8]);
        // Mode 3: colour 6 where the written byte has ones.
        let masked = Controls {
            mode: 3,
            set_reset: 6,
            ..planar()
        };
        planes.read(&masked, 100);
        planes.write(&masked, 100, 0xc0);
        assert_eq!(pixel_colours(&planes, 100), [6, 6, 9, 9, 9, 9, 9, 9]);
    }

    #[test]
    fn the_logical_functions_and_the_rotate_apply_in_write_mode_0() {
        let mut planes = Planes::default();
        let base = planar();
        planes.read(&base, 0);
        planes.write(&base, 0, 0b1100_0000);
        // XOR with the value rotated right by one.
        let xor = Controls {
            rotate_function: 0x18 | 1,
            ..base
        };
        planes.read(&xor, 0);
        planes.write(&xor, 0, 0b1100_0000);
        assert_eq!(planes.plane(0)[0], 0b1100_0000 ^ 0b0110_0000);
    }

    #[test]
    fn read_mode_1_compares_colours_and_read_mode_0_picks_a_plane() {
        let mut planes = Planes::default();
        let fill = Controls {
            mode: 2,
            bit_mask: 0xf0,
            ..planar()
        };
        planes.read(&fill, 0);
        planes.write(&fill, 0, 0x0c);
        let compare = Controls {
            mode: 0x08,
            colour_compare: 0x0c,
            ..planar()
        };
        assert_eq!(planes.read(&compare, 0), 0xf0);
        // Plane 3 of colour 12 is set, plane 0 is not.
        assert_eq!(
            planes.read(
                &Controls {
                    read_map: 3,
                    ..planar()
                },
                0
            ),
            0xf0
        );
        assert_eq!(
            planes.read(
                &Controls {
                    read_map: 0,
                    ..planar()
                },
                0
            ),
            0x00
        );
    }

    #[test]
    fn chain_4_is_one_byte_a_pixel_and_offsets_wrap_in_the_plane() {
        let mut planes = Planes::default();
        let chained = Controls {
            memory_mode: 0x0e,
            ..planar()
        };
        for (address, value) in [(0usize, 0x11u8), (1, 0x22), (2, 0x33), (3, 0x44), (4, 0x55)] {
            planes.write(&chained, address, value);
        }
        assert_eq!(planes.read(&chained, 2), 0x33);
        assert_eq!(planes.read(&chained, 4), 0x55);
        assert_eq!(planes.plane(1)[0], 0x22);
        // An offset past the plane wraps instead of leaving it.
        planes.write(&planar(), PLANE + 5, 0xff);
        assert_eq!(planes.plane(0)[5], 0xff);
    }
}
