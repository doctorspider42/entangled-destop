//! The VMM half of the isolated renderer (ADR-0004's GPU-012 amendment):
//! spawn a helper process, forward validated commands to it, and treat its
//! death as a *degradation*, never a failure of the VM.
//!
//! Unix-only because the transport is a `UnixStream` pair; the protocol it
//! speaks is portable (see [`super::protocol`]), which is what keeps a future
//! Windows/ANGLE renderer from needing a different architecture.
//!
//! # The one rule
//!
//! Every method funnels through [`RemoteRenderer::call`], and **any** wire
//! error there marks the renderer dead and is reported as an in-band
//! [`CommandError`]. Nothing in this file may panic on a broken pipe, a
//! truncated reply or a helper that exits: those are the expected outcomes
//! this module exists to survive.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use virtio_core::{GuestMem, HostWaker};

use crate::blob::{BlobMapping, BlobSupport};
use crate::error::CommandError;
use crate::protocol::{MemEntry, Rect, ResourceCreate3d, ResourceCreateBlob, Transfer3d};
use crate::renderer::{CapsetInfo, FenceOutcome, Renderer3d};
use crate::resource::{read_backing, write_backing};

use super::protocol::{
    Reply, Request, REMOTE_MAX_BACKING, REMOTE_MAX_TOTAL_SHADOW, REMOTE_XFER_WINDOW, VERSION,
};
use super::{read_frame, write_frame, WireError};

/// How long the client waits for the helper's handshake before giving up.
///
/// The handshake itself is fast — capset sizes are static tables in
/// virglrenderer, readable before EGL comes up — so this only has to cover
/// process start and a `dlopen` on a busy machine.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the fence monitor asks the device to poll while host fences are
/// outstanding (the in-process renderer's tick, plus a little slack for the
/// round trip).
const FENCE_TICK: std::time::Duration = std::time::Duration::from_millis(2);

/// Idle sleep of the monitor when nothing is outstanding.
const FENCE_IDLE: std::time::Duration = std::time::Duration::from_millis(100);

/// Why a renderer process could not be started.
#[derive(Debug)]
pub enum SpawnError {
    /// The helper could not be launched at all.
    Spawn(std::io::Error),
    /// The socket pair could not be created.
    Socket(std::io::Error),
    /// The helper started but the handshake failed — most often because it
    /// could not load virglrenderer, and its message says so.
    Handshake(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot start the renderer process: {error}"),
            Self::Socket(error) => write!(f, "cannot create the renderer socket: {error}"),
            Self::Handshake(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Per-resource state the client keeps so the helper never needs guest
/// addresses.
#[derive(Default)]
struct RemoteResource {
    /// The guest's backing list, as attached.
    entries: Vec<MemEntry>,
    /// Guest memory the entries point into.
    mem: Option<Arc<GuestMem>>,
    /// Total backing length, which is also the helper's shadow size.
    len: u64,
}

/// Shared with the fence monitor thread.
struct MonitorState {
    outstanding: std::sync::atomic::AtomicUsize,
    stop: AtomicBool,
    waker: Arc<dyn HostWaker>,
}

struct FenceMonitor {
    state: Arc<MonitorState>,
    thread: std::thread::JoinHandle<()>,
}

impl FenceMonitor {
    fn spawn(waker: Arc<dyn HostWaker>) -> Option<Self> {
        let state = Arc::new(MonitorState {
            outstanding: std::sync::atomic::AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            waker,
        });
        let worker = Arc::clone(&state);
        std::thread::Builder::new()
            .name("virgl-remote-fence".into())
            .spawn(move || {
                while !worker.stop.load(Ordering::Acquire) {
                    if worker.outstanding.load(Ordering::Acquire) == 0 {
                        std::thread::park_timeout(FENCE_IDLE);
                        continue;
                    }
                    std::thread::park_timeout(FENCE_TICK);
                    if worker.stop.load(Ordering::Acquire) {
                        return;
                    }
                    if worker.outstanding.load(Ordering::Acquire) > 0 {
                        worker.waker.wake();
                    }
                }
            })
            .ok()
            .map(|thread| Self { state, thread })
    }

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
    }
}

/// [`Renderer3d`] backed by a renderer running in another process.
pub struct RemoteRenderer {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    /// Frame payload buffer, reused across calls.
    rx: Vec<u8>,
    /// Staging buffer for transfer spans, reused across calls.
    staging: Vec<u8>,
    capsets: Vec<CapsetInfo>,
    /// What the helper's renderer can do with blobs, learned once at
    /// handshake time (VEN-2001) — the device asks before it decides which
    /// feature bits to offer, so it cannot be a per-command query.
    blob_support: BlobSupport,
    resources: std::collections::HashMap<u32, RemoteResource>,
    /// Cleared the moment the helper stops answering. Everything after that
    /// fails in band and the device degrades to 2D (GPU-012).
    alive: bool,
    monitor: Option<FenceMonitor>,
    fences_in_flight: usize,
}

impl std::fmt::Debug for RemoteRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteRenderer")
            .field("pid", &self.child.id())
            .field("alive", &self.alive)
            .field("capsets", &self.capsets.len())
            .field("resources", &self.resources.len())
            .finish()
    }
}

