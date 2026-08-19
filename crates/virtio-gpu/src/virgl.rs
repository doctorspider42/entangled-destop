//! The real host renderer (ADR-0004): virglrenderer over FFI, `dlopen`ed at
//! runtime.
//!
//! Linux-only by `cfg` (the library speaks EGL; the Windows renderer story is
//! ADR-0004 §6). Nothing links at build time: [`VirglRenderer::load`] opens
//! `libvirglrenderer.so.1` with `libloading`, resolves the ~20 entry points of
//! the stable 0.9 API, initializes EGL (surfaceless — no window system needed,
//! which is what lets this run under WSLg and headless CI alike) and
//! implements [`Renderer3d`] by forwarding the commands `Gpu3d` already
//! validated.
//!
//! # Trust and safety model
//!
//! virglrenderer is *inside* the trust boundary (ADR-0004 §3): it receives
//! only validated input, and we treat its process-global state with the same
//! care as a kernel API. What this module is responsible for is the FFI
//! contract:
//!
//! * **Pointer lifetimes.** `virgl_renderer_resource_attach_iov` stores the
//!   iovec *array pointer* until detach — so every attachment owns its boxed
//!   array plus an `Arc<GuestMem>` clone, kept in [`Attachment`] until the
//!   library gives the array back (detach/unref) or the deferred reset runs.
//! * **Thread affinity.** virglrenderer binds its EGL contexts to the calling
//!   thread. All queue processing happens on the device's worker thread, so
//!   initialization is *lazy* (first command) and a guest-driven device reset
//!   — which arrives on a vCPU thread — only marks the renderer dirty; the
//!   actual `virgl_renderer_reset` runs on the next worker-thread call.
//! * **One renderer per process.** The library is process-global; a second
//!   simultaneous instance is refused at `load`.
//!
//! Every `unsafe` block documents why its pointers are valid.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

use virtio_core::GuestMem;

use crate::error::CommandError;
use crate::protocol::{MemEntry, Rect, ResourceCreate3d, Transfer3d};
use crate::renderer::{CapsetInfo, Renderer3d};
use crate::MAX_RESOURCE_PIXELS;

/// `VIRGL_RENDERER_USE_EGL`.
const USE_EGL: c_int = 1;
/// `VIRGL_RENDERER_USE_SURFACELESS`.
const USE_SURFACELESS: c_int = 1 << 3;
/// `VIRGL_RENDERER_USE_GLES`.
const USE_GLES: c_int = 1 << 4;

/// `VIRTIO_GPU_CAPSET_VIRGL` / `VIRTIO_GPU_CAPSET_VIRGL2`.
const CAPSET_VIRGL: u32 = 1;
const CAPSET_VIRGL2: u32 = 2;

/// One process-wide renderer (the library is a singleton).
static RENDERER_LIVE: AtomicBool = AtomicBool::new(false);

/// `struct iovec` as virglrenderer consumes it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Iovec {
    iov_base: *mut c_void,
    iov_len: usize,
}

/// `struct virgl_box`.
#[repr(C)]
struct VirglBox {
    x: c_uint,
    y: c_uint,
    z: c_uint,
    w: c_uint,
    h: c_uint,
    d: c_uint,
}

/// `struct virgl_renderer_resource_create_args`.
#[repr(C)]
struct CreateArgs {
    handle: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
}

/// `struct virgl_renderer_callbacks`, version 1 (the EGL path uses none of
/// the GL-context callbacks; `write_fence` must exist).
#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: Option<extern "C" fn(*mut c_void, u32)>,
    create_gl_context: Option<extern "C" fn(*mut c_void, c_int, *mut c_void) -> *mut c_void>,
    destroy_gl_context: Option<extern "C" fn(*mut c_void, *mut c_void)>,
    make_current: Option<extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int>,
    get_drm_fd: Option<extern "C" fn(*mut c_void) -> c_int>,
}

/// Fences are synchronous in phase 1 (ADR-0004 §4): commands complete before
/// their response is written, so the callback only exists because the ABI
/// requires it.
extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {}

