//! The CPU reference for checks 6 and 7: the exact image the triangle shader
//! must produce, pixel for pixel.
//!
//! Exactness is possible because nothing in the picture is ambiguous. The
//! vertices sit on whole pixel coordinates (exact in float and in any
//! rasterizer's fixed-point sub-pixel grid), every edge and every region
//! boundary misses every pixel centre by at least 1/6 px (asserted by the unit
//! test below), and the shader writes only 0.0 and 1.0 per channel. The clear
//! colours are multiples of 0.2, which convert to UNORM8 without a rounding
//! tie.

pub const SIZE: u32 = 256;

/// Triangle vertices in pixel coordinates (x right, y down), region order:
/// A = red, B = green, C = blue.
pub const VERTS_PX: [(f64, f64); 3] = [(128.0, 16.0), (240.0, 240.0), (16.0, 240.0)];

pub const RED: [u8; 4] = [255, 0, 0, 255];
pub const GREEN: [u8; 4] = [0, 255, 0, 255];
pub const BLUE: [u8; 4] = [0, 0, 255, 255];

/// Check 6 clears to this, check 7 to [`CLEAR_7`], so an image left over from
/// one can never pass the other.
pub const CLEAR_6: [f32; 4] = [0.2, 0.4, 0.6, 1.0];
pub const CLEAR_7: [f32; 4] = [0.6, 0.4, 0.2, 1.0];

/// Probe points and what they sample: one inside each region, three outside.
pub const PROBES: [(u32, u32, &str); 6] = [
    (128, 60, "red region"),
    (200, 220, "green region"),
    (56, 220, "blue region"),
    (10, 10, "outside, top-left"),
    (250, 128, "outside, right"),
    (128, 250, "outside, below"),
];

/// The vertex buffer: clip-space position (x, y) then a one-hot barycentric.
pub fn vertex_data() -> [f32; 15] {
    let ndc = |(x, y): (f64, f64)| ((x / 128.0 - 1.0) as f32, (y / 128.0 - 1.0) as f32);
    let (ax, ay) = ndc(VERTS_PX[0]);
    let (bx, by) = ndc(VERTS_PX[1]);
    let (cx, cy) = ndc(VERTS_PX[2]);
    [
        ax, ay, 1.0, 0.0, 0.0, //
        bx, by, 0.0, 1.0, 0.0, //
        cx, cy, 0.0, 0.0, 1.0,
    ]
}

/// UNORM8 of a clear colour, the way the spec rounds (nearest).
pub fn clear_rgba8(clear: [f32; 4]) -> [u8; 4] {
    clear.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn edge(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// The colour at a sample position, or `None` outside the triangle.
pub fn shade(p: (f64, f64)) -> Option<[u8; 4]> {
    let [a, b, c] = VERTS_PX;
    let area = edge(a, b, c);
    let wa = edge(b, c, p) / area;
    let wb = edge(c, a, p) / area;
    let wc = edge(a, b, p) / area;
    if wa <= 0.0 || wb <= 0.0 || wc <= 0.0 {
        return None;
    }
    // Same order and ties as fs_main in shaders/triangle.wgsl.
    Some(if wa >= wb && wa >= wc {
        RED
    } else if wb >= wc {
        GREEN
    } else {
        BLUE
    })
}

/// The whole expected image, tightly packed RGBA8, row 0 at the top.
pub fn reference(clear: [f32; 4]) -> Vec<u8> {
    let bg = clear_rgba8(clear);
    let mut img = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = shade((f64::from(x) + 0.5, f64::from(y) + 0.5)).unwrap_or(bg);
            img.extend_from_slice(&px);
        }
    }
    img
}

/// FNV-1a, 64 bit.
pub fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// How many pixels of `img` are exactly `colour`.
pub fn count(img: &[u8], colour: [u8; 4]) -> usize {
    img.as_chunks::<4>()
        .0
        .iter()
        .filter(|p| **p == colour)
        .count()
}

