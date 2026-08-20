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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

use virtio_core::GuestMem;

use virtio_core::HostWaker;

use crate::error::CommandError;
use crate::protocol::{MemEntry, Rect, ResourceCreate3d, Transfer3d};
use crate::renderer::{CapsetInfo, FenceOutcome, Renderer3d};
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

/// Environment variable that suppresses the Venus probe entirely, for a host
/// whose newer virglrenderer has venus compiled in but whose Vulkan ICD is not
/// one anybody should be rendering against.
pub const VENUS_ENV: &str = "ENTANGLED_GPU_VENUS";

/// `struct virgl_renderer_resource_create_blob_args` (virglrenderer 0.10+).
///
/// Laid out field-for-field against the C header. It is only ever constructed
/// when the matching symbol resolved, i.e. when the loaded library is new
/// enough to define this struct at all.
#[repr(C)]
struct CreateBlobArgs {
    res_handle: u32,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    iovecs: *const Iovec,
    num_iovs: u32,
}

/// The Venus-era entry points (virglrenderer 0.10+), resolved *optionally*.
///
/// Runtime detection, never a compile-time fork — the same discipline the
/// whole `dlopen` design rests on (ADR-0004 §2). On jammy's 0.9.1 none of
/// these resolve, the probe says so once at load, and the device behaves
/// exactly as it did before Venus existed.
struct VenusApi {
    /// `virgl_renderer_context_create_with_flags(ctx_id, flags, nlen, name)`,
    /// where `flags` carries the capset id that selects the context type.
    context_create_with_flags: unsafe extern "C" fn(u32, u32, u32, *const c_char) -> c_int,
    /// `virgl_renderer_resource_create_blob(&args)`.
    resource_create_blob: unsafe extern "C" fn(*const CreateBlobArgs) -> c_int,
}

/// Symbols the Venus probe requires the library to have before it believes it.
///
/// Four, but only two are *stored* ([`VenusApi`]): `virgl_renderer_resource_map`
/// and `..._unmap` are what a host-visible mapping would call, and there is no
/// window to map into until the machine layer allocates one — so resolving
/// them is evidence about the library, not a capability we can use yet
/// (VEN-2003; see `map_blob`).
const VENUS_SYMBOLS: [&str; 4] = [
    "virgl_renderer_context_create_with_flags",
    "virgl_renderer_resource_create_blob",
    "virgl_renderer_resource_map",
    "virgl_renderer_resource_unmap",
];

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

/// The init cookie: what the library hands back to every callback.
///
/// Its *address* is registered with virglrenderer at init and retained for
/// the life of the process (see the module docs), so the box is leaked on
/// drop and the callback may always dereference it.
struct Cookie {
    /// Recognizable in a debugger ("virg"); the library only round-trips the
    /// pointer.
    _magic: u32,
    /// Host fence ids the library has retired, in retirement order, waiting
    /// to be collected by [`VirglRenderer::poll_fences`].
    ///
    /// A `Mutex` because the pointer is shared with C: in practice
    /// `write_fence` only ever runs on the thread inside
    /// `virgl_renderer_poll` (our worker), so it is uncontended.
    retired: Mutex<Vec<u32>>,
}

/// `virgl_renderer_callbacks::write_fence` — the library telling us a fence
/// retired (ADR-0004 phase 2).
///
/// Called from inside `virgl_renderer_poll`, i.e. synchronously on the device
/// worker thread, so all this does is append the id for the caller to pick
/// up. Nothing here may panic: it is a C callback frame.
extern "C" fn write_fence(cookie: *mut c_void, fence: u32) {
    if cookie.is_null() {
        return;
    }
    // SAFETY: the pointer is the `Cookie` box registered at
    // `virgl_renderer_init`, which is leaked rather than freed (see `Drop`),
    // so it is still live whenever the library calls back. The library never
    // hands this pointer to anything else and we only take a shared
    // reference.
    let cookie = unsafe { &*cookie.cast::<Cookie>() };
    if let Ok(mut retired) = cookie.retired.lock() {
        // Bound the list the same way the device bounds its fence table: a
        // runaway host cannot make this grow without limit.
        if retired.len() < crate::fence::MAX_PENDING_FENCES * 4 {
            retired.push(fence);
        }
    }
}

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
    /// `virgl_renderer_create_fence(client_fence_id, ctx_id)` — phase 2.
    create_fence: unsafe extern "C" fn(c_int, u32) -> c_int,
    /// `virgl_renderer_poll()`: retires finished fences, calling `write_fence`
    /// for each, synchronously on the calling thread.
    poll: unsafe extern "C" fn(),
    /// `virgl_renderer_get_poll_fd()`: an fd that becomes readable when the
    /// library has fence work, or -1 on a host whose GL stack offers none.
    get_poll_fd: unsafe extern "C" fn() -> c_int,
    /// The Venus-era entry points, when this library has them (VEN-2003).
    venus: Option<VenusApi>,
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