/// The resolved entry points of the 0.9 API.
///
/// Field-per-symbol rather than lazy lookups: `load` fails up front if the
/// installed library is too old, instead of a guest command failing later.
struct Api {
    init: unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int,
    reset: unsafe extern "C" fn(),
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    context_create: unsafe extern "C" fn(u32, u32, *const c_char) -> c_int,
    context_destroy: unsafe extern "C" fn(u32),
    resource_create: unsafe extern "C" fn(*mut CreateArgs, *mut Iovec, u32) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    resource_attach_iov: unsafe extern "C" fn(c_int, *mut Iovec, c_int) -> c_int,
    resource_detach_iov: unsafe extern "C" fn(c_int, *mut *mut Iovec, *mut c_int),
    ctx_attach_resource: unsafe extern "C" fn(c_int, c_int),
    ctx_detach_resource: unsafe extern "C" fn(c_int, c_int),
    transfer_write_iov: unsafe extern "C" fn(
        u32,
        u32,
        c_int,
        u32,
        u32,
        *mut VirglBox,
        u64,
        *mut Iovec,
        c_uint,
    ) -> c_int,
    transfer_read_iov: unsafe extern "C" fn(
        u32,
        u32,
        u32,
        u32,
        u32,
        *mut VirglBox,
        u64,
        *mut Iovec,
        c_int,
    ) -> c_int,
    submit_cmd: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
    // ManuallyDrop = the library is never dlclose'd. Unloading a GL stack at
    // runtime is famously unsafe: mesa's driver threads leave TLS destructors
    // behind and `virgl_renderer_cleanup` + dlclose segfault at thread exit
    // (observed with the d3d12 driver under WSLg). QEMU and crosvm keep the
    // renderer for the life of the process for the same reason; so do we.
    _lib: std::mem::ManuallyDrop<libloading::Library>,
}

/// A live backing attachment: what keeps the pointers virglrenderer holds
/// valid.
struct Attachment {
    /// The iovec array whose *address* the library stored at attach time.
    _iov: Box<[Iovec]>,
    /// Keeps the guest memory mapping (which `iov` points into) alive even if
    /// the device is reset and drops its own Arc first.
    _mem: Arc<GuestMem>,
}

/// Per-resource state the renderer keeps on the host side.
#[derive(Default)]
struct ResourceState {
    width: u32,
    height: u32,
    attachment: Option<Attachment>,
    /// Full-frame BGRA shadow for scanout/cursor readback, allocated on first
    /// read (only presented resources ever get one).
    shadow: Vec<u8>,
}

/// What phase of life the process-global library is in.
enum State {
    /// Loaded, symbols resolved, `virgl_renderer_init` not yet called — that
    /// happens on the first command so EGL binds to the worker thread.
    Loaded,
    Ready,
    /// A device reset arrived (possibly from a vCPU thread);
    /// `virgl_renderer_reset` runs before the next command.
    NeedsReset,
}

/// [`Renderer3d`] on libvirglrenderer. See the module docs.
pub struct VirglRenderer {
    api: Api,
    state: State,
    capsets: Vec<CapsetInfo>,
    resources: HashMap<u32, ResourceState>,
    contexts: Vec<u32>,
    /// Attachments freed by a deferred reset stay here until the reset really
    /// runs (the library may still hold their array pointers).
    graveyard: Vec<Attachment>,
    /// Dword staging for submits (virglrenderer reads u32s; the gathered
    /// request buffer is byte-aligned).
    submit_buf: Vec<u32>,
    /// The init cookie; its address is registered with the library.
    cookie: Box<u32>,
    /// The callbacks struct. The library stores the *pointer* it was given at
    /// init (QEMU keeps a static for the same reason), so this must live
    /// exactly as long as the initialized library state.
    callbacks: Box<Callbacks>,
}

// SAFETY: the renderer is moved to the device worker thread once and used
// there; raw pointers inside (`Attachment::iov` bases) point into guest
// memory kept alive by the `Arc<GuestMem>` next to them, and the library
// handle is itself Send. Nothing here is shared between threads.
unsafe impl Send for VirglRenderer {}

impl VirglRenderer {
    /// Opens the library, resolves the API and probes the capsets. Fails —
    /// with a message good enough to act on — when the library is missing,
    /// too old, or a renderer already exists in this process.
    ///
    /// EGL/GL initialization is deferred to the first command (thread
    /// affinity; see the module docs), but the capset *sizes* need the
    /// library only, not a GL context, so they are read here.
    pub fn load() -> Result<Self, String> {
        if RENDERER_LIVE.swap(true, Ordering::SeqCst) {
            return Err("a virglrenderer instance already exists in this process".into());
        }
        let result = Self::load_inner();
        if result.is_err() {
            RENDERER_LIVE.store(false, Ordering::SeqCst);
        }
        result
    }

