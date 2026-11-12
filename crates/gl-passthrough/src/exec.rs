//! Runs a stream's records on the host's OpenGL.
//!
//! [`Executor`] owns the guest's contexts and what each draws into: a
//! framebuffer object the size of the guest's window, because the host has
//! no window of its own for it. It must live on one thread — OpenGL contexts
//! are bound to threads — which is what [`crate::GlThread`] is for.
//!
//! # Vertex arrays
//!
//! OpenGL 1.1's vertex arrays are pointers into the program's memory that
//! OpenGL reads later, at the draw. The guest's driver keeps the pointers to
//! itself and sends, with each draw, the vertices that draw will read:
//!
//! ```text
//! DRAW_ARRAYS:   mode, count, array…
//! DRAW_ELEMENTS: mode, vertices, indices, index[indices], array…
//! array:         kind, size, type, data[count × size × sizeof(type), padded]
//! ```
//!
//! Here the arrays are checked against the count (and every index against
//! the vertex count), pointed at, drawn from, and switched off again before
//! the record's memory goes away. Should setting a pointer fail — which
//! would leave OpenGL holding the previous draw's pointer — the draw is not
//! made: the error flag is read to find out, and what it held before is kept
//! for the guest's `glGetError`.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::Arc;

use crate::api::{self, Api};
use crate::calls::{self, op};
use crate::sizes;
use crate::{bytes_of, control, Outcome, Present, Record, Records, MAX_CONTEXTS, MAX_DRAWABLE};
use crate::{f64_at, MAX_RESPONSE};

/// The host side of contexts: the one part that differs per platform.
pub trait Platform {
    type Context;

    /// The functions, valid for every context this platform creates.
    fn api(&self) -> Arc<Api>;
    /// A new context, sharing objects with `share` if given.
    fn create(&mut self, share: Option<&Self::Context>) -> Option<Self::Context>;
    fn destroy(&mut self, context: Self::Context);
    /// Makes `context` current on this thread, or none.
    fn make_current(&mut self, context: Option<&Self::Context>) -> bool;
    /// `wglCopyContext`.
    fn copy(&mut self, from: &Self::Context, to: &Self::Context, mask: u32) -> bool;
}

const NO_ERROR: u32 = 0;
const NONE: u32 = 0;
const BACK: u32 = 0x0405;
const DRAW_BUFFER: u32 = 0x0c01;
const READ_BUFFER: u32 = 0x0c02;
const DOUBLEBUFFER: u32 = 0x0c32;
const PACK_ALIGNMENT: u32 = 0x0d05;
const UNPACK_ALIGNMENT: u32 = 0x0cf5;
const VENDOR: u32 = 0x1f00;
const RENDERER: u32 = 0x1f01;
const VERSION: u32 = 0x1f02;
const EXTENSIONS: u32 = 0x1f03;
const VERTEX_ARRAY: u32 = 0x8074;
const NORMAL_ARRAY: u32 = 0x8075;
const COLOR_ARRAY: u32 = 0x8076;
const TEXTURE_COORD_ARRAY: u32 = 0x8078;
const PIXEL_MODE_BIT: u32 = 0x20;
const MAP_COLOR: u32 = 0x0d10;
const RED_SCALE: u32 = 0x0d14;
const ALPHA_BIAS: u32 = 0x0d1d;

/// How many values the host's `glGet*` may write for one name: far more
/// than any name of OpenGL 1.1 (16) or of what hosts have added since.
const GET_ROOM: usize = 1024;
/// How many of them go back to the guest.
const GET_SENT: usize = 16;
/// The most names `glGenTextures` makes at once.
const MAX_GEN: u32 = 4096;

struct Drawable {
    framebuffer: u32,
    color: u32,
    depth: u32,
    width: u32,
    height: u32,
}

struct Slot<C> {
    context: C,
    drawable: Option<Drawable>,
    /// An OpenGL error this crate read off the flag on the guest's behalf.
    held_error: u32,
}

/// The guest's contexts, and the means to run its records on them.
pub struct Executor<P: Platform> {
    platform: P,
    gl: Arc<Api>,
    slots: HashMap<u32, Slot<P::Context>>,
    next_id: u32,
    /// The context current on this thread, by id.
    current: Option<u32>,
}