/// How often the fence monitor asks the device to poll while host fences are
/// outstanding.
///
/// QEMU's equivalent timer runs at 10 ms; that is a quarter of a frame at
/// 60 Hz, which shows up as visible jitter. 1 ms costs one cheap
/// `virgl_renderer_poll` (a `glClientWaitSync(0)` scan of the fence list) per
/// millisecond *only while something is in flight*, and idles completely
/// otherwise.
const FENCE_TICK: Duration = Duration::from_millis(1);

/// How long the monitor sleeps when nothing is outstanding, as a backstop in
/// case an `unpark` is ever missed.
const FENCE_IDLE: Duration = Duration::from_millis(100);

/// State shared with the fence-monitor thread (ADR-0004 phase 2).
struct MonitorState {
    /// Host fences the *device* is still waiting for. Set from the worker
    /// thread; read by the monitor to decide whether to keep ticking.
    outstanding: AtomicUsize,
    stop: AtomicBool,
    waker: Arc<dyn HostWaker>,
}

/// The thread that turns "the host GPU finished" into a device notification.
///
/// It never touches the library itself — GL is thread-affine, so *polling*
/// must happen on the worker thread. All it does is wake the worker while
/// fences are outstanding; the worker then calls `poll_fences`, which is
/// where `virgl_renderer_poll` actually runs.
struct FenceMonitor {
    state: Arc<MonitorState>,
    thread: std::thread::JoinHandle<()>,
}

impl FenceMonitor {
    fn spawn(waker: Arc<dyn HostWaker>) -> Option<Self> {
        let state = Arc::new(MonitorState {
            outstanding: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            waker,
        });
        let worker_state = Arc::clone(&state);
        match std::thread::Builder::new()
            .name("virgl-fence".into())
            .spawn(move || monitor_loop(worker_state))
        {
            Ok(thread) => Some(Self { state, thread }),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not start the virgl fence monitor; fences stay synchronous"
                );
                None
            }
        }
    }

    /// Tells the monitor how many fences the device is waiting for, waking it
    /// if it was idle.
    fn set_outstanding(&self, count: usize) {
        let previous = self.state.outstanding.swap(count, Ordering::Release);
        if previous == 0 && count > 0 {
            self.thread.thread().unpark();
        }
    }
}

impl Drop for FenceMonitor {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
        // Not joined: the monitor only sleeps and wakes, and a `VirglRenderer`
        // is dropped either at VM teardown (where a 1 ms straggler is
        // irrelevant) or on the worker thread it must not block.
    }
}