impl RemoteRenderer {
    /// Spawns the default helper: this executable's `gpu-renderer`
    /// subcommand, which is how a production `entangled run` isolates the
    /// renderer.
    pub fn spawn() -> Result<Self, SpawnError> {
        let exe = std::env::current_exe().map_err(SpawnError::Spawn)?;
        let mut command = Command::new(exe);
        command.arg("gpu-renderer");
        Self::spawn_with(command)
    }

    /// Spawns an arbitrary helper command, which must speak the protocol on
    /// **stdin**. Tests use this to run a helper with a non-GL renderer, and
    /// it is the seam a future Windows implementation replaces wholesale.
    pub fn spawn_with(mut command: Command) -> Result<Self, SpawnError> {
        let (ours, theirs) = UnixStream::pair().map_err(SpawnError::Socket)?;
        // The helper gets its socket as stdin: no fd-passing games, no
        // filesystem path anyone else could connect to, and stdout/stderr stay
        // inherited so the helper's own log lines land in the VMM's terminal.
        command.stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)));
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::inherit());
        let child = command.spawn().map_err(SpawnError::Spawn)?;
        // The `Command` still holds the child's end of the socket pair, and
        // while *we* hold it open a dead child never shows up as EOF — the
        // handshake would wait out its whole timeout instead of failing at
        // once. Dropping the command closes our copy, which is what makes
        // "the renderer died" observable.
        drop(command);

        // A helper that dies during bring-up (no virglrenderer, no EGL) must
        // fail the *run*, loudly, rather than hang it: ADR-0004 §7's rule that
        // a profile which asked for 3D never silently gets llvmpipe.
        ours.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(SpawnError::Socket)?;
        let reader = BufReader::new(ours.try_clone().map_err(SpawnError::Socket)?);
        let writer = BufWriter::new(ours);

        let mut renderer = Self {
            child,
            reader,
            writer,
            rx: Vec::new(),
            staging: Vec::new(),
            capsets: Vec::new(),
            blob_support: BlobSupport::NONE,
            resources: std::collections::HashMap::new(),
            alive: true,
            monitor: None,
            fences_in_flight: 0,
        };

        match renderer.call(&Request::Hello { version: VERSION }) {
            Ok(Reply::Capsets(capsets)) if !capsets.is_empty() => {
                tracing::info!(
                    pid = renderer.child.id(),
                    capsets = capsets.len(),
                    "3D renderer running in an isolated process (GPU-012)"
                );
                renderer.capsets = capsets;
            }
            Ok(Reply::Error(message)) => {
                renderer.shutdown();
                return Err(SpawnError::Handshake(message));
            }
            Ok(other) => {
                renderer.shutdown();
                return Err(SpawnError::Handshake(format!(
                    "the renderer process answered the handshake with {other:?}"
                )));
            }
            Err(error) => {
                renderer.shutdown();
                return Err(SpawnError::Handshake(format!(
                    "the renderer process did not come up: {error}"
                )));
            }
        }
        // Second half of the handshake (VEN-2001): what the helper can do with
        // blobs. Still inside the read timeout, because a helper that cannot
        // answer this cannot answer anything. A refusal is not fatal — it just
        // means no blob feature bit — so an error here is recorded, not
        // propagated.
        match renderer.call(&Request::BlobSupport) {
            Ok(Reply::BlobSupport(support)) => {
                tracing::info!(
                    pid = renderer.child.id(),
                    guest = support.guest,
                    host3d = support.host3d,
                    host_visible_bytes = support.host_visible_bytes.unwrap_or(0),
                    "isolated renderer blob support"
                );
                renderer.blob_support = support;
            }
            Ok(other) => {
                renderer.shutdown();
                return Err(SpawnError::Handshake(format!(
                    "the renderer process answered the blob-support query with {other:?}"
                )));
            }
            Err(error) => {
                renderer.shutdown();
                return Err(SpawnError::Handshake(format!(
                    "the renderer process did not answer the blob-support query: {error}"
                )));
            }
        }

        // Handshake done: no more read timeouts. A command that takes longer
        // than a handshake is a busy GPU, not a dead helper, and the device's
        // own fence watchdog is what covers a genuinely stuck host.
        let _ = renderer.reader.get_ref().set_read_timeout(None);
        Ok(renderer)
    }

    /// Process id of the helper — for logs, for `entangled doctor`, and for a
    /// test that needs to kill it.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// One request, one reply. **Every** failure here means the renderer is
    /// gone: mark it and report in band.
    fn call(&mut self, request: &Request) -> Result<Reply, CommandError> {
        if !self.alive {
            return Err(CommandError::Renderer("the 3D renderer is gone".into()));
        }
        match self.exchange(request) {
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.mark_dead(&error);
                Err(CommandError::Renderer(format!(
                    "the 3D renderer process was lost: {error}"
                )))
            }
        }
    }

    fn exchange(&mut self, request: &Request) -> Result<Reply, WireError> {
        write_frame(&mut self.writer, request.tag(), &request.encode())?;
        let tag = read_frame(&mut self.reader, &mut self.rx)?;
        Ok(Reply::decode(tag, &self.rx)?)
    }

    /// A reply that is either `Ok` or an in-band renderer error.
    fn call_ok(&mut self, request: &Request) -> Result<(), CommandError> {
        match self.call(request)? {
            Reply::Ok => Ok(()),
            Reply::Error(message) => Err(CommandError::Renderer(message)),
            other => Err(CommandError::Renderer(format!(
                "unexpected reply {other:?}"
            ))),
        }
    }

    /// Same, for calls whose failure the trait cannot report (`ctx_destroy`,
    /// `resource_unref`, …): log and carry on.
    fn call_quiet(&mut self, request: &Request) {
        if let Err(error) = self.call_ok(request) {
            tracing::debug!(%error, ?request, "isolated renderer rejected a teardown call");
        }
    }

    fn call_bytes(&mut self, request: &Request) -> Result<Vec<u8>, CommandError> {
        match self.call(request)? {
            Reply::Bytes(bytes) => Ok(bytes),
            Reply::Error(message) => Err(CommandError::Renderer(message)),
            other => Err(CommandError::Renderer(format!(
                "unexpected reply {other:?}"
            ))),
        }
    }

    fn mark_dead(&mut self, error: &WireError) {
        if !self.alive {
            return;
        }
        self.alive = false;
        let status = self.child.try_wait().ok().flatten();
        tracing::error!(
            pid = self.child.id(),
            ?status,
            %error,
            "the isolated 3D renderer died; the VM keeps running and virtio-gpu \
             degrades to 2D (ADR-0004 GPU-012)"
        );
        // The device will be told through `is_alive`; wake it now so it does
        // not wait for the guest to kick before releasing what it held.
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.state.waker.wake();
        }
    }

    /// Kills the helper and reaps it. Idempotent.
    fn shutdown(&mut self) {
        self.alive = false;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The span of a resource's backing that one transfer may need.
    ///
    /// The exact span is unknowable here (bytes-per-element belongs to the
    /// format, which is the renderer's), so this is `offset`-anchored and
    /// bounded by [`REMOTE_XFER_WINDOW`] — big enough for every real transfer
    /// mesa issues, small enough to bound what one guest command pushes
    /// through the socket.
    fn transfer_span(resource: &RemoteResource, xfer: &Transfer3d) -> (u64, usize) {
        let offset = xfer.offset.min(resource.len);
        let remaining = resource.len.saturating_sub(offset);
        let len = remaining.min(REMOTE_XFER_WINDOW as u64);
        (offset, usize::try_from(len).unwrap_or(0))
    }
}

