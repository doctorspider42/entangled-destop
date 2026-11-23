//! The host OpenGL's entry points, as a table of function pointers.
//!
//! One table for every platform: the functions are looked up by name through
//! whatever the platform's loader is (`GetProcAddress` and
//! `wglGetProcAddress` on Windows), so nothing here links against an OpenGL
//! library. The plain functions' rows are generated
//! ([`crate::calls::plain_functions`]); the rest are listed below by hand,
//! with the framebuffer-object functions the off-screen drawable needs.

use std::ffi::{c_void, CStr};

/// `GL_FRAMEBUFFER` and friends, for the off-screen drawable.
pub const FRAMEBUFFER: u32 = 0x8d40;
pub const RENDERBUFFER: u32 = 0x8d41;
pub const COLOR_ATTACHMENT0: u32 = 0x8ce0;
pub const DEPTH_ATTACHMENT: u32 = 0x8d00;
pub const STENCIL_ATTACHMENT: u32 = 0x8d20;
pub const DEPTH24_STENCIL8: u32 = 0x88f0;
pub const RGBA8: u32 = 0x8058;
pub const FRAMEBUFFER_COMPLETE: u32 = 0x8cd5;

macro_rules! api_table {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// Every host OpenGL function this crate calls.
        #[allow(non_snake_case, missing_docs)]
        pub struct Api {
            $(pub $name: unsafe extern "system" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            /// Looks every function up through `loader`, which answers null
            /// for a name it does not have; `Err` names the first one
            /// missing.
            ///
            /// # Safety
            /// A non-null answer must be the address of the OpenGL function
            /// of that name, with the platform's OpenGL calling convention.
            pub unsafe fn load(
                loader: &dyn Fn(&CStr) -> *const c_void,
            ) -> Result<Self, &'static str> {
                Ok(Self {
                    $($name: {
                        let name = concat!("gl", stringify!($name), "\0");
                        let address = CStr::from_bytes_with_nul(name.as_bytes())
                            .map(|name| loader(name))
                            .unwrap_or(std::ptr::null());
                        if address.is_null() {
                            return Err(stringify!($name));
                        }
                        // SAFETY: the caller's contract — this is that
                        // function's address — and a function pointer is
                        // the size of a data pointer on every host we build
                        // for.
                        unsafe {
                            std::mem::transmute::<
                                *const c_void,
                                unsafe extern "system" fn($($arg),*) $(-> $ret)?,
                            >(address)
                        }
                    },)*
                })
            }
        }
    };
}

macro_rules! with_hand_written {
    ($($plain:tt)*) => {
        api_table! {
            $($plain)*
            GetError: fn() -> u32;
            GetString: fn(u32) -> *const u8;
            GetIntegerv: fn(u32, *mut i32);
            GetFloatv: fn(u32, *mut f32);
            GetBooleanv: fn(u32, *mut u8);
            GetDoublev: fn(u32, *mut f64);
            IsEnabled: fn(u32) -> u8;
            IsList: fn(u32) -> u8;
            IsTexture: fn(u32) -> u8;
            GenLists: fn(i32) -> u32;
            GenTextures: fn(i32, *mut u32);
            DeleteTextures: fn(i32, *const u32);
            Finish: fn();
            Flush: fn();
            DrawBuffer: fn(u32);
            ReadBuffer: fn(u32);
            PixelStorei: fn(u32, i32);
            LoadMatrixf: fn(*const f32);
            LoadMatrixd: fn(*const f64);
            MultMatrixf: fn(*const f32);
            MultMatrixd: fn(*const f64);
            Fogfv: fn(u32, *const f32);
            Fogiv: fn(u32, *const i32);
            Lightfv: fn(u32, u32, *const f32);
            Lightiv: fn(u32, u32, *const i32);
            LightModelfv: fn(u32, *const f32);
            LightModeliv: fn(u32, *const i32);
            Materialfv: fn(u32, u32, *const f32);
            Materialiv: fn(u32, u32, *const i32);
            TexParameterfv: fn(u32, u32, *const f32);
            TexParameteriv: fn(u32, u32, *const i32);
            TexEnvfv: fn(u32, u32, *const f32);
            TexEnviv: fn(u32, u32, *const i32);
            TexGenfv: fn(u32, u32, *const f32);
            TexGeniv: fn(u32, u32, *const i32);
            TexGendv: fn(u32, u32, *const f64);
            ClipPlane: fn(u32, *const f64);
            TexImage1D: fn(u32, i32, i32, i32, i32, u32, u32, *const c_void);
            TexImage2D: fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
            TexSubImage1D: fn(u32, i32, i32, i32, u32, u32, *const c_void);
            TexSubImage2D: fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
            DrawPixels: fn(i32, i32, u32, u32, *const c_void);
            ReadPixels: fn(i32, i32, i32, i32, u32, u32, *mut c_void);
            Bitmap: fn(i32, i32, f32, f32, f32, f32, *const u8);
            PolygonStipple: fn(*const u8);
            CallLists: fn(i32, u32, *const c_void);
            VertexPointer: fn(i32, u32, i32, *const c_void);
            NormalPointer: fn(u32, i32, *const c_void);
            ColorPointer: fn(i32, u32, i32, *const c_void);
            TexCoordPointer: fn(i32, u32, i32, *const c_void);
            EnableClientState: fn(u32);
            DisableClientState: fn(u32);
            DrawArrays: fn(u32, i32, i32);
            DrawElements: fn(u32, i32, u32, *const c_void);
            GetLightfv: fn(u32, u32, *mut f32);
            GetLightiv: fn(u32, u32, *mut i32);
            GetMaterialfv: fn(u32, u32, *mut f32);
            GetMaterialiv: fn(u32, u32, *mut i32);
            GetTexEnvfv: fn(u32, u32, *mut f32);
            GetTexEnviv: fn(u32, u32, *mut i32);
            GetTexGenfv: fn(u32, u32, *mut f32);
            GetTexGeniv: fn(u32, u32, *mut i32);
            GetTexParameterfv: fn(u32, u32, *mut f32);
            GetTexParameteriv: fn(u32, u32, *mut i32);
            GetTexLevelParameterfv: fn(u32, i32, u32, *mut f32);
            GetTexLevelParameteriv: fn(u32, i32, u32, *mut i32);
            GenFramebuffers: fn(i32, *mut u32);
            BindFramebuffer: fn(u32, u32);
            GenRenderbuffers: fn(i32, *mut u32);
            BindRenderbuffer: fn(u32, u32);
            RenderbufferStorage: fn(u32, u32, i32, i32);
            FramebufferRenderbuffer: fn(u32, u32, u32, u32);
            CheckFramebufferStatus: fn(u32) -> u32;
        }
    };
}

crate::calls::plain_functions!(with_hand_written);