fn monitor_loop(state: Arc<MonitorState>) {
    while !state.stop.load(Ordering::Acquire) {
        if state.outstanding.load(Ordering::Acquire) == 0 {
            std::thread::park_timeout(FENCE_IDLE);
            continue;
        }
        std::thread::park_timeout(FENCE_TICK);
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        if state.outstanding.load(Ordering::Acquire) > 0 {
            state.waker.wake();
        }
    }
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
    /// The init cookie; its address is registered with the library, which is
    /// also how `write_fence` finds the retired-fence list.
    cookie: Box<Cookie>,
    /// The fence monitor, once a host waker has been installed and the thread
    /// started. `None` means fences complete synchronously (phase 1).
    monitor: Option<FenceMonitor>,
    /// `virgl_renderer_get_poll_fd()` as reported after init: >= 0 means the
    /// host GL stack has a real fence-completion fd. Recorded for the log and
    /// for `entangled doctor`; the monitor ticks either way, because polling
    /// itself has to happen on the worker thread.
    poll_fd: c_int,
    /// Host fences created but not yet reported retired — diagnostics, and
    /// what tells the monitor whether to keep ticking.
    fences_in_flight: usize,
    /// The callbacks struct. The library stores the *pointer* it was given at
    /// init (QEMU keeps a static for the same reason), so this must live
    /// exactly as long as the initialized library state.
    callbacks: Box<Callbacks>,
    /// The thread `virgl_renderer_init` ran on — the only thread whose EGL
    /// binding lets teardown calls execute GL. `Drop` on any other thread
    /// leaks instead of calling into the library (see `Drop`).
    init_thread: Option<std::thread::ThreadId>,
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
            create_fence: sym!("virgl_renderer_create_fence"),
            poll: sym!("virgl_renderer_poll"),
            get_poll_fd: sym!("virgl_renderer_get_poll_fd"),
            venus: None,
            _lib: std::mem::ManuallyDrop::new(lib),
        };

        // Capset versions/sizes are static tables in the library — readable
        // *before* init, which matters: the guest kernel reads `num_capsets`
        // from config space long before the first 3D command triggers EGL
        // bring-up, and a device that says 0 there leaves the whole guest
        // mesa stack capability-blind (observed: GNOME booted but every
        // format probe misfired).
        let mut capsets = Vec::new();
        for id in [CAPSET_VIRGL, CAPSET_VIRGL2] {
            let (mut max_version, mut max_size) = (0u32, 0u32);
            // SAFETY: out-pointers to locals, valid for the call; this entry
            // point only reads static size tables (verified pre-init).
            unsafe { (api.get_cap_set)(id, &mut max_version, &mut max_size) };
            if max_size > 0 {
                capsets.push(CapsetInfo {
                    id,
                    max_version,
                    max_size,
                });
            }
        }
        if capsets.is_empty() {
            return Err("virglrenderer reports no capability sets".into());
        }

        // ------------------------------------------------ the Venus probe
        //
        // Optional symbols, resolved all-or-nothing. A library with some but
        // not all of them is not one we know how to drive, and half a Venus
        // implementation is worse than none: the guest would negotiate the
        // capset and then fail on its first allocation.
        let mut api = api;
        if std::env::var(VENUS_ENV).as_deref() == Ok("off") {
            tracing::info!("{VENUS_ENV}=off: not probing virglrenderer for Venus support");
        } else {
            macro_rules! opt_sym {
                ($ty:ty, $name:literal) => {
                    // SAFETY: same contract as `sym!` above — looked up by C
                    // name in the library already opened, and the declared
                    // type is the 0.10+ prototype quoted on `VenusApi`. Unlike
                    // `sym!` a miss is not an error: it just means this
                    // library predates Venus.
                    unsafe { api._lib.get::<$ty>(concat!($name, " ").as_bytes()) }
                        .ok()
                        .map(|symbol| *symbol)
                };
            }
            let context_create_with_flags = opt_sym!(
                unsafe extern "C" fn(u32, u32, u32, *const c_char) -> c_int,
                "virgl_renderer_context_create_with_flags"
            );
            let resource_create_blob = opt_sym!(
                unsafe extern "C" fn(*const CreateBlobArgs) -> c_int,
                "virgl_renderer_resource_create_blob"
            );
            // Presence-only: see `VENUS_SYMBOLS`.
            let resource_map = opt_sym!(*const c_void, "virgl_renderer_resource_map");
            let resource_unmap = opt_sym!(*const c_void, "virgl_renderer_resource_unmap");

            let missing: Vec<&str> = VENUS_SYMBOLS
                .iter()
                .zip([
                    context_create_with_flags.is_some(),
                    resource_create_blob.is_some(),
                    resource_map.is_some(),
                    resource_unmap.is_some(),
                ])
                .filter(|(_, found)| !found)
                .map(|(name, _)| *name)
                .collect();
            let (mut venus_version, mut venus_size) = (0u32, 0u32);
            // SAFETY: out-pointers to locals; the entry point reads static
            // size tables and is safe before init (as for VIRGL above).
            unsafe { (api.get_cap_set)(crate::CAPSET_VENUS, &mut venus_version, &mut venus_size) };

            match (context_create_with_flags, resource_create_blob) {
                (Some(context_create_with_flags), Some(resource_create_blob))
                    if missing.is_empty() && venus_size > 0 =>
                {
                    api.venus = Some(VenusApi {
                        context_create_with_flags,
                        resource_create_blob,
                    });
                    capsets.push(CapsetInfo {
                        id: crate::CAPSET_VENUS,
                        max_version: venus_version,
                        max_size: venus_size,
                    });
                    tracing::info!(
                        max_version = venus_version,
                        max_size = venus_size,
                        "virglrenderer serves the Venus capset (VEN-2003)"
                    );
                }
                _ => tracing::info!(
                    venus_capset_bytes = venus_size,
                    missing = ?missing,
                    "virglrenderer has no usable Venus support; 3D stays classic virgl (VEN-2003)"
                ),
            }
        }
        let api = api;

        Ok(Self {
            api,
            state: State::Loaded,
            capsets,
            resources: HashMap::new(),
            contexts: Vec::new(),
            graveyard: Vec::new(),
            submit_buf: Vec::new(),
            cookie: Box::new(Cookie {
                _magic: 0x7669_7267,
                retired: Mutex::new(Vec::new()),
            }),
            monitor: None,
            poll_fd: -1,
            fences_in_flight: 0,
            callbacks: Box::new(Callbacks {
                version: 1,
                write_fence: Some(write_fence),
                create_gl_context: None,
                destroy_gl_context: None,
                make_current: None,
                get_drm_fd: None,
            }),
            init_thread: None,
        })
    }

    /// Brings the process-global renderer up (once) and applies a deferred
    /// reset. Called at the top of every trait method, i.e. always on the
    /// worker thread.
    fn ensure_ready(&mut self) -> Result<(), CommandError> {
        match self.state {
            State::Ready => Ok(()),
            State::Loaded => {
                let cookie = std::ptr::from_mut::<Cookie>(&mut *self.cookie).cast::<c_void>();
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
                self.init_thread = Some(std::thread::current().id());
                // SAFETY: no arguments; valid only after a successful init,
                // which is where we are. -1 means this host's GL stack has no
                // fence-completion fd (WSLg's d3d12 driver does not).
                self.poll_fd = unsafe { (self.api.get_poll_fd)() };
                tracing::info!(
                    capsets = self.capsets.len(),
                    poll_fd = self.poll_fd,
                    async_fences = self.monitor.is_some(),
                    "virglrenderer initialized"
                );
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

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, name: &str) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let name = CString::new(name).unwrap_or_default();
        let bytes = name.as_bytes();
        // A typed context needs `virgl_renderer_context_create_with_flags`
        // (0.10+), where `flags` *is* the capset id. Pinned 0.9 has only
        // `virgl_renderer_context_create`, which always makes a classic virgl
        // context — so a capset there is refused rather than silently
        // downgraded. `Gpu3d` has already refused a capset this renderer does
        // not advertise; this is the belt to that braces.
        let rc = match (&self.api.venus, capset_id) {
            (_, 0) => {
                // SAFETY: `name` is a NUL-terminated buffer of `bytes.len()`
                // visible characters, alive across the call; the library
                // copies it.
                unsafe { (self.api.context_create)(ctx_id, bytes.len() as u32, name.as_ptr()) }
            }
            (Some(venus), capset) => {
                // SAFETY: same buffer contract as above; the symbol was
                // resolved at load and its prototype is quoted on `VenusApi`.
                unsafe {
                    (venus.context_create_with_flags)(
                        ctx_id,
                        capset,
                        bytes.len() as u32,
                        name.as_ptr(),
                    )
                }
            }
            (None, capset) => return Err(CommandError::UnsupportedContextType(capset)),
        };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("context_create: {rc}")));
        }
        self.contexts.push(ctx_id);
        Ok(())
    }

    fn blob_support(&self) -> crate::blob::BlobSupport {
        // VEN-2001/VEN-2003. `host_visible_bytes` is deliberately `None` even
        // on a library that has `virgl_renderer_resource_map`: mapping a blob
        // into the *guest* needs a shared-memory window the machine layer has
        // to allocate and back with host pages, and neither host does that
        // yet. Claiming a window we cannot back would fail the guest's
        // `mmap` after it had already built a Vulkan allocation around it.
        crate::blob::BlobSupport {
            guest: self.api.venus.is_some(),
            host3d: self.api.venus.is_some(),
            host_visible_bytes: None,
        }
    }

    fn create_blob(
        &mut self,
        args: &crate::protocol::ResourceCreateBlob,
        _mem: &Arc<GuestMem>,
        _entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        self.ensure_ready()?;
        let venus = self
            .api
            .venus
            .as_ref()
            .ok_or(CommandError::UnsupportedBlobMem(args.blob_mem))?;
        // Only host-side blobs reach a renderer at all (the device keeps
        // guest-memory blobs to itself), so there are no iovecs to pass.
        let c_args = CreateBlobArgs {
            res_handle: args.resource_id,
            ctx_id: 0,
            blob_mem: args.blob_mem,
            blob_flags: args.blob_flags,
            blob_id: args.blob_id,
            size: args.size,
            iovecs: std::ptr::null(),
            num_iovs: 0,
        };
        // SAFETY: `c_args` is a live local of exactly the layout the 0.10 API
        // declares, and the library only reads it during the call (it copies
        // what it keeps). The null iovec pointer is legal for `num_iovs == 0`,
        // which is what a HOST3D blob is.
        let rc = unsafe { (venus.resource_create_blob)(&c_args) };
        if rc != 0 {
            return Err(CommandError::Renderer(format!(
                "resource_create_blob: {rc}"
            )));
        }
        self.resources
            .insert(args.resource_id, ResourceState::default());
        Ok(())
    }

    fn destroy_blob(&mut self, resource_id: u32) {
        if self.ensure_ready().is_err() {
            return;
        }
        self.resources.remove(&resource_id);
        // SAFETY: plain id argument; the library ignores unknown ids.
        unsafe { (self.api.resource_unref)(resource_id) };
    }

    fn map_blob(
        &mut self,
        _resource_id: u32,
        _offset: u64,
        _size: u64,
    ) -> Result<crate::blob::BlobMapping, CommandError> {
        // See `blob_support`: there is no window to map into yet. The FFI half
        // (`virgl_renderer_resource_map`) is resolved and ready; what is
        // missing is the machine layer's BAR/GPA window and the host mapping
        // behind it.
        Err(CommandError::NoHostVisibleWindow)
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
        // Only plausibly-presentable resources are read back (a flush of a
        // 16k x 16k texture must not allocate a gigabyte).
        if u64::from(width) * u64::from(height) > MAX_RESOURCE_PIXELS {
            return Err(CommandError::OutOfMemory);
        }

        // Phase 2: read the dirty rect **straight into the caller's packed
        // buffer** with an explicit row stride, instead of phase 1's
        // full-frame shadow plus a row-by-row repack. That removes one CPU
        // copy of every flushed rect and the frame-sized host allocation
        // (7.9 MiB at 1080p) from the present path — the part of the
        // zero-copy story a host without dmabuf can still have (ADR-0004
        // phase 2's scanout amendment).
        let row_bytes = usize::try_from(u64::from(rect.width) * 4).unwrap_or(usize::MAX);
        let frame = row_bytes
            .checked_mul(usize::try_from(rect.height).unwrap_or(usize::MAX))
            .ok_or(CommandError::OutOfMemory)?;
        out.clear();
        out.try_reserve_exact(frame)
            .map_err(|_| CommandError::OutOfMemory)?;
        out.resize(frame, 0);

        let mut region = VirglBox {
            x: rect.x,
            y: rect.y,
            z: 0,
            w: rect.width,
            h: rect.height,
            d: 1,
        };
        let mut iov = Iovec {
            iov_base: out.as_mut_ptr().cast(),
            iov_len: out.len(),
        };
        let stride = u32::try_from(row_bytes).map_err(|_| CommandError::OutOfMemory)?;
        // SAFETY: the iovec covers exactly the `frame` bytes of `out`, which
        // is what a `rect.height` x `stride` readback writes, and `rect` was
        // bounds-checked against the resource geometry by the caller
        // (`Gpu3d::read_rect_bgra`). The library finishes writing before it
        // returns and keeps no pointer.
        let rc = unsafe {
            (self.api.transfer_read_iov)(
                resource_id,
                0, // ctx 0: the resource itself, not a GL context's view
                0, // level
                stride,
                0, // layer_stride
                &mut region,
                0, // offset into the destination
                &mut iov,
                1,
            )
        };
        if rc != 0 {
            out.clear();
            return Err(CommandError::Renderer(format!("read scanout: {rc}")));
        }
        Ok(())
    }

    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        // One monitor per renderer; a second install (there is none today)
        // would replace it.
        self.monitor = FenceMonitor::spawn(waker);
    }

    fn create_fence(&mut self, ctx_id: u32, fence_id: u32) -> Result<FenceOutcome, CommandError> {
        self.ensure_ready()?;
        // Without a monitor nothing would ever ask us to poll, so a deferred
        // response could never complete: stay on phase 1's synchronous model.
        // This is also the WSLg-without-a-waker and the `ENTANGLED_QUEUE_NOTIFY=sync`
        // case.
        let Some(monitor) = self.monitor.as_ref() else {
            return Ok(FenceOutcome::Signalled);
        };
        // SAFETY: plain scalars. The fence id is the guest's, truncated to the
        // library's int; the context was validated live by `Gpu3d`.
        let rc = unsafe { (self.api.create_fence)(fence_id as c_int, ctx_id) };
        if rc != 0 {
            return Err(CommandError::Renderer(format!("create_fence: {rc}")));
        }
        self.fences_in_flight = self.fences_in_flight.saturating_add(1);
        monitor.set_outstanding(self.fences_in_flight);
        Ok(FenceOutcome::Pending)
    }

    fn poll_fences(&mut self, still_pending: usize) -> Vec<u32> {
        if !matches!(self.state, State::Ready) || self.monitor.is_none() {
            return Vec::new();
        }
        // SAFETY: no arguments. Runs `write_fence` for every retired fence,
        // synchronously on this (the EGL-owning worker) thread — which is the
        // only thread allowed to touch GL, and the reason polling cannot live
        // in the monitor thread.
        unsafe { (self.api.poll)() };
        let retired = match self.cookie.retired.lock() {
            Ok(mut list) => std::mem::take(&mut *list),
            Err(_) => Vec::new(),
        };
        // The device is the authority on what is still awaited (its watchdog
        // may have given up on a fence the library still owes us), so its
        // count — minus whatever this call just retired — is what the monitor
        // is told.
        self.fences_in_flight = still_pending.saturating_sub(retired.len());
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.set_outstanding(self.fences_in_flight);
        }
        retired
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
        // Fences belong to contexts that are about to stop existing; the
        // device drops its pending responses in the same reset, so nothing is
        // waiting for these any more.
        self.fences_in_flight = 0;
        if let Ok(mut retired) = self.cookie.retired.lock() {
            retired.clear();
        }
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.set_outstanding(0);
        }
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
                // EGL is bound to the init thread; teardown GL calls from any
                // other thread run without a current context (observed
                // SIGSEGV in the d3d12 driver). A cross-thread drop therefore
                // leaks the library-held resources — the process is on its
                // way out whenever a VM's device tree is dropped, and the
                // renderer is process-global either way.
                let same_thread = self.init_thread == Some(std::thread::current().id());
                if same_thread {
                    for id in ids {
                        self.detach_iov(id);
                        // SAFETY: plain id of a resource this renderer
                        // created, called on the EGL-owning thread.
                        unsafe { (self.api.resource_unref)(id) };
                    }
                    // SAFETY: frees every context/resource the library still
                    // holds; our graveyard arrays outlive this call.
                    unsafe { (self.api.reset)() };
                    self.graveyard.clear();
                } else {
                    tracing::debug!(
                        "virglrenderer dropped off its EGL thread; leaving the \
                         library state to the process teardown"
                    );
                    // The library may still hold every attachment's iovec
                    // array pointer — those allocations must outlive us too.
                    for id in ids {
                        if let Some(state) = self.resources.get_mut(&id) {
                            if let Some(attachment) = state.attachment.take() {
                                self.graveyard.push(attachment);
                            }
                        }
                    }
                    for attachment in self.graveyard.drain(..) {
                        std::mem::forget(attachment);
                    }
                }
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
                let cookie = std::mem::replace(
                    &mut self.cookie,
                    Box::new(Cookie {
                        _magic: 0,
                        retired: Mutex::new(Vec::new()),
                    }),
                );
                std::mem::forget(cookie);
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