struct Arrays<'a> {
    vertex: Option<(u32, u32, &'a [u8])>,
    normal: Option<(u32, &'a [u8])>,
    color: Option<(u32, u32, &'a [u8])>,
    texcoord: Option<(u32, u32, &'a [u8])>,
}

/// The arrays of a draw of `count` vertices, or `None` if any of them is not
/// something `gl*Pointer` would take or does not hold `count` vertices.
fn parse_arrays(mut a: &[u32], count: u32) -> Option<Arrays<'_>> {
    use sizes::{BYTE, DOUBLE, FLOAT, INT, SHORT, UNSIGNED_BYTE, UNSIGNED_INT, UNSIGNED_SHORT};
    let mut arrays = Arrays {
        vertex: None,
        normal: None,
        color: None,
        texcoord: None,
    };
    while !a.is_empty() {
        let (&[kind, size, ty], rest) = a.split_first_chunk::<3>()?;
        let bytes = sizes::array_bytes(count, size, ty)?;
        let words = sizes::words_for(bytes);
        if words > rest.len() {
            return None;
        }
        let (data, rest) = rest.split_at(words);
        let data = &bytes_of(data)[..bytes];
        a = rest;
        // The sizes and types each `gl*Pointer` accepts — so that none of
        // those calls can fail and leave an older pointer in place.
        match kind {
            1 if (2..=4).contains(&size) && matches!(ty, SHORT | INT | FLOAT | DOUBLE) => {
                arrays.vertex = Some((size, ty, data));
            }
            2 if size == 3 && matches!(ty, BYTE | SHORT | INT | FLOAT | DOUBLE) => {
                arrays.normal = Some((ty, data));
            }
            3 if (3..=4).contains(&size)
                && matches!(
                    ty,
                    BYTE | UNSIGNED_BYTE
                        | SHORT
                        | UNSIGNED_SHORT
                        | INT
                        | UNSIGNED_INT
                        | FLOAT
                        | DOUBLE
                ) =>
            {
                arrays.color = Some((size, ty, data));
            }
            4 if (1..=4).contains(&size) && matches!(ty, SHORT | INT | FLOAT | DOUBLE) => {
                arrays.texcoord = Some((size, ty, data));
            }
            _ => return None,
        }
    }
    arrays.vertex.is_some().then_some(arrays)
}

fn floats(a: &[u32]) -> [f32; 16] {
    let mut out = [0.0; 16];
    for (out, word) in out.iter_mut().zip(a) {
        *out = f32::from_bits(*word);
    }
    out
}

fn ints(a: &[u32]) -> [i32; 16] {
    let mut out = [0; 16];
    for (out, word) in out.iter_mut().zip(a) {
        *out = *word as i32;
    }
    out
}

fn doubles(a: &[u32]) -> [f64; 16] {
    let mut out = [0.0; 16];
    for (out, pair) in out.iter_mut().zip(a.chunks_exact(2)) {
        *out = f64_at(pair, 0);
    }
    out
}

/// The record's bytes from word `from` on, if there are at least `bytes`.
fn payload(a: &[u32], from: usize, bytes: usize) -> Option<&[u8]> {
    let data = bytes_of(a.get(from..)?);
    (data.len() >= bytes).then(|| &data[..bytes])
}

fn answer(out: &mut Outcome, words: &[u32]) {
    if out.response.len() + words.len() * 4 <= MAX_RESPONSE {
        out.response.extend_from_slice(bytes_of(words));
    }
}

fn answer_bytes(out: &mut Outcome, bytes: &[u8]) {
    let padded = sizes::words_for(bytes.len()) * 4;
    if out.response.len() + padded <= MAX_RESPONSE {
        out.response.extend_from_slice(bytes);
        out.response
            .resize(out.response.len() + padded - bytes.len(), 0);
    }
}

/// `GL_BACK`, `GL_FRONT` and the rest all mean the one colour buffer the
/// off-screen drawable has.
fn buffer_for(mode: u32) -> u32 {
    if mode == NONE {
        NONE
    } else {
        api::COLOR_ATTACHMENT0
    }
}

impl<P: Platform> Executor<P> {
    pub fn new(platform: P) -> Self {
        let gl = platform.api();
        Self {
            platform,
            gl,
            slots: HashMap::new(),
            next_id: 1,
            current: None,
        }
    }

    /// How many contexts the guest holds.
    pub fn contexts(&self) -> usize {
        self.slots.len()
    }

    /// Runs one buffer. Never fails: a record that makes no sense is
    /// skipped, and a stream that ends inside a record ends there.
    pub fn run(&mut self, words: &[u32]) -> Outcome {
        let mut out = Outcome::default();
        // No context is selected until the buffer says which: buffers from
        // different guest programs interleave.
        let mut selected = false;
        for record in Records::new(words) {
            let Ok(record) = record else {
                tracing::debug!("OpenGL command stream truncated");
                break;
            };
            if record.opcode >= control::BASE {
                self.control(record, &mut selected, &mut out);
            } else if selected {
                self.call(record, &mut out);
            }
        }
        out
    }

    /// Destroys every context: the guest was reset.
    pub fn reset(&mut self) {
        self.platform.make_current(None);
        self.current = None;
        for (_, slot) in self.slots.drain() {
            self.platform.destroy(slot.context);
        }
    }

    fn control(&mut self, record: Record<'_>, selected: &mut bool, out: &mut Outcome) {
        let a = record.args;
        match (record.opcode, a) {
            (control::CONTEXT_CREATE, &[share]) => {
                let id = self.create(share).unwrap_or(0);
                answer(out, &[id]);
            }
            (control::CONTEXT_DESTROY, &[id]) => {
                if self.current == Some(id) {
                    self.platform.make_current(None);
                    self.current = None;
                    *selected = false;
                }
                if let Some(slot) = self.slots.remove(&id) {
                    self.platform.destroy(slot.context);
                }
            }
            (control::SELECT, &[id, width, height]) => {
                *selected = self.select(id, width, height);
            }
            (control::SWAP, &[x, y]) => {
                if *selected {
                    if let Some(present) = self.read_back(x as i32, y as i32) {
                        out.presents.push(present);
                    }
                }
                answer(out, &[1]);
            }
            (control::DRAW_ARRAYS, &[mode, count, ref arrays @ ..]) => {
                if *selected {
                    if let Some(arrays) = parse_arrays(arrays, count) {
                        self.draw(mode, count, None, &arrays);
                    }
                }
            }
            (control::DRAW_ELEMENTS, &[mode, vertices, indices, ref rest @ ..]) => {
                if *selected && indices <= sizes::MAX_VERTICES && indices as usize <= rest.len() {
                    let (index, arrays) = rest.split_at(indices as usize);
                    if index.iter().all(|&i| i < vertices) {
                        if let Some(arrays) = parse_arrays(arrays, vertices) {
                            self.draw(mode, vertices, Some(index), &arrays);
                        }
                    }
                }
            }
            (control::CONTEXT_COPY, &[from, to, mask]) => {
                if let (Some(from), Some(to)) = (self.slots.get(&from), self.slots.get(&to)) {
                    self.platform.copy(&from.context, &to.context, mask);
                }
            }
            _ => tracing::debug!(opcode = record.opcode, "malformed control record"),
        }
    }

    fn create(&mut self, share: u32) -> Option<u32> {
        if self.slots.len() >= MAX_CONTEXTS {
            return None;
        }
        let share = match share {
            0 => None,
            id => Some(&self.slots.get(&id)?.context),
        };
        let context = self.platform.create(share)?;
        let id = self.next_id;
        // Never 0, never one in use.
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        while self.slots.contains_key(&self.next_id) || self.next_id == 0 {
            self.next_id = self.next_id.wrapping_add(1);
        }
        self.slots.insert(
            id,
            Slot {
                context,
                drawable: None,
                held_error: NO_ERROR,
            },
        );
        Some(id)
    }

    fn select(&mut self, id: u32, width: u32, height: u32) -> bool {
        let Some(slot) = self.slots.get_mut(&id) else {
            return false;
        };
        if self.current != Some(id) {
            if !self.platform.make_current(Some(&slot.context)) {
                self.current = None;
                return false;
            }
            self.current = Some(id);
        }
        let width = width.clamp(1, MAX_DRAWABLE);
        let height = height.clamp(1, MAX_DRAWABLE);
        let gl = &*self.gl;
        let fresh = slot.drawable.is_none();
        let drawable = slot.drawable.get_or_insert_with(|| {
            let mut names = [0u32; 3];
            // SAFETY: the context is current; each call writes one name
            // into the array element it is given.
            unsafe {
                (gl.GenFramebuffers)(1, &mut names[0]);
                (gl.GenRenderbuffers)(2, names[1..].as_mut_ptr());
            }
            Drawable {
                framebuffer: names[0],
                color: names[1],
                depth: names[2],
                width: 0,
                height: 0,
            }
        });
        if drawable.width != width || drawable.height != height {
            // SAFETY: the context is current and these take numbers only.
            let complete = unsafe {
                (gl.BindFramebuffer)(api::FRAMEBUFFER, drawable.framebuffer);
                (gl.BindRenderbuffer)(api::RENDERBUFFER, drawable.color);
                (gl.RenderbufferStorage)(
                    api::RENDERBUFFER,
                    api::RGBA8,
                    width as i32,
                    height as i32,
                );
                (gl.BindRenderbuffer)(api::RENDERBUFFER, drawable.depth);
                (gl.RenderbufferStorage)(
                    api::RENDERBUFFER,
                    api::DEPTH24_STENCIL8,
                    width as i32,
                    height as i32,
                );
                (gl.BindRenderbuffer)(api::RENDERBUFFER, 0);
                (gl.FramebufferRenderbuffer)(
                    api::FRAMEBUFFER,
                    api::COLOR_ATTACHMENT0,
                    api::RENDERBUFFER,
                    drawable.color,
                );
                (gl.FramebufferRenderbuffer)(
                    api::FRAMEBUFFER,
                    api::DEPTH_ATTACHMENT,
                    api::RENDERBUFFER,
                    drawable.depth,
                );
                (gl.FramebufferRenderbuffer)(
                    api::FRAMEBUFFER,
                    api::STENCIL_ATTACHMENT,
                    api::RENDERBUFFER,
                    drawable.depth,
                );
                (gl.CheckFramebufferStatus)(api::FRAMEBUFFER) == api::FRAMEBUFFER_COMPLETE
            };
            if !complete {
                tracing::warn!(width, height, "host OpenGL framebuffer is incomplete");
                return false;
            }
            drawable.width = width;
            drawable.height = height;
            if fresh {
                // What a context finds on its first window: a viewport and
                // scissor of the window's size. And the pixel-store state
                // every size in `sizes` assumes, for good — no opcode
                // changes it.
                // SAFETY: the context is current; numbers only.
                unsafe {
                    (gl.Viewport)(0, 0, width as i32, height as i32);
                    (gl.Scissor)(0, 0, width as i32, height as i32);
                    (gl.PixelStorei)(PACK_ALIGNMENT, 1);
                    (gl.PixelStorei)(UNPACK_ALIGNMENT, 1);
                }
            }
        }
        true
    }

    fn slot(&mut self) -> Option<&mut Slot<P::Context>> {
        self.slots.get_mut(&self.current?)
    }

    /// The picture in the current context's drawable.
    fn read_back(&mut self, x: i32, y: i32) -> Option<Present> {
        let gl = self.gl.clone();
        let drawable = self.slot()?.drawable.as_ref()?;
        let (width, height) = (drawable.width, drawable.height);
        let row = width as usize * 4;
        let mut pixels = vec![0u8; row * height as usize];
        // SAFETY: the context is current. `glReadPixels` writes `width` ×
        // `height` pixels of four bytes with pack alignment 1 — exactly
        // `pixels`. The attribute push and pop put the guest's read buffer
        // and pixel-transfer state aside so the picture is read as drawn.
        unsafe {
            (gl.PushAttrib)(PIXEL_MODE_BIT);
            (gl.ReadBuffer)(api::COLOR_ATTACHMENT0);
            (gl.PixelTransferi)(MAP_COLOR, 0);
            for name in RED_SCALE..=ALPHA_BIAS {
                // Scales and biases alternate: RED_SCALE, RED_BIAS, ….
                (gl.PixelTransferf)(
                    name,
                    if (name - RED_SCALE) % 2 == 0 {
                        1.0
                    } else {
                        0.0
                    },
                );
            }
            (gl.ReadPixels)(
                0,
                0,
                width as i32,
                height as i32,
                sizes::BGRA,
                sizes::UNSIGNED_BYTE,
                pixels.as_mut_ptr().cast::<c_void>(),
            );
            (gl.PopAttrib)();
        }
        // OpenGL's first row is the bottom one.
        let mut flipped = Vec::with_capacity(pixels.len());
        for line in pixels.chunks_exact(row).rev() {
            flipped.extend_from_slice(line);
        }
        Some(Present {
            x,
            y,
            width,
            height,
            pixels: flipped,
        })
    }

    /// Reads the host's error flag into the slot, keeping the oldest error.
    fn hold_error(&mut self) {
        let gl = self.gl.clone();
        // SAFETY: the context is current; no arguments.
        let error = unsafe { (gl.GetError)() };
        if let Some(slot) = self.slot() {
            if slot.held_error == NO_ERROR {
                slot.held_error = error;
            }
        }
    }

    fn draw(&mut self, mode: u32, count: u32, index: Option<&[u32]>, arrays: &Arrays<'_>) {
        let gl = self.gl.clone();
        let Some((size, ty, vertex)) = arrays.vertex else {
            return;
        };
        self.hold_error();
        // SAFETY: the context is current. Each pointer is to a slice
        // `parse_arrays` sized for `count` elements of that size and type,
        // alive until this function returns; the states are disabled again
        // below, before it does.
        let pointed = unsafe {
            (gl.VertexPointer)(size as i32, ty, 0, vertex.as_ptr().cast());
            (gl.EnableClientState)(VERTEX_ARRAY);
            if let Some((ty, data)) = arrays.normal {
                (gl.NormalPointer)(ty, 0, data.as_ptr().cast());
                (gl.EnableClientState)(NORMAL_ARRAY);
            }
            if let Some((size, ty, data)) = arrays.color {
                (gl.ColorPointer)(size as i32, ty, 0, data.as_ptr().cast());
                (gl.EnableClientState)(COLOR_ARRAY);
            }
            if let Some((size, ty, data)) = arrays.texcoord {
                (gl.TexCoordPointer)(size as i32, ty, 0, data.as_ptr().cast());
                (gl.EnableClientState)(TEXTURE_COORD_ARRAY);
            }
            (gl.GetError)() == NO_ERROR
        };
        if pointed {
            // SAFETY: every enabled array holds `count` vertices, and every
            // index was checked to be below `count`.
            unsafe {
                match index {
                    Some(index) => (gl.DrawElements)(
                        mode,
                        index.len() as i32,
                        sizes::UNSIGNED_INT,
                        index.as_ptr().cast(),
                    ),
                    None => (gl.DrawArrays)(mode, 0, count as i32),
                }
            }
        }
        // SAFETY: the context is current; numbers only.
        unsafe {
            for state in [VERTEX_ARRAY, NORMAL_ARRAY, COLOR_ARRAY, TEXTURE_COORD_ARRAY] {
                (gl.DisableClientState)(state);
            }
        }
    }

    fn call(&mut self, record: Record<'_>, out: &mut Outcome) {
        let a = record.args;
        let plain = calls::PLAIN_WORDS
            .get(usize::from(record.opcode))
            .copied()
            .flatten();
        if let Some(words) = plain {
            if a.len() == usize::from(words) {
                // SAFETY: a context is current (`run` checked), and the
                // argument count is the one the function takes.
                unsafe { calls::call_plain(&self.gl, record.opcode, a) };
            }
            return;
        }
        self.by_hand(record.opcode, a, out);
    }

    #[allow(clippy::too_many_lines)]
    fn by_hand(&mut self, opcode: u16, a: &[u32], out: &mut Outcome) {
        let gl = self.gl.clone();
        macro_rules! host {
            ($($body:tt)*) => {{
                // SAFETY: a context is current. Where a
                // pointer goes in for OpenGL to read, it is to an array of this
                // function's holding at least as many elements as any name makes
                // OpenGL read (16 for the parameter families, 16 for matrices), or
                // to a slice `payload` checked against the size `sizes` computed
                // from the same arguments. Where OpenGL writes, it writes into
                // `GET_ROOM` elements, or into a buffer of the computed size.
                #[allow(unused_unsafe)]
                unsafe { $($body)* }
            }};
        }
        match (opcode, a) {
            (op::GetError, []) => {
                let held = self.slot().map_or(NO_ERROR, |slot| {
                    std::mem::replace(&mut slot.held_error, NO_ERROR)
                });
                let error = if held == NO_ERROR {
                    host! { (gl.GetError)() }
                } else {
                    held
                };
                answer(out, &[error]);
            }
            (op::GetString, &[name]) => {
                let text: Vec<u8> = match name {
                    VENDOR => b"Entangled Desktop".to_vec(),
                    VERSION => b"1.1.0".to_vec(),
                    EXTENSIONS => Vec::new(),
                    RENDERER => {
                        let host = host! { (gl.GetString)(RENDERER) };
                        let mut text = b"Entangled OpenGL on ".to_vec();
                        if host.is_null() {
                            text.extend_from_slice(b"the host");
                        } else {
                            // SAFETY: a non-null answer of `glGetString` is
                            // a NUL-terminated string the host's OpenGL
                            // keeps alive.
                            let host = unsafe { CStr::from_ptr(host.cast()) };
                            text.extend(host.to_bytes().iter().take(200));
                        }
                        text
                    }
                    _ => Vec::new(),
                };
                let mut text = text;
                text.push(0);
                answer_bytes(out, &text);
            }
            (op::GetIntegerv, &[name]) => {
                let mut values = [0i32; GET_ROOM];
                host! { (gl.GetIntegerv)(name, values.as_mut_ptr()) };
                match name {
                    // The guest never hears of the framebuffer object.
                    DRAW_BUFFER | READ_BUFFER if values[0] as u32 == api::COLOR_ATTACHMENT0 => {
                        values[0] = BACK as i32;
                    }
                    DOUBLEBUFFER => values[0] = 1,
                    _ => {}
                }
                let words: Vec<u32> = values[..GET_SENT].iter().map(|v| *v as u32).collect();
                answer(out, &words);
            }
            (op::GetFloatv, &[name]) => {
                let mut values = [0f32; GET_ROOM];
                host! { (gl.GetFloatv)(name, values.as_mut_ptr()) };
                if matches!(name, DRAW_BUFFER | READ_BUFFER)
                    && values[0] == api::COLOR_ATTACHMENT0 as f32
                {
                    values[0] = BACK as f32;
                }
                let words: Vec<u32> = values[..GET_SENT].iter().map(|v| v.to_bits()).collect();
                answer(out, &words);
            }
            (op::GetBooleanv, &[name]) => {
                let mut values = [0u8; GET_ROOM];
                host! { (gl.GetBooleanv)(name, values.as_mut_ptr()) };
                if name == DOUBLEBUFFER {
                    values[0] = 1;
                }
                let words: Vec<u32> = values[..GET_SENT].iter().map(|v| u32::from(*v)).collect();
                answer(out, &words);
            }
            (op::GetDoublev, &[name]) => {
                let mut values = [0f64; GET_ROOM];
                host! { (gl.GetDoublev)(name, values.as_mut_ptr()) };
                let mut words = Vec::with_capacity(GET_SENT * 2);
                for value in &values[..GET_SENT] {
                    let bits = value.to_bits();
                    words.push(bits as u32);
                    words.push((bits >> 32) as u32);
                }
                answer(out, &words);
            }
            (op::IsEnabled, &[cap]) => {
                answer(out, &[u32::from(host! { (gl.IsEnabled)(cap) })]);
            }
            (op::IsList, &[list]) => answer(out, &[u32::from(host! { (gl.IsList)(list) })]),
            (op::IsTexture, &[texture]) => {
                answer(out, &[u32::from(host! { (gl.IsTexture)(texture) })]);
            }
            (op::GenLists, &[range]) => {
                answer(out, &[host! { (gl.GenLists)(range as i32) }]);
            }
            (op::GenTextures, &[count]) => {
                let count = count.min(MAX_GEN);
                let mut names = vec![0u32; count as usize];
                // Writes `count` names into `count` elements.
                host! { (gl.GenTextures)(count as i32, names.as_mut_ptr()) };
                answer(out, &names);
            }
            (op::DeleteTextures, names) if names.len() <= MAX_GEN as usize => host! {
                (gl.DeleteTextures)(names.len() as i32, names.as_ptr());
            },
            (op::Flush, []) => host! { (gl.Flush)() },
            (op::Finish, []) => {
                host! { (gl.Finish)() };
                answer(out, &[1]);
            }
            (op::DrawBuffer, &[mode]) => host! { (gl.DrawBuffer)(buffer_for(mode)) },
            (op::ReadBuffer, &[mode]) => host! { (gl.ReadBuffer)(buffer_for(mode)) },
            (op::LoadMatrixf, m) if m.len() == 16 => host! {
                (gl.LoadMatrixf)(floats(m).as_ptr());
            },
            (op::MultMatrixf, m) if m.len() == 16 => host! {
                (gl.MultMatrixf)(floats(m).as_ptr());
            },
            (op::LoadMatrixd, m) if m.len() == 32 => host! {
                (gl.LoadMatrixd)(doubles(m).as_ptr());
            },
            (op::MultMatrixd, m) if m.len() == 32 => host! {
                (gl.MultMatrixd)(doubles(m).as_ptr());
            },
            (op::Fogfv, &[name, ref v @ ..]) => host! { (gl.Fogfv)(name, floats(v).as_ptr()) },
            (op::Fogiv, &[name, ref v @ ..]) => host! { (gl.Fogiv)(name, ints(v).as_ptr()) },
            (op::LightModelfv, &[name, ref v @ ..]) => host! {
                (gl.LightModelfv)(name, floats(v).as_ptr());
            },
            (op::LightModeliv, &[name, ref v @ ..]) => host! {
                (gl.LightModeliv)(name, ints(v).as_ptr());
            },
            (op::Lightfv, &[x, name, ref v @ ..]) => host! {
                (gl.Lightfv)(x, name, floats(v).as_ptr());
            },
            (op::Lightiv, &[x, name, ref v @ ..]) => host! {
                (gl.Lightiv)(x, name, ints(v).as_ptr());
            },
            (op::Materialfv, &[x, name, ref v @ ..]) => host! {
                (gl.Materialfv)(x, name, floats(v).as_ptr());
            },
            (op::Materialiv, &[x, name, ref v @ ..]) => host! {
                (gl.Materialiv)(x, name, ints(v).as_ptr());
            },
            (op::TexParameterfv, &[x, name, ref v @ ..]) => host! {
                (gl.TexParameterfv)(x, name, floats(v).as_ptr());
            },
            (op::TexParameteriv, &[x, name, ref v @ ..]) => host! {
                (gl.TexParameteriv)(x, name, ints(v).as_ptr());
            },
            (op::TexEnvfv, &[x, name, ref v @ ..]) => host! {
                (gl.TexEnvfv)(x, name, floats(v).as_ptr());
            },
            (op::TexEnviv, &[x, name, ref v @ ..]) => host! {
                (gl.TexEnviv)(x, name, ints(v).as_ptr());
            },
            (op::TexGenfv, &[x, name, ref v @ ..]) => host! {
                (gl.TexGenfv)(x, name, floats(v).as_ptr());
            },
            (op::TexGeniv, &[x, name, ref v @ ..]) => host! {
                (gl.TexGeniv)(x, name, ints(v).as_ptr());
            },
            (op::TexGendv, &[x, name, ref v @ ..]) => host! {
                (gl.TexGendv)(x, name, doubles(v).as_ptr());
            },
            (op::ClipPlane, &[plane, ref v @ ..]) if v.len() == 8 => host! {
                (gl.ClipPlane)(plane, doubles(v).as_ptr());
            },
            (op::PolygonStipple, mask) if mask.len() == 32 => host! {
                (gl.PolygonStipple)(bytes_of(mask).as_ptr());
            },
            (op::CallLists, lists) if lists.len() <= sizes::MAX_VERTICES as usize => host! {
                (gl.CallLists)(
                    lists.len() as i32,
                    sizes::UNSIGNED_INT,
                    lists.as_ptr().cast(),
                );
            },
            (
                op::TexImage2D,
                &[target, level, internal, width, height, border, format, ty, has, ..],
            ) => {
                let pixels = if has == 0 {
                    Some(std::ptr::null())
                } else {
                    sizes::image_bytes(width, height, format, ty)
                        .and_then(|bytes| payload(a, 9, bytes))
                        .map(|data| data.as_ptr().cast::<c_void>())
                };
                if let Some(pixels) = pixels {
                    host! {
                        (gl.TexImage2D)(
                            target,
                            level as i32,
                            internal as i32,
                            width as i32,
                            height as i32,
                            border as i32,
                            format,
                            ty,
                            pixels,
                        );
                    }
                }
            }
            (op::TexImage1D, &[target, level, internal, width, border, format, ty, has, ..]) => {
                let pixels = if has == 0 {
                    Some(std::ptr::null())
                } else {
                    sizes::image_bytes(width, 1, format, ty)
                        .and_then(|bytes| payload(a, 8, bytes))
                        .map(|data| data.as_ptr().cast::<c_void>())
                };
                if let Some(pixels) = pixels {
                    host! {
                        (gl.TexImage1D)(
                            target,
                            level as i32,
                            internal as i32,
                            width as i32,
                            border as i32,
                            format,
                            ty,
                            pixels,
                        );
                    }
                }
            }
            (op::TexSubImage2D, &[target, level, x, y, width, height, format, ty, ..]) => {
                if let Some(data) = sizes::image_bytes(width, height, format, ty)
                    .and_then(|bytes| payload(a, 8, bytes))
                {
                    host! {
                        (gl.TexSubImage2D)(
                            target,
                            level as i32,
                            x as i32,
                            y as i32,
                            width as i32,
                            height as i32,
                            format,
                            ty,
                            data.as_ptr().cast(),
                        );
                    }
                }
            }
            (op::TexSubImage1D, &[target, level, x, width, format, ty, ..]) => {
                if let Some(data) =
                    sizes::image_bytes(width, 1, format, ty).and_then(|bytes| payload(a, 6, bytes))
                {
                    host! {
                        (gl.TexSubImage1D)(
                            target,
                            level as i32,
                            x as i32,
                            width as i32,
                            format,
                            ty,
                            data.as_ptr().cast(),
                        );
                    }
                }
            }
            (op::DrawPixels, &[width, height, format, ty, ..]) => {
                if let Some(data) = sizes::image_bytes(width, height, format, ty)
                    .and_then(|bytes| payload(a, 4, bytes))
                {
                    host! {
                        (gl.DrawPixels)(
                            width as i32,
                            height as i32,
                            format,
                            ty,
                            data.as_ptr().cast(),
                        );
                    }
                }
            }
            (op::ReadPixels, &[x, y, width, height, format, ty]) => {
                let room = MAX_RESPONSE - out.response.len().min(MAX_RESPONSE);
                if let Some(bytes) =
                    sizes::image_bytes(width, height, format, ty).filter(|bytes| *bytes <= room)
                {
                    let mut pixels = vec![0u8; bytes];
                    host! {
                        (gl.ReadPixels)(
                            x as i32,
                            y as i32,
                            width as i32,
                            height as i32,
                            format,
                            ty,
                            pixels.as_mut_ptr().cast(),
                        );
                    }
                    answer_bytes(out, &pixels);
                }
            }
            (op::Bitmap, &[width, height, xorig, yorig, xmove, ymove, has, ..]) => {
                let bits = if has == 0 {
                    Some(std::ptr::null())
                } else {
                    sizes::bitmap_bytes(width, height)
                        .and_then(|bytes| payload(a, 7, bytes))
                        .map(<[u8]>::as_ptr)
                };
                if let Some(bits) = bits {
                    host! {
                        (gl.Bitmap)(
                            width as i32,
                            height as i32,
                            f32::from_bits(xorig),
                            f32::from_bits(yorig),
                            f32::from_bits(xmove),
                            f32::from_bits(ymove),
                            bits,
                        );
                    }
                }
            }
            (
                op::GetLightfv
                | op::GetMaterialfv
                | op::GetTexEnvfv
                | op::GetTexGenfv
                | op::GetTexParameterfv,
                &[x, name],
            ) => {
                let mut values = [0f32; GET_ROOM];
                let get = match opcode {
                    op::GetLightfv => gl.GetLightfv,
                    op::GetMaterialfv => gl.GetMaterialfv,
                    op::GetTexEnvfv => gl.GetTexEnvfv,
                    op::GetTexGenfv => gl.GetTexGenfv,
                    _ => gl.GetTexParameterfv,
                };
                host! { get(x, name, values.as_mut_ptr()) };
                answer(
                    out,
                    &[values[0], values[1], values[2], values[3]].map(f32::to_bits),
                );
            }
            (
                op::GetLightiv
                | op::GetMaterialiv
                | op::GetTexEnviv
                | op::GetTexGeniv
                | op::GetTexParameteriv,
                &[x, name],
            ) => {
                let mut values = [0i32; GET_ROOM];
                let get = match opcode {
                    op::GetLightiv => gl.GetLightiv,
                    op::GetMaterialiv => gl.GetMaterialiv,
                    op::GetTexEnviv => gl.GetTexEnviv,
                    op::GetTexGeniv => gl.GetTexGeniv,
                    _ => gl.GetTexParameteriv,
                };
                host! { get(x, name, values.as_mut_ptr()) };
                answer(
                    out,
                    &[values[0], values[1], values[2], values[3]].map(|v| v as u32),
                );
            }
            (op::GetTexLevelParameterfv, &[target, level, name]) => {
                let mut values = [0f32; GET_ROOM];
                host! {
                    (gl.GetTexLevelParameterfv)(target, level as i32, name, values.as_mut_ptr());
                }
                answer(
                    out,
                    &[values[0], values[1], values[2], values[3]].map(f32::to_bits),
                );
            }
            (op::GetTexLevelParameteriv, &[target, level, name]) => {
                let mut values = [0i32; GET_ROOM];
                host! {
                    (gl.GetTexLevelParameteriv)(target, level as i32, name, values.as_mut_ptr());
                }
                answer(
                    out,
                    &[values[0], values[1], values[2], values[3]].map(|v| v as u32),
                );
            }
            _ => tracing::trace!(opcode, words = a.len(), "OpenGL record not run"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sizes::{FLOAT, UNSIGNED_BYTE};

    fn array(kind: u32, size: u32, ty: u32, data: &[u32]) -> Vec<u32> {
        let mut words = vec![kind, size, ty];
        words.extend_from_slice(data);
        words
    }

    #[test]
    fn arrays_must_hold_every_vertex_the_draw_reads() {
        // Three vertices of three floats: nine words.
        let ok = array(1, 3, FLOAT, &[0; 9]);
        assert!(parse_arrays(&ok, 3).is_some());
        // One word short.
        let short = array(1, 3, FLOAT, &[0; 8]);
        assert!(parse_arrays(&short, 3).is_none());
        // The same data is not four vertices.
        assert!(parse_arrays(&ok, 4).is_none());
    }

    #[test]
    fn arrays_gl_would_refuse_are_refused_first() {
        // glVertexPointer takes no unsigned bytes, and no size of 1.
        assert!(parse_arrays(&array(1, 3, UNSIGNED_BYTE, &[0; 3]), 3).is_none());
        assert!(parse_arrays(&array(1, 1, FLOAT, &[0; 3]), 3).is_none());
        // A normal has three components.
        let mut both = array(1, 3, FLOAT, &[0; 9]);
        both.extend(array(2, 4, FLOAT, &[0; 12]));
        assert!(parse_arrays(&both, 3).is_none());
        // An unknown kind, and a draw with no vertex array at all.
        assert!(parse_arrays(&array(9, 3, FLOAT, &[0; 9]), 3).is_none());
        assert!(parse_arrays(&array(3, 4, UNSIGNED_BYTE, &[0; 3]), 3).is_none());
        // A header cut short.
        assert!(parse_arrays(&[1, 3], 0).is_none());
    }

    #[test]
    fn colours_ride_along_packed_to_words() {
        let mut both = array(1, 2, FLOAT, &[0; 6]);
        // Three RGBA byte colours: twelve bytes, three words.
        both.extend(array(3, 4, UNSIGNED_BYTE, &[0x4433_2211; 3]));
        let arrays = parse_arrays(&both, 3).unwrap();
        let (size, ty, data) = arrays.color.unwrap();
        assert_eq!((size, ty, data.len()), (4, UNSIGNED_BYTE, 12));
        assert_eq!(&data[..4], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn parameters_are_padded_never_overread() {
        assert_eq!(floats(&[1.0f32.to_bits()])[..2], [1.0, 0.0]);
        assert_eq!(ints(&[u32::MAX; 40])[15], -1);
        let d = doubles(&[0, 0x3ff0_0000, 7]);
        assert_eq!(d[0], 1.0);
        assert_eq!(d[1], 0.0);
        assert!(payload(&[0; 4], 1, 12).is_some());
        assert!(payload(&[0; 4], 1, 13).is_none());
        assert!(payload(&[0; 4], 9, 0).is_none());
    }
}
