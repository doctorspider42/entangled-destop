//! The guest's OpenGL driver and the host's executor, end to end, with no
//! guest between them.
//!
//! `guest/winxp/build.sh` also compiles the XP driver's source for the host
//! (`out/test/entgl-test.dll`), with the escape to the display driver
//! replaced by a hook. This test is that hook: what the driver would send
//! through the device goes straight to a [`GlThread`], and the answer goes
//! back. The calls below are made the way a program makes them — through
//! the dispatch table the driver hands `opengl32.dll`.
//!
//! Self-skips without the test DLL (it needs clang and a Windows SDK to
//! build) or without a usable host OpenGL.

#![cfg(windows)]
#![allow(clippy::missing_transmute_annotations)]

use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use gl_passthrough::calls::op;
use gl_passthrough::{GlThread, Present};

type Handle = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const c_char) -> Handle;
    fn GetProcAddress(module: Handle, name: *const c_char) -> *const c_void;
}

#[link(name = "user32")]
extern "system" {
    fn CreateWindowExA(
        ex_style: u32,
        class: *const c_char,
        name: *const c_char,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        param: *mut c_void,
    ) -> Handle;
    fn GetDC(window: Handle) -> Handle;
}

static GL: OnceLock<GlThread> = OnceLock::new();
static PRESENTS: Mutex<Vec<Present>> = Mutex::new(Vec::new());

/// What the driver calls instead of `ExtEscape`.
extern "C" fn transact(stream: *const u32, words: u32, response: *mut u8, bytes: u32) -> i32 {
    // SAFETY: the driver passes its buffer and the room for the answer.
    let stream = unsafe { std::slice::from_raw_parts(stream, words as usize) };
    let Some(gl) = GL.get() else { return -1 };
    let outcome = gl.run(stream.to_vec());
    PRESENTS.lock().unwrap().extend(outcome.presents);
    let n = outcome.response.len().min(bytes as usize);
    if n > 0 {
        // SAFETY: `response` has room for `bytes` bytes, and `n` is no more.
        unsafe { std::ptr::copy_nonoverlapping(outcome.response.as_ptr(), response, n) };
    }
    n as i32
}

#[repr(C)]
struct DispatchTable {
    count: i32,
    table: [*const c_void; 336],
}

struct Driver {
    module: Handle,
    table: *const DispatchTable,
}

impl Driver {
    fn symbol(&self, name: &CStr) -> *const c_void {
        // SAFETY: a loaded module and a NUL-terminated name.
        let address = unsafe { GetProcAddress(self.module, name.as_ptr()) };
        assert!(!address.is_null(), "{name:?} is not exported");
        address
    }

    /// The OpenGL function at `opcode` of the dispatch table.
    fn gl(&self, opcode: u16) -> *const c_void {
        // SAFETY: the table the driver returned from `DrvSetContext`.
        unsafe { (*self.table).table[usize::from(opcode)] }
    }
}

fn dll() -> Option<PathBuf> {
    let path = std::env::var_os("ENTGL_TEST_DLL").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../guest/winxp/out/test/entgl-test.dll")
        },
        PathBuf::from,
    );
    path.exists().then_some(path)
}

fn pixel(present: &Present, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * present.width + x) * 4) as usize;
    [
        present.pixels[at + 2],
        present.pixels[at + 1],
        present.pixels[at],
    ]
}

