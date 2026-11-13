//! The whole path on a real host OpenGL: a stream in, a picture out.
//!
//! Self-skips on a host with no usable OpenGL (a CI runner with no GPU and
//! no software renderer), like the hypervisor tests without a hypervisor.

#![cfg(windows)]

use gl_passthrough::calls::op;
use gl_passthrough::{control, GlThread, Writer};

fn gl() -> Option<GlThread> {
    match GlThread::start() {
        Ok(gl) => Some(gl),
        Err(why) => {
            eprintln!("skipping: {why}");
            None
        }
    }
}

fn f(value: f32) -> u32 {
    value.to_bits()
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn pixel(present: &gl_passthrough::Present, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * present.width + x) * 4) as usize;
    // B, G, R, X → R, G, B.
    [
        present.pixels[at + 2],
        present.pixels[at + 1],
        present.pixels[at],
    ]
}

fn create(gl: &GlThread) -> u32 {
    let mut w = Writer::new();
    w.record(control::CONTEXT_CREATE, &[0]);
    let out = gl.run(w.words().to_vec());
    let id = word(&out.response);
    assert_ne!(id, 0, "no context");
    id
}

#[test]
fn an_immediate_mode_triangle_comes_back_as_pixels() {
    let Some(gl) = gl() else { return };
    let id = create(&gl);
    let mut w = Writer::new();
    w.record(control::SELECT, &[id, 64, 48])
        .record(op::ClearColor, &[f(0.0), f(0.0), f(1.0), f(1.0)])
        .record(op::Clear, &[0x4000])
        .record(op::Color3f, &[f(1.0), f(0.0), f(0.0)])
        .record(op::Begin, &[4])
        // Covers the bottom-left half of the window.
        .record(op::Vertex2f, &[f(-1.0), f(-1.0)])
        .record(op::Vertex2f, &[f(1.0), f(-1.0)])
        .record(op::Vertex2f, &[f(-1.0), f(1.0)])
        .record(op::End, &[])
        .record(control::SWAP, &[10, 20]);
    let out = gl.run(w.words().to_vec());
    let present = out.presents.first().expect("a picture");
    assert_eq!((present.x, present.y), (10, 20));
    assert_eq!((present.width, present.height), (64, 48));
    // Top row first: the bottom-left is the triangle, the top-right is not.
    assert_eq!(pixel(present, 4, 44), [255, 0, 0]);
    assert_eq!(pixel(present, 60, 3), [0, 0, 255]);
    assert_eq!(word(&out.response), 1, "the swap answers");
}

#[test]
fn a_textured_quad_from_vertex_arrays() {
    let Some(gl) = gl() else { return };
    let id = create(&gl);
    let mut w = Writer::new();
    w.record(control::SELECT, &[id, 32, 32]);
    w.record(op::GenTextures, &[1]);
    let out = gl.run(w.words().to_vec());
    let texture = word(&out.response);
    assert_ne!(texture, 0);

    const TEXTURE_2D: u32 = 0x0de1;
    const NEAREST: u32 = 0x2600;
    const REPLACE: u32 = 0x1e01;
    const RGBA: u32 = 0x1908;
    const UNSIGNED_BYTE: u32 = 0x1401;
    const FLOAT: u32 = 0x1406;
    // 2×2: green, green / white, white — bottom row first.
    let texels = [0xff00_ff00u32, 0xff00_ff00, 0xffff_ffff, 0xffff_ffff];
    let mut image = vec![TEXTURE_2D, 0, RGBA, 2, 2, 0, RGBA, UNSIGNED_BYTE, 1];
    image.extend_from_slice(&texels);
    let quad = [-1.0f32, -1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0];
    let coords = [0.0f32, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
    let mut draw = vec![7 /* GL_QUADS */, 4];
    draw.extend([1, 2, FLOAT]);
    draw.extend(quad.map(f));
    draw.extend([4, 2, FLOAT]);
    draw.extend(coords.map(f));

    let mut w = Writer::new();
    w.record(control::SELECT, &[id, 32, 32])
        .record(op::BindTexture, &[TEXTURE_2D, texture])
        .record(op::TexParameteri, &[TEXTURE_2D, 0x2800, NEAREST])
        .record(op::TexParameteri, &[TEXTURE_2D, 0x2801, NEAREST])
        .record(op::TexEnvi, &[0x2300, 0x2200, REPLACE])
        .record(op::TexImage2D, &image)
        .record(op::Enable, &[TEXTURE_2D])
        .record(control::DRAW_ARRAYS, &draw)
        .record(op::GetError, &[])
        .record(control::SWAP, &[0, 0]);
    let out = gl.run(w.words().to_vec());
    assert_eq!(word(&out.response), 0, "no OpenGL error");
    let present = out.presents.first().expect("a picture");
    // The texture's first row is the bottom of the quad.
    assert_eq!(pixel(present, 16, 28), [0, 255, 0]);
    assert_eq!(pixel(present, 16, 4), [255, 255, 255]);
}

#[test]
fn hostile_records_draw_nothing_and_break_nothing() {
    let Some(gl) = gl() else { return };
    let id = create(&gl);
    const FLOAT: u32 = 0x1406;
    let mut w = Writer::new();
    w.record(control::SELECT, &[id, 16, 16])
        .record(op::ClearColor, &[f(0.0), f(1.0), f(0.0), f(1.0)])
        .record(op::Clear, &[0x4000])
        // A million vertices, twelve bytes of them.
        .record(control::DRAW_ARRAYS, &[4, 1_000_000, 1, 3, FLOAT, 0, 0, 0])
        // An index past the vertices.
        .record(
            control::DRAW_ELEMENTS,
            &[4, 1, 3, 0, 0, 7, 1, 2, FLOAT, 0, 0],
        )
        // A texture that claims 4096×4096 and brings one word.
        .record(
            op::TexImage2D,
            &[0x0de1, 0, 0x1908, 4096, 4096, 0, 0x1908, 0x1401, 1, 0],
        )
        // A plain call with the wrong number of arguments, an opcode that
        // does not exist, and a context that does not either.
        .record(op::Vertex3f, &[0])
        .record(999, &[1, 2, 3])
        .record(control::CONTEXT_DESTROY, &[0xdead])
        .record(control::SWAP, &[0, 0]);
    let mut words = w.words().to_vec();
    // And the stream ends inside a record.
    words.extend([u32::from(op::Vertex3f) | 3 << 16, 0]);
    let out = gl.run(words);
    let present = out.presents.first().expect("a picture");
    assert_eq!(pixel(present, 8, 8), [0, 255, 0]);

    // Records before any SELECT, or for a context that is not there, are
    // not run at all.
    let mut w = Writer::new();
    w.record(op::Clear, &[0x4000])
        .record(control::SELECT, &[id + 100, 16, 16])
        .record(op::GetError, &[]);
    assert!(gl.run(w.words().to_vec()).response.is_empty());
}