pub fn pixel(img: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

/// Compares a rendered image against the reference: probes first (the
/// diagnosis a human reads), then every pixel, then the checksum.
pub fn verify(got: &[u8], clear: [f32; 4]) -> Result<String, String> {
    let want = reference(clear);
    if got.len() != want.len() {
        return Err(format!(
            "image is {} bytes, expected {}",
            got.len(),
            want.len()
        ));
    }
    let mut problems = Vec::new();
    for (x, y, what) in PROBES {
        let (g, w) = (pixel(got, x, y), pixel(&want, x, y));
        if g != w {
            problems.push(format!("({x},{y}) {what} = {g:?}, expected {w:?}"));
        }
    }
    let mut bad = 0usize;
    let mut first_bad = None;
    for y in 0..SIZE {
        for x in 0..SIZE {
            if pixel(got, x, y) != pixel(&want, x, y) {
                bad += 1;
                first_bad.get_or_insert((x, y));
            }
        }
    }
    let (gs, ws) = (checksum(got), checksum(&want));
    if bad == 0 && problems.is_empty() && gs == ws {
        let counts = [RED, GREEN, BLUE, clear_rgba8(clear)].map(|c| count(got, c));
        return Ok(format!(
            "256x256 exact, {} probes ok, px red/green/blue/clear={}/{}/{}/{}, fnv1a=0x{gs:016x}",
            PROBES.len(),
            counts[0],
            counts[1],
            counts[2],
            counts[3]
        ));
    }
    if let Some((x, y)) = first_bad {
        problems.push(format!(
            "{bad} of {} pixels differ, first at ({x},{y}) = {:?} expected {:?}",
            SIZE * SIZE,
            pixel(got, x, y),
            pixel(&want, x, y)
        ));
    }
    problems.push(format!("fnv1a got 0x{gs:016x} expected 0x{ws:016x}"));
    Err(problems.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No pixel centre is close enough to an edge or a region boundary for a
    /// correct GPU to disagree with the reference: nudging every sample by
    /// 0.1 px in any direction never changes its colour.
    #[test]
    fn every_pixel_centre_is_unambiguous() {
        let d = 0.1;
        let nudges = [
            (d, 0.0),
            (-d, 0.0),
            (0.0, d),
            (0.0, -d),
            (d, d),
            (-d, -d),
            (d, -d),
            (-d, d),
        ];
        for y in 0..SIZE {
            for x in 0..SIZE {
                let c = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                let at = shade(c);
                for (dx, dy) in nudges {
                    assert_eq!(
                        shade((c.0 + dx, c.1 + dy)),
                        at,
                        "pixel ({x},{y}) is ambiguous"
                    );
                }
            }
        }
    }

    #[test]
    fn probes_see_what_they_claim() {
        let img = reference(CLEAR_6);
        let bg = clear_rgba8(CLEAR_6);
        let expect = [RED, GREEN, BLUE, bg, bg, bg];
        for ((x, y, what), want) in PROBES.into_iter().zip(expect) {
            assert_eq!(pixel(&img, x, y), want, "{what}");
        }
        assert_eq!(bg, [51, 102, 153, 255]);
        assert_eq!(clear_rgba8(CLEAR_7), [153, 102, 51, 255]);
    }

    #[test]
    fn all_three_regions_are_substantial() {
        let img = reference(CLEAR_6);
        for c in [RED, GREEN, BLUE] {
            let n = count(&img, c);
            assert!(n > 5000, "{c:?} covers only {n} px");
        }
    }

    #[test]
    fn verify_accepts_the_reference_and_names_a_wrong_pixel() {
        let mut img = reference(CLEAR_6);
        assert!(verify(&img, CLEAR_6).is_ok());
        assert!(verify(&img, CLEAR_7).is_err());
        let i = ((60 * SIZE + 128) * 4) as usize;
        img[i] = 0;
        let err = verify(&img, CLEAR_6).unwrap_err();
        assert!(err.contains("(128,60) red region"), "{err}");
        assert!(err.contains("1 of 65536 pixels differ"), "{err}");
    }
}