    fn load_inner() -> Result<Self, String> {
        let lib = ["libvirglrenderer.so.1", "libvirglrenderer.so"]
            .iter()
            .find_map(|name| {
                // SAFETY: dlopen of a system library; its constructors are the
                // platform loader's business, and we resolve symbols before use.
                unsafe { libloading::Library::new(name) }.ok()
            })
            .ok_or_else(|| {
                "libvirglrenderer.so.1 not found — install libvirglrenderer1 \
                 (Debian/Ubuntu) or disable [display] virgl"
                    .to_string()
            })?;

        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol is looked up by its C name in the library
                // just opened; the transmute to the declared fn type matches
                // the 0.9 API prototypes quoted in `Api`.
                unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .map(|s: libloading::Symbol<_>| *s)
                    .map_err(|e| format!("{}: {e} — virglrenderer too old?", $name))?
            };
        }

        let api = Api {
            init: sym!("virgl_renderer_init"),
            reset: sym!("virgl_renderer_reset"),
            get_cap_set: sym!("virgl_renderer_get_cap_set"),
            fill_caps: sym!("virgl_renderer_fill_caps"),
            context_create: sym!("virgl_renderer_context_create"),
            context_destroy: sym!("virgl_renderer_context_destroy"),
            resource_create: sym!("virgl_renderer_resource_create"),
            resource_unref: sym!("virgl_renderer_resource_unref"),
            resource_attach_iov: sym!("virgl_renderer_resource_attach_iov"),
            resource_detach_iov: sym!("virgl_renderer_resource_detach_iov"),
            ctx_attach_resource: sym!("virgl_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!("virgl_renderer_ctx_detach_resource"),
            transfer_write_iov: sym!("virgl_renderer_transfer_write_iov"),
            transfer_read_iov: sym!("virgl_renderer_transfer_read_iov"),
            submit_cmd: sym!("virgl_renderer_submit_cmd"),
            _lib: std::mem::ManuallyDrop::new(lib),
        };

        Ok(Self {
            api,
            state: State::Loaded,
            capsets: Vec::new(),
            resources: HashMap::new(),
            contexts: Vec::new(),
            graveyard: Vec::new(),
            submit_buf: Vec::new(),
            // A recognizable pattern ("virg") should the cookie surface in a
            // debugger; the library only round-trips the pointer.
            cookie: Box::new(0x7669_7267),
            callbacks: Box::new(Callbacks {
                version: 1,
                write_fence: Some(write_fence),
                create_gl_context: None,
                destroy_gl_context: None,
                make_current: None,
                get_drm_fd: None,
            }),
        })
    }

    /// Brings the process-global renderer up (once) and applies a deferred
    /// reset. Called at the top of every trait method, i.e. always on the
    /// worker thread.
    fn ensure_ready(&mut self) -> Result<(), CommandError> {
        match self.state {
            State::Ready => Ok(()),
            State::Loaded => {
                let cookie = std::ptr::from_mut::<u32>(&mut *self.cookie).cast::<c_void>();
                let callbacks = std::ptr::from_mut::<Callbacks>(&mut *self.callbacks);
                // Surfaceless EGL first (headless, WSLg — the probed
                // configuration); GLES as the fallback for hosts whose EGL
                // offers no desktop-GL contexts.
                let mut rc = -1;
                for flags in [
                    USE_EGL | USE_SURFACELESS,
                    USE_EGL | USE_SURFACELESS | USE_GLES,
                ] {
                    // SAFETY: the cookie and the callbacks struct are boxed in
                    // `self` and outlive the initialized library — required,
                    // because the library stores both *pointers*.
                    rc = unsafe { (self.api.init)(cookie, flags, callbacks) };
                    if rc == 0 {
                        break;
                    }
                }
                if rc != 0 {
                    return Err(CommandError::Renderer(format!(
                        "virgl_renderer_init failed ({rc}): no usable EGL/GL on this host"
                    )));
                }
                for id in [CAPSET_VIRGL, CAPSET_VIRGL2] {
                    let (mut max_version, mut max_size) = (0u32, 0u32);
                    // SAFETY: out-pointers to locals, valid for the call.
                    unsafe { (self.api.get_cap_set)(id, &mut max_version, &mut max_size) };
                    if max_size > 0 {
                        self.capsets.push(CapsetInfo {
                            id,
                            max_version,
                            max_size,
                        });
                    }
                }
                tracing::info!(capsets = self.capsets.len(), "virglrenderer initialized");
                self.state = State::Ready;
                Ok(())
            }
            State::NeedsReset => {
                // SAFETY: no arguments; destroys every context and resource
                // the library holds. Runs before the graveyard is emptied, so
                // any iovec array the library still references is alive.
                unsafe { (self.api.reset)() };
                self.graveyard.clear();
                tracing::info!("virglrenderer reset");
                self.state = State::Ready;
                Ok(())
            }
        }
    }

    /// Detaches a resource's iovec array from the library and reclaims it.
    fn detach_iov(&mut self, resource_id: u32) {
        let Some(state) = self.resources.get_mut(&resource_id) else {
            return;
        };
        if state.attachment.take().is_some() {
            let mut iov: *mut Iovec = std::ptr::null_mut();
            let mut num: c_int = 0;
            // SAFETY: out-pointers to locals; the library hands back the
            // array pointer we gave it at attach time (we drop our own box —
            // the returned pointer is not freed here because it *is* ours).
            unsafe { (self.api.resource_detach_iov)(resource_id as c_int, &mut iov, &mut num) };
        }
    }
}