impl Drop for RemoteRenderer {
    fn drop(&mut self) {
        // Monitor first: it holds a waker that would otherwise poke a device
        // that is going away.
        self.monitor = None;
        self.shutdown();
    }
}

impl Renderer3d for RemoteRenderer {
    fn capsets(&self) -> &[CapsetInfo] {
        &self.capsets
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        self.call_bytes(&Request::Capset { id, version })
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, name: &str) -> Result<(), CommandError> {
        self.call_ok(&Request::CtxCreate {
            ctx_id,
            capset_id,
            name: name.to_owned(),
        })
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        self.call_quiet(&Request::CtxDestroy { ctx_id });
    }

    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> Result<(), CommandError> {
        self.call_ok(&Request::ResourceCreate(*args))?;
        self.resources
            .insert(args.resource_id, RemoteResource::default());
        Ok(())
    }

    fn resource_unref(&mut self, resource_id: u32) {
        self.resources.remove(&resource_id);
        self.call_quiet(&Request::ResourceUnref { resource_id });
    }

    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.call_quiet(&Request::CtxAttach {
            ctx_id,
            resource_id,
        });
    }

    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.call_quiet(&Request::CtxDetach {
            ctx_id,
            resource_id,
        });
    }

    fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        let len = entries
            .iter()
            .fold(0u64, |sum, e| sum.saturating_add(u64::from(e.length)));
        // Both bounds are checked here as well as in the helper: the guest
        // gets a clean ERR_OUT_OF_MEMORY instead of a renderer error string,
        // and nothing crosses the wire that is going to be refused anyway.
        // (The helper checks too, because it does not trust us either.)
        let held: u64 = self
            .resources
            .iter()
            .filter(|(id, _)| **id != resource_id)
            .map(|(_, resource)| resource.len)
            .sum();
        if len > REMOTE_MAX_BACKING || held.saturating_add(len) > REMOTE_MAX_TOTAL_SHADOW {
            // Honest refusal: the isolated renderer really cannot shadow it.
            tracing::warn!(
                resource_id,
                len,
                held,
                "attach refused: over the isolated renderer's shadow-backing budget"
            );
            return Err(CommandError::OutOfMemory);
        }
        self.call_ok(&Request::AttachBacking { resource_id, len })?;
        let resource = self.resources.entry(resource_id).or_default();
        resource.entries = entries.to_vec();
        resource.mem = Some(Arc::clone(mem));
        resource.len = len;
        Ok(())
    }

    fn detach_backing(&mut self, resource_id: u32) {
        if let Some(resource) = self.resources.get_mut(&resource_id) {
            resource.entries.clear();
            resource.mem = None;
            resource.len = 0;
        }
        self.call_quiet(&Request::DetachBacking { resource_id });
    }

    fn transfer_to_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        let resource = self
            .resources
            .get(&xfer.resource_id)
            .ok_or(CommandError::UnknownResource(xfer.resource_id))?;
        let mem = Arc::clone(
            resource
                .mem
                .as_ref()
                .ok_or(CommandError::NoBacking(xfer.resource_id))?,
        );
        let entries = resource.entries.clone();
        let (offset, len) = Self::transfer_span(resource, xfer);
        // Guest pages are read here, in the VMM, through the checked API — the
        // helper never sees a guest address.
        let mut staging = std::mem::take(&mut self.staging);
        staging.clear();
        let reserve = staging.try_reserve(len);
        let outcome = match reserve {
            Ok(()) => {
                staging.resize(len, 0);
                read_backing(&mem, &entries, offset, &mut staging)
            }
            Err(_) => Err(CommandError::OutOfMemory),
        };
        if let Err(error) = outcome {
            self.staging = staging;
            return Err(error);
        }
        let request = Request::TransferToHost {
            ctx_id,
            xfer: *xfer,
            shadow_offset: offset,
            bytes: std::mem::take(&mut staging),
        };
        let result = self.call_ok(&request);
        // Reclaim the buffer for the next transfer.
        if let Request::TransferToHost { bytes, .. } = request {
            staging = bytes;
        }
        self.staging = staging;
        result
    }

    fn transfer_from_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        let resource = self
            .resources
            .get(&xfer.resource_id)
            .ok_or(CommandError::UnknownResource(xfer.resource_id))?;
        let mem = Arc::clone(
            resource
                .mem
                .as_ref()
                .ok_or(CommandError::NoBacking(xfer.resource_id))?,
        );
        let entries = resource.entries.clone();
        let (offset, len) = Self::transfer_span(resource, xfer);
        let bytes = self.call_bytes(&Request::TransferFromHost {
            ctx_id,
            xfer: *xfer,
            shadow_offset: offset,
            len: u32::try_from(len).unwrap_or(u32::MAX),
        })?;
        // The helper's answer is untrusted input: write only as much as the
        // guest's backing actually covers, through the checked API.
        write_backing(&mem, &entries, offset, &bytes)
    }

    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        self.call_ok(&Request::Submit {
            ctx_id,
            stream: stream.to_vec(),
        })
    }

    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        let pixels = self.call_bytes(&Request::ReadRect {
            resource_id,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        })?;
        let expected = usize::try_from(rect.pixels().saturating_mul(4)).unwrap_or(usize::MAX);
        if pixels.len() != expected {
            return Err(CommandError::Renderer(format!(
                "the renderer returned {} bytes for a {}x{} rect, expected {expected}",
                pixels.len(),
                rect.width,
                rect.height
            )));
        }
        *out = pixels;
        Ok(())
    }

    fn reset(&mut self) {
        self.resources.clear();
        self.fences_in_flight = 0;
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.set_outstanding(0);
        }
        self.call_quiet(&Request::Reset);
    }

    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        self.monitor = FenceMonitor::spawn(waker);
    }

    fn create_fence(&mut self, ctx_id: u32, fence_id: u32) -> Result<FenceOutcome, CommandError> {
        // Without a monitor nothing would ask us to poll, so a deferred
        // response could never complete.
        if self.monitor.is_none() {
            return Ok(FenceOutcome::Signalled);
        }
        match self.call(&Request::CreateFence { ctx_id, fence_id })? {
            Reply::Fence { pending: true } => {
                self.fences_in_flight = self.fences_in_flight.saturating_add(1);
                if let Some(monitor) = self.monitor.as_ref() {
                    monitor.set_outstanding(self.fences_in_flight);
                }
                Ok(FenceOutcome::Pending)
            }
            Reply::Fence { pending: false } => Ok(FenceOutcome::Signalled),
            Reply::Error(message) => Err(CommandError::Renderer(message)),
            other => Err(CommandError::Renderer(format!(
                "unexpected reply {other:?}"
            ))),
        }
    }

    fn poll_fences(&mut self, still_pending: usize) -> Vec<u32> {
        if !self.alive || self.monitor.is_none() {
            return Vec::new();
        }
        let retired = match self.call(&Request::PollFences) {
            Ok(Reply::Fences(ids)) => ids,
            Ok(_) | Err(_) => Vec::new(),
        };
        self.fences_in_flight = still_pending.saturating_sub(retired.len());
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.set_outstanding(self.fences_in_flight);
        }
        retired
    }

    fn is_alive(&self) -> bool {
        self.alive
    }

    // ------------------------------------- blob resources (EPIC 20/VEN-2001)

    fn blob_support(&self) -> BlobSupport {
        self.blob_support
    }

    fn create_blob(
        &mut self,
        args: &ResourceCreateBlob,
        _mem: &Arc<GuestMem>,
        _entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        // Guest pages deliberately do not cross the boundary (GPU-012): the
        // helper gets the blob's *identity* and size, never an address.
        self.call_ok(&Request::CreateBlob(*args))
    }

    fn destroy_blob(&mut self, resource_id: u32) {
        self.call_quiet(&Request::DestroyBlob { resource_id });
    }

    fn map_blob(
        &mut self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<BlobMapping, CommandError> {
        match self.call(&Request::MapBlob {
            resource_id,
            offset,
            size,
        })? {
            Reply::Mapping { map_info } => Ok(BlobMapping { map_info }),
            Reply::Error(message) => Err(CommandError::Renderer(message)),
            other => Err(CommandError::Renderer(format!(
                "unexpected reply {other:?}"
            ))),
        }
    }

    fn unmap_blob(&mut self, resource_id: u32, offset: u64) {
        self.call_quiet(&Request::UnmapBlob {
            resource_id,
            offset,
        });
    }
}