macro_rules! call {
    ($driver:expr, $op:ident, fn($($ty:ty),*) $(-> $ret:ty)?, $($arg:expr),*) => {{
        let entry = $driver.gl(op::$op);
        type Entry = unsafe extern "system" fn($($ty),*) $(-> $ret)?;
        // SAFETY: the entry is the driver's implementation of that OpenGL
        // function, which has this signature.
        let f = unsafe { std::mem::transmute::<*const c_void, Entry>(entry) };
        // SAFETY: as above; pointers passed are to live locals.
        unsafe { f($($arg),*) }
    }};
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_program_draws_through_the_driver_and_sees_it_come_back() {
    let Some(path) = dll() else {
        eprintln!("skipping: no entgl-test.dll (run guest/winxp/build.sh)");
        return;
    };
    match GlThread::start() {
        Ok(gl) => {
            let _ = GL.set(gl);
        }
        Err(why) => {
            eprintln!("skipping: {why}");
            return;
        }
    }
    let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: plain Win32 calls with NUL-terminated names; results checked.
    let (module, dc) = unsafe {
        let module = LoadLibraryA(name.as_ptr());
        assert!(!module.is_null(), "cannot load {path:?}");
        let window = CreateWindowExA(
            0,
            c"STATIC".as_ptr(),
            c"".as_ptr(),
            0x8000_0000, // WS_POPUP
            30,
            40,
            64,
            48,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert!(!window.is_null());
        (module, GetDC(window))
    };
    let mut driver = Driver {
        module,
        table: std::ptr::null(),
    };
    // SAFETY: the driver's exports, with the signatures an ICD has.
    let (create, set, swap, release, delete) = unsafe {
        let hook: unsafe extern "C" fn(extern "C" fn(*const u32, u32, *mut u8, u32) -> i32) =
            std::mem::transmute(driver.symbol(c"entgl_test_hook"));
        hook(transact);
        let create: unsafe extern "system" fn(Handle) -> usize =
            std::mem::transmute(driver.symbol(c"DrvCreateContext"));
        let set: unsafe extern "system" fn(Handle, usize, *const c_void) -> *const DispatchTable =
            std::mem::transmute(driver.symbol(c"DrvSetContext"));
        let swap: unsafe extern "system" fn(Handle) -> i32 =
            std::mem::transmute(driver.symbol(c"DrvSwapBuffers"));
        let release: unsafe extern "system" fn(usize) -> i32 =
            std::mem::transmute(driver.symbol(c"DrvReleaseContext"));
        let delete: unsafe extern "system" fn(usize) -> i32 =
            std::mem::transmute(driver.symbol(c"DrvDeleteContext"));
        (create, set, swap, release, delete)
    };
    // SAFETY: as declared above.
    let context = unsafe { create(dc) };
    assert_ne!(context, 0, "no context");
    // SAFETY: a context this driver made, on this thread.
    driver.table = unsafe { set(dc, context, std::ptr::null()) };
    assert!(!driver.table.is_null());
    // SAFETY: the table just returned.
    assert_eq!(unsafe { (*driver.table).count }, 336);

    // Questions first: the string is the driver's, the viewport the
    // window's — which the host learnt from the driver.
    let version = call!(driver, GetString, fn(u32) -> *const c_char, 0x1f02);
    // SAFETY: a NUL-terminated string the driver keeps.
    assert_eq!(unsafe { CStr::from_ptr(version) }.to_bytes(), b"1.1.0");
    let mut viewport = [0i32; 4];
    call!(
        driver,
        GetIntegerv,
        fn(u32, *mut i32),
        0x0ba2,
        viewport.as_mut_ptr()
    );
    assert_eq!(viewport, [0, 0, 64, 48]);

    // Immediate mode: a red triangle over the bottom-left half, on blue.
    call!(
        driver,
        ClearColor,
        fn(f32, f32, f32, f32),
        0.0,
        0.0,
        1.0,
        1.0
    );
    call!(driver, Clear, fn(u32), 0x4000);
    call!(driver, Begin, fn(u32), 4);
    call!(driver, Color3ub, fn(u8, u8, u8), 255, 0, 0);
    let corners = [[-1.0f32, -1.0], [1.0, -1.0], [-1.0, 1.0]];
    for corner in &corners {
        // The vector form: the driver turns it into glVertex2f.
        call!(driver, Vertex2fv, fn(*const f32), corner.as_ptr());
    }
    call!(driver, End, fn(),);

    // Vertex arrays with a stride: a green quad in the top-right quarter.
    #[repr(C)]
    struct Vertex {
        position: [f32; 2],
        color: [u8; 4],
        unused: u32,
    }
    let green = [0, 255, 0, 255];
    let quad = [[0.5f32, 0.5], [1.0, 0.5], [1.0, 1.0], [0.5, 1.0]].map(|position| Vertex {
        position,
        color: green,
        unused: 0xdead_beef,
    });
    let stride = size_of::<Vertex>() as i32;
    call!(driver, EnableClientState, fn(u32), 0x8074);
    call!(driver, EnableClientState, fn(u32), 0x8076);
    call!(
        driver,
        VertexPointer,
        fn(i32, u32, i32, *const c_void),
        2,
        0x1406,
        stride,
        quad.as_ptr().cast()
    );
    call!(
        driver,
        ColorPointer,
        fn(i32, u32, i32, *const c_void),
        4,
        0x1401,
        stride,
        std::ptr::from_ref(&quad[0].color).cast()
    );
    // Through indices that do not start at 0: only vertices 1..=3 travel.
    let indices = [1u16, 2, 3];
    call!(
        driver,
        DrawElements,
        fn(u32, i32, u32, *const c_void),
        4,
        3,
        0x1403,
        indices.as_ptr().cast()
    );
    call!(driver, DrawArrays, fn(u32, i32, i32), 7, 0, 4);
    call!(driver, DisableClientState, fn(u32), 0x8076);
    call!(driver, DisableClientState, fn(u32), 0x8074);

    // A 3x2 RGB texture with the default unpack alignment of 4: each row
    // is 9 bytes of pixels and 3 of padding, which must not be sent.
    let mut texture = 0u32;
    call!(driver, GenTextures, fn(i32, *mut u32), 1, &mut texture);
    assert_ne!(texture, 0);
    call!(driver, BindTexture, fn(u32, u32), 0x0de1, texture);
    let white = [255u8; 3];
    let mut texels = Vec::new();
    for _ in 0..2 {
        for _ in 0..3 {
            texels.extend(white);
        }
        texels.extend([0x55; 3]);
    }
    call!(
        driver,
        TexImage2D,
        fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
        0x0de1,
        0,
        3,
        3,
        2,
        0,
        0x1907,
        0x1401,
        texels.as_ptr().cast()
    );
    let mut width = [0i32; 1];
    call!(
        driver,
        GetTexLevelParameteriv,
        fn(u32, i32, u32, *mut i32),
        0x0de1,
        0,
        0x1000,
        width.as_mut_ptr()
    );
    assert_eq!(width[0], 3);
    assert_eq!(call!(driver, GetError, fn() -> u32,), 0, "no OpenGL error");

    // Read back before the swap: one pixel, bottom-left, into a buffer the
    // pack alignment pads.
    let mut read = [0u8; 4];
    call!(
        driver,
        ReadPixels,
        fn(i32, i32, i32, i32, u32, u32, *mut c_void),
        2,
        2,
        1,
        1,
        0x1907,
        0x1401,
        read.as_mut_ptr().cast()
    );
    assert_eq!(read, [255, 0, 0, 0], "the triangle, read back");

    // SAFETY: as declared above.
    assert_ne!(unsafe { swap(dc) }, 0);
    let present = PRESENTS.lock().unwrap().pop().expect("a picture");
    // The window's client corner on the screen, and its size.
    assert_eq!((present.x, present.y), (30, 40));
    assert_eq!((present.width, present.height), (64, 48));
    assert_eq!(pixel(&present, 4, 44), [255, 0, 0], "triangle");
    assert_eq!(pixel(&present, 56, 4), [0, 255, 0], "quad from arrays");
    assert_eq!(pixel(&present, 40, 14), [0, 0, 255], "background");

    // SAFETY: as declared above.
    unsafe {
        assert_ne!(release(context), 0);
        assert_ne!(delete(context), 0);
    }
}