impl Renderer3d for VirglRenderer {
    fn capsets(&self) -> &[CapsetInfo] {
        &self.capsets
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        self.ensure_ready()?;
        let info = self
            .capsets
            .iter()
            .find(|c| c.id == id)
            .copied()
            .ok_or(CommandError::UnknownCapset { id, version })?;
        let mut blob = vec![0u8; info.max_size as usize];
        // SAFETY: `blob` is exactly the `max_size` the library reported for
        // this set, which is the buffer size `fill_caps` writes into.
        unsafe { (self.api.fill_caps)(id, version, blob.as_mut_ptr().cast()) };
        Ok(blob)
    }

    fn ctx_create(&mut self, ctx_id: u32, name: &str) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let name = CString::new(name).unwrap_or_default();
        let bytes = name.as_bytes();
        // SAFETY: `name` is a NUL-terminated buffer of `bytes.len()` visible
        // characters, alive across the call; the library copies it.
        let rc = unsafe { (self.api.context_create)(ctx_id, bytes.len() as u32, name.as_ptr()) };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("context_create: {rc}")));
        }
        self.contexts.push(ctx_id);
        Ok(())
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        if self.ensure_ready().is_err() {
            return;
        }
        self.contexts.retain(|c| *c != ctx_id);
        // SAFETY: plain id argument; the library ignores unknown ids.
        unsafe { (self.api.context_destroy)(ctx_id) };
    }

    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let mut c_args = CreateArgs {
            handle: args.resource_id,
            target: args.target,
            format: args.format,
            bind: args.bind,
            width: args.width,
            height: args.height,
            depth: args.depth,
            array_size: args.array_size,
            last_level: args.last_level,
            nr_samples: args.nr_samples,
            flags: args.flags,
        };
        // SAFETY: `c_args` matches `struct virgl_renderer_resource_create_args`
        // field for field; no iovecs are attached at creation (null, 0).
        let rc = unsafe { (self.api.resource_create)(&mut c_args, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("resource_create: {rc}")));
        }
        self.resources.insert(
            args.resource_id,
            ResourceState {
                width: args.width,
                height: args.height,
                ..ResourceState::default()
            },
        );
        Ok(())
    }

    fn resource_unref(&mut self, resource_id: u32) {
        if self.ensure_ready().is_err() {
            return;
        }
        self.detach_iov(resource_id);
        self.resources.remove(&resource_id);
        // SAFETY: plain id; any context attachments are severed by the
        // library itself on unref.
        unsafe { (self.api.resource_unref)(resource_id) };
    }

    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        if self.ensure_ready().is_err() {
            return;
        }
        // SAFETY: plain ids, both known live to the validation front.
        unsafe { (self.api.ctx_attach_resource)(ctx_id as c_int, resource_id as c_int) };
    }

    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        if self.ensure_ready().is_err() {
            return;
        }
        // SAFETY: plain ids.
        unsafe { (self.api.ctx_detach_resource)(ctx_id as c_int, resource_id as c_int) };
    }

    fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        self.ensure_ready()?;
        // Translate every guest page run into a host pointer through the
        // checked API *before* anything crosses the FFI: an entry outside
        // guest RAM fails the whole attach.
        let mut iov = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.length == 0 {
                continue;
            }
            let len = entry.length as usize;
            let slice = mem.get_slice(GuestAddress(entry.addr), len).map_err(|e| {
                CommandError::Unreadable {
                    addr: entry.addr,
                    reason: e.to_string(),
                }
            })?;
            iov.push(Iovec {
                // SAFETY-relevant: the pointer stays valid because the
                // attachment holds an `Arc<GuestMem>` clone for as long as the
                // library may use it (struct `Attachment`).
                iov_base: slice.ptr_guard_mut().as_ptr().cast(),
                iov_len: len,
            });
        }
        let mut iov = iov.into_boxed_slice();

        // Replace-in-place: the guest may attach without detaching first.
        self.detach_iov(resource_id);

        let state = self
            .resources
            .get_mut(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        let num = c_int::try_from(iov.len()).map_err(|_| CommandError::OutOfMemory)?;
        // SAFETY: `iov` is a live array of `num` iovecs whose base pointers
        // are valid guest-memory host addresses (checked above). The library
        // stores the *array pointer*, so the box is kept in `Attachment`
        // until `detach_iov`/reset returns it.
        let rc =
            unsafe { (self.api.resource_attach_iov)(resource_id as c_int, iov.as_mut_ptr(), num) };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("attach_iov: {rc}")));
        }
        state.attachment = Some(Attachment {
            _iov: iov,
            _mem: Arc::clone(mem),
        });
        Ok(())
    }

    fn detach_backing(&mut self, resource_id: u32) {
        if self.ensure_ready().is_err() {
            return;
        }
        self.detach_iov(resource_id);
    }

    fn transfer_to_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let mut region = VirglBox {
            x: xfer.region.x,
            y: xfer.region.y,
            z: xfer.region.z,
            w: xfer.region.w,
            h: xfer.region.h,
            d: xfer.region.d,
        };
        // SAFETY: the box is a stack value alive for the call; a null iov
        // means "use the attached backing", whose array we keep alive.
        let rc = unsafe {
            (self.api.transfer_write_iov)(
                xfer.resource_id,
                ctx_id,
                xfer.level as c_int,
                xfer.stride,
                xfer.layer_stride,
                &mut region,
                xfer.offset,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("transfer_to_host: {rc}")));
        }
        Ok(())
    }

    fn transfer_from_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let mut region = VirglBox {
            x: xfer.region.x,
            y: xfer.region.y,
            z: xfer.region.z,
            w: xfer.region.w,
            h: xfer.region.h,
            d: xfer.region.d,
        };
        // SAFETY: as in `transfer_to_host`; null iov reads back into the
        // attached backing store.
        let rc = unsafe {
            (self.api.transfer_read_iov)(
                xfer.resource_id,
                ctx_id,
                xfer.level,
                xfer.stride,
                xfer.layer_stride,
                &mut region,
                xfer.offset,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("transfer_from_host: {rc}")));
        }
        Ok(())
    }

    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        self.ensure_ready()?;
        // Copy to dword storage: the library decodes u32s and the gathered
        // request buffer is byte-aligned. The length is a validated multiple
        // of 4 (`validate_stream`).
        self.submit_buf.clear();
        self.submit_buf.reserve(stream.len() / 4);
        for chunk in stream.chunks_exact(4) {
            self.submit_buf
                .push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        let ndw = c_int::try_from(self.submit_buf.len())
            .map_err(|_| CommandError::StreamTooLarge(stream.len()))?;
        // SAFETY: the buffer holds `ndw` dwords and outlives the call (the
        // library decodes synchronously and does not keep the pointer).
        let rc = unsafe {
            (self.api.submit_cmd)(self.submit_buf.as_mut_ptr().cast(), ctx_id as c_int, ndw)
        };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("submit_cmd: {rc}")));
        }
        Ok(())
    }

    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let state = self
            .resources
            .get_mut(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        let (width, height) = (state.width, state.height);
        // Only plausibly-presentable resources get a shadow (a flush of a
        // 16k×16k texture must not allocate a gigabyte).
        if u64::from(width) * u64::from(height) > MAX_RESOURCE_PIXELS {
            return Err(CommandError::OutOfMemory);
        }
        let stride = width as usize * 4;
        let frame = stride * height as usize;
        if state.shadow.len() != frame {
            state.shadow.clear();
            state
                .shadow
                .try_reserve_exact(frame)
                .map_err(|_| CommandError::OutOfMemory)?;
            state.shadow.resize(frame, 0);
        }

        let mut region = VirglBox {
            x: rect.x,
            y: rect.y,
            z: 0,
            w: rect.width,
            h: rect.height,
            d: 1,
        };
        let offset = u64::from(rect.y) * stride as u64 + u64::from(rect.x) * 4;
        let mut iov = Iovec {
            iov_base: state.shadow.as_mut_ptr().cast(),
            iov_len: state.shadow.len(),
        };
        // SAFETY: the iovec covers the whole `frame`-byte shadow buffer;
        // `offset` + the rect rows at `stride` stay inside it because `rect`
        // fits `width`×`height` (validated by the caller). The library
        // finishes writing before returning.
        let rc = unsafe {
            (self.api.transfer_read_iov)(
                resource_id,
                0, // ctx 0: the resource itself, not a GL context's view
                0, // level
                0, // stride 0 = the resource's own row stride (our layout)
                0, // layer_stride
                &mut region,
                offset,
                &mut iov,
                1,
            )
        };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("read scanout: {rc}")));
        }

        // Pack the rect rows out of the full-frame layout.
        let row_bytes = rect.width as usize * 4;
        out.clear();
        out.try_reserve_exact(row_bytes * rect.height as usize)
            .map_err(|_| CommandError::OutOfMemory)?;
        for row in 0..rect.height as usize {
            let start = (rect.y as usize + row) * stride + rect.x as usize * 4;
            let src = state
                .shadow
                .get(start..start + row_bytes)
                .ok_or(CommandError::OutOfMemory)?;
            out.extend_from_slice(src);
        }
        Ok(())
    }

    fn reset(&mut self) {
        // May run on a vCPU thread (device reset is an MMIO write), where the
        // library's EGL context is not current — so only *mark*; the real
        // reset happens on the worker thread (`ensure_ready`). The
        // attachments move to the graveyard because the library still holds
        // their array pointers until `virgl_renderer_reset` actually runs.
        for state in self.resources.values_mut() {
            if let Some(attachment) = state.attachment.take() {
                self.graveyard.push(attachment);
            }
        }
        self.resources.clear();
        self.contexts.clear();
        if matches!(self.state, State::Ready) {
            self.state = State::NeedsReset;
        }
    }
}

