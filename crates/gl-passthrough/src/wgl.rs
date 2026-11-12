//! The host's OpenGL on Windows: WGL contexts on a window nobody sees.
//!
//! A WGL context needs a device context with a pixel format, and that needs
//! a window; this one is a 1×1 pop-up that is never shown. Nothing is drawn
//! to it — every guest context draws into a framebuffer object
//! ([`crate::exec`]) — so its pixel format only has to be one OpenGL
//! accepts.
//!
//! The handful of Win32 functions are declared here by hand: they are few,
//! stable since Windows 95, and it keeps the `windows` crate (and its
//! feature list) out of a crate that is otherwise portable.

use std::ffi::{c_char, c_void, CStr};
use std::sync::Arc;

use crate::api::Api;
use crate::exec::Platform;

type Handle = *mut c_void;

#[repr(C)]
struct PixelFormatDescriptor {
    size: u16,
    version: u16,
    flags: u32,
    pixel_type: u8,
    color_bits: u8,
    // Red, green, blue and alpha bits and shifts, the accumulation buffer.
    unused: [u8; 13],
    depth_bits: u8,
    stencil_bits: u8,
    aux_buffers: u8,
    layer_type: u8,
    reserved: u8,
    layer_mask: u32,
    visible_mask: u32,
    damage_mask: u32,
}

const PFD_DRAW_TO_WINDOW: u32 = 0x4;
const PFD_SUPPORT_OPENGL: u32 = 0x20;
const PFD_DOUBLEBUFFER: u32 = 0x1;
const WS_POPUP: u32 = 0x8000_0000;

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
    fn DestroyWindow(window: Handle) -> i32;
    fn GetDC(window: Handle) -> Handle;
    fn ReleaseDC(window: Handle, dc: Handle) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn ChoosePixelFormat(dc: Handle, descriptor: *const PixelFormatDescriptor) -> i32;
    fn SetPixelFormat(dc: Handle, format: i32, descriptor: *const PixelFormatDescriptor) -> i32;
}

#[link(name = "opengl32")]
extern "system" {
    fn wglCreateContext(dc: Handle) -> Handle;
    fn wglDeleteContext(context: Handle) -> i32;
    fn wglMakeCurrent(dc: Handle, context: Handle) -> i32;
    fn wglGetProcAddress(name: *const c_char) -> *const c_void;
    fn wglShareLists(first: Handle, second: Handle) -> i32;
    fn wglCopyContext(from: Handle, to: Handle, mask: u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleA(name: *const c_char) -> Handle;
    fn GetProcAddress(module: Handle, name: *const c_char) -> *const c_void;
}

/// A WGL context of the hidden window's.
pub struct Context(Handle);

/// The hidden window, its device context and the loaded functions.
pub struct Wgl {
    window: Handle,
    dc: Handle,
    /// Kept so the functions could be loaded; deleted with the window.
    first: Handle,
    api: Arc<Api>,
}

impl Wgl {
    /// Creates the window and a first context, and loads every function
    /// through it. `Err` says which step the host's OpenGL failed at.
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain Win32 calls with valid arguments — a predefined
        // window class, NUL-terminated names, null for every optional
        // handle. Each result is checked before it is used, and what was
        // created is destroyed on the way out of a failure.
        unsafe {
            let window = CreateWindowExA(
                0,
                c"STATIC".as_ptr(),
                c"Entangled OpenGL".as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if window.is_null() {
                return Err("no window for the host OpenGL context".into());
            }
            let dc = GetDC(window);
            let descriptor = PixelFormatDescriptor {
                size: size_of::<PixelFormatDescriptor>() as u16,
                version: 1,
                flags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                pixel_type: 0, // PFD_TYPE_RGBA
                color_bits: 32,
                unused: [0; 13],
                depth_bits: 24,
                stencil_bits: 8,
                aux_buffers: 0,
                layer_type: 0,
                reserved: 0,
                layer_mask: 0,
                visible_mask: 0,
                damage_mask: 0,
            };
            let fail = |why: &str| {
                ReleaseDC(window, dc);
                DestroyWindow(window);
                Err(why.to_owned())
            };
            let format = ChoosePixelFormat(dc, &descriptor);
            if dc.is_null() || format == 0 || SetPixelFormat(dc, format, &descriptor) == 0 {
                return fail("the host has no OpenGL pixel format");
            }
            let first = wglCreateContext(dc);
            if first.is_null() || wglMakeCurrent(dc, first) == 0 {
                if !first.is_null() {
                    wglDeleteContext(first);
                }
                return fail("the host could not create an OpenGL context");
            }
            let library = GetModuleHandleA(c"opengl32.dll".as_ptr());
            let loader = |name: &CStr| {
                // OpenGL 1.1 lives in opengl32.dll's export table; anything
                // newer comes from the driver. `wglGetProcAddress` answers
                // small integers, not just null, for a name it lacks.
                let exported = GetProcAddress(library, name.as_ptr());
                if !exported.is_null() {
                    return exported;
                }
                let extension = wglGetProcAddress(name.as_ptr());
                if (extension as usize) <= 3 || extension as isize == -1 {
                    std::ptr::null()
                } else {
                    extension
                }
            };
            let api = Api::load(&loader);
            wglMakeCurrent(std::ptr::null_mut(), std::ptr::null_mut());
            match api {
                Ok(api) => Ok(Self {
                    window,
                    dc,
                    first,
                    api: Arc::new(api),
                }),
                Err(missing) => {
                    wglDeleteContext(first);
                    fail(&format!("the host's OpenGL has no gl{missing}"))
                }
            }
        }
    }
}

impl Drop for Wgl {
    fn drop(&mut self) {
        // SAFETY: the handles are the ones `new` created, used on the
        // thread that created them, and not used again.
        unsafe {
            wglMakeCurrent(std::ptr::null_mut(), std::ptr::null_mut());
            wglDeleteContext(self.first);
            ReleaseDC(self.window, self.dc);
            DestroyWindow(self.window);
        }
    }
}

impl Platform for Wgl {
    type Context = Context;

    fn api(&self) -> Arc<Api> {
        self.api.clone()
    }

    fn create(&mut self, share: Option<&Context>) -> Option<Context> {
        // SAFETY: `dc` is the window's, with its pixel format set; `share`
        // is a live context of the same device context.
        unsafe {
            let context = wglCreateContext(self.dc);
            if context.is_null() {
                return None;
            }
            if let Some(share) = share {
                if wglShareLists(share.0, context) == 0 {
                    wglDeleteContext(context);
                    return None;
                }
            }
            Some(Context(context))
        }
    }

    fn destroy(&mut self, context: Context) {
        // SAFETY: a context `create` made, consumed here.
        unsafe {
            wglDeleteContext(context.0);
        }
    }

    fn make_current(&mut self, context: Option<&Context>) -> bool {
        // SAFETY: `dc` and the context are live, and this is the thread
        // that owns them.
        unsafe {
            match context {
                Some(context) => wglMakeCurrent(self.dc, context.0) != 0,
                None => wglMakeCurrent(std::ptr::null_mut(), std::ptr::null_mut()) != 0,
            }
        }
    }

    fn share(&mut self, first: &Context, second: &Context) -> bool {
        // SAFETY: both are live contexts of this window.
        unsafe { wglShareLists(first.0, second.0) != 0 }
    }

    fn copy(&mut self, from: &Context, to: &Context, mask: u32) -> bool {
        // SAFETY: both are live contexts of this window.
        unsafe { wglCopyContext(from.0, to.0, mask) != 0 }
    }
}