impl Drop for VirglRenderer {
    fn drop(&mut self) {
        // Give every guest-memory pointer back; the library state itself is
        // deliberately *not* torn down. `virgl_renderer_cleanup` terminates
        // EGL, which unloads mesa's driver while its worker threads still
        // hold TLS destructors — an observed SIGSEGV at thread exit (see
        // `Api::_lib`). One process = at most one initialized renderer, for
        // the whole process lifetime; only `load` before first init clears
        // the guard.
        let ids: Vec<u32> = self.resources.keys().copied().collect();
        match self.state {
            State::Ready | State::NeedsReset => {
                for id in ids {
                    self.detach_iov(id);
                    // SAFETY: plain id of a resource this renderer created.
                    unsafe { (self.api.resource_unref)(id) };
                }
                // SAFETY: frees every context/resource the library still
                // holds; our graveyard arrays outlive this call.
                unsafe { (self.api.reset)() };
                self.graveyard.clear();
                // The initialized library keeps the cookie and callbacks
                // pointers forever — so they must live forever (a few dozen
                // bytes, once per process).
                let callbacks = std::mem::replace(
                    &mut self.callbacks,
                    Box::new(Callbacks {
                        version: 0,
                        write_fence: None,
                        create_gl_context: None,
                        destroy_gl_context: None,
                        make_current: None,
                        get_drm_fd: None,
                    }),
                );
                std::mem::forget(callbacks);
                std::mem::forget(std::mem::replace(&mut self.cookie, Box::new(0)));
                // RENDERER_LIVE stays true: re-initializing without cleanup
                // would leak an EGL display per cycle.
            }
            State::Loaded => {
                // Never initialized: nothing global happened, a fresh load
                // may try again.
                self.graveyard.clear();
                RENDERER_LIVE.store(false, Ordering::SeqCst);
            }
        }
    }
}
