//! The helper half of the isolated renderer (ADR-0004's GPU-012 amendment):
//! the loop that runs *inside* the process that is allowed to crash.
//!
//! It owns a [`Renderer3d`] — in production the real `VirglRenderer`, in tests
//! whatever the caller passes — and answers requests from the VMM one at a
//! time. There is no concurrency: GL is thread-affine, the protocol is strict
//! request/response, and a single-threaded server is the whole reason the
//! client can treat "the socket went quiet" as "the renderer died".
//!
//! # Shadow backings
//!
//! The VMM never sends guest addresses (see [`super`]), so a resource's
//! backing lives here as a private memory region — allocated with the *same*
//! `GuestMem` type real guest RAM uses, one region per resource, so the
//! renderer's `attach_backing` translates its entries through exactly the
//! checked `vm-memory` path it always does. Nothing in this file needs a raw
//! pointer or an `unsafe` block for it.
//!
//! A `TransferToHost` writes the received span into that region before the
//! renderer reads it; a `TransferFromHost` reads the span back out after the
//! renderer wrote it. Both go through checked accesses: the VMM is not trusted
//! here either, and a helper that panicked would look exactly like the crash
//! this design exists to contain.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use vm_memory::{Bytes, GuestAddress};

use virtio_core::GuestMem;

use crate::protocol::{MemEntry, Rect};
use crate::renderer::{FenceOutcome, Renderer3d};

use super::protocol::{Reply, Request, REMOTE_MAX_BACKING, REMOTE_MAX_TOTAL_SHADOW, VERSION};
use super::{read_frame, write_frame, WireError};

/// One resource's host-side stand-in for the guest backing: a private
/// `GuestMem` region of exactly the length the VMM reported, addressed from 0.
struct Shadow {
    mem: Arc<GuestMem>,
    len: u64,
}

impl Shadow {
    fn new(len: u64) -> Result<Self, String> {
        let size = usize::try_from(len).map_err(|_| "backing length does not fit this host")?;
        let mem = GuestMem::from_ranges(&[(GuestAddress(0), size)])
            .map_err(|e| format!("cannot allocate a {len}-byte backing shadow: {e}"))?;
        Ok(Self {
            mem: Arc::new(mem),
            len,
        })
    }

    /// The single-entry backing list describing the whole shadow.
    fn entries(&self) -> Vec<MemEntry> {
        vec![MemEntry {
            addr: 0,
            length: u32::try_from(self.len).unwrap_or(u32::MAX),
        }]
    }
}

/// Runs the protocol against `renderer` until the peer closes the connection.
///
/// Returns `Ok(())` on a clean close (the VMM shut the VM down) and an error
/// only for a protocol violation — a crash of the *renderer* never comes back
/// through here, which is the point: the process simply dies and the client
/// notices.
pub fn serve(stream: UnixStream, mut renderer: Box<dyn Renderer3d>) -> Result<(), WireError> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut payload = Vec::new();
    let mut shadows: HashMap<u32, Shadow> = HashMap::new();

    loop {
        let tag = match read_frame(&mut reader, &mut payload) {
            Ok(tag) => tag,
            Err(WireError::Closed) => {
                tracing::debug!("the VMM closed the renderer connection; exiting");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let request = match Request::decode(tag, &payload) {
            Ok(request) => request,
            Err(error) => {
                // A frame this build cannot parse is fatal to the *session*,
                // not something to guess at.
                let reply = Reply::Error(error.to_string());
                write_frame(&mut writer, reply.tag(), &reply.encode())?;
                return Err(error.into());
            }
        };
        let reply = handle(&mut renderer, &mut shadows, request);
        write_frame(&mut writer, reply.tag(), &reply.encode())?;
    }
}

/// Serves on **stdin**, which is where [`super::RemoteRenderer`] puts the
/// socket. This is the whole body of the helper subcommand.
pub fn serve_stdin(renderer: Box<dyn Renderer3d>) -> Result<(), WireError> {
    // SAFETY: fd 0 is a `UnixStream` end the parent installed as this
    // process's stdin (`Stdio::from(socket)`), and this function is the only
    // consumer of it — nothing else in the helper reads stdin, so taking
    // ownership of the descriptor here cannot alias another owner.
    let stream = unsafe { UnixStream::from_raw_fd(0) };
    serve(stream, renderer)
}

fn error(message: impl std::fmt::Display) -> Reply {
    Reply::Error(message.to_string())
}

fn handle(
    renderer: &mut Box<dyn Renderer3d>,
    shadows: &mut HashMap<u32, Shadow>,
    request: Request,
) -> Reply {
    match request {
        Request::Hello { version } => {
            if version != VERSION {
                return error(format!(
                    "renderer protocol mismatch: the VMM speaks {version}, this helper speaks \
                     {VERSION}"
                ));
            }
            Reply::Capsets(renderer.capsets().to_vec())
        }
        Request::Capset { id, version } => match renderer.capset(id, version) {
            Ok(blob) => Reply::Bytes(blob),
            Err(e) => error(e),
        },
        Request::CtxCreate { ctx_id, name } => match renderer.ctx_create(ctx_id, &name) {
            Ok(()) => Reply::Ok,
            Err(e) => error(e),
        },
        Request::CtxDestroy { ctx_id } => {
            renderer.ctx_destroy(ctx_id);
            Reply::Ok
        }
        Request::ResourceCreate(args) => match renderer.resource_create_3d(&args) {
            Ok(()) => Reply::Ok,
            Err(e) => error(e),
        },
        Request::ResourceUnref { resource_id } => {
            renderer.resource_unref(resource_id);
            shadows.remove(&resource_id);
            Reply::Ok
        }
        Request::CtxAttach {
            ctx_id,
            resource_id,
        } => {
            renderer.ctx_attach_resource(ctx_id, resource_id);
            Reply::Ok
        }
        Request::CtxDetach {
            ctx_id,
            resource_id,
        } => {
            renderer.ctx_detach_resource(ctx_id, resource_id);
            Reply::Ok
        }
        Request::AttachBacking { resource_id, len } => {
            if len == 0 {
                return error("a backing of zero bytes");
            }
            if len > REMOTE_MAX_BACKING {
                return error(format!(
                    "backing of {len} bytes exceeds the isolated renderer's {REMOTE_MAX_BACKING} \
                     byte per-resource limit"
                ));
            }
            // The total budget, which is the bound isolation actually needs:
            // every shadow is host memory the in-process renderer would never
            // have allocated (there a backing is guest RAM). Re-attaching the
            // same resource replaces its shadow, so its old size does not
            // count against the budget.
            let held: u64 = shadows
                .iter()
                .filter(|(id, _)| **id != resource_id)
                .map(|(_, shadow)| shadow.len)
                .sum();
            if held.saturating_add(len) > REMOTE_MAX_TOTAL_SHADOW {
                return error(format!(
                    "backing of {len} bytes would put the isolated renderer over its \
                     {REMOTE_MAX_TOTAL_SHADOW}-byte total shadow budget ({held} already held)"
                ));
            }
            let shadow = match Shadow::new(len) {
                Ok(shadow) => shadow,
                Err(message) => return error(message),
            };
            let entries = shadow.entries();
            let mem = Arc::clone(&shadow.mem);
            // Inserted before the attach so the region (whose pointers the
            // renderer keeps) is owned for at least as long as the attachment.
            shadows.insert(resource_id, shadow);
            match renderer.attach_backing(resource_id, &mem, &entries) {
                Ok(()) => Reply::Ok,
                Err(e) => {
                    shadows.remove(&resource_id);
                    error(e)
                }
            }
        }
        Request::DetachBacking { resource_id } => {
            // The renderer gives the pointers back first, then the region goes.
            renderer.detach_backing(resource_id);
            shadows.remove(&resource_id);
            Reply::Ok
        }
        Request::TransferToHost {
            ctx_id,
            xfer,
            shadow_offset,
            bytes,
        } => {
            if let Some(shadow) = shadows.get(&xfer.resource_id) {
                if let Err(e) = shadow.mem.write_slice(&bytes, GuestAddress(shadow_offset)) {
                    return error(format!(
                        "transfer span of {} bytes at {shadow_offset} does not fit the \
                         {}-byte backing: {e}",
                        bytes.len(),
                        shadow.len
                    ));
                }
            }
            match renderer.transfer_to_host(ctx_id, &xfer) {
                Ok(()) => Reply::Ok,
                Err(e) => error(e),
            }
        }
        Request::TransferFromHost {
            ctx_id,
            xfer,
            shadow_offset,
            len,
        } => {
            if let Err(e) = renderer.transfer_from_host(ctx_id, &xfer) {
                return error(e);
            }
            let Some(shadow) = shadows.get(&xfer.resource_id) else {
                return error("no backing shadow for this resource");
            };
            // Clamp rather than fail: the client asks for a bounded window and
            // a short tail at the end of the backing is normal.
            let available = shadow.len.saturating_sub(shadow_offset.min(shadow.len));
            let want = usize::try_from(u64::from(len).min(available)).unwrap_or(0);
            let mut span = vec![0u8; want];
            match shadow
                .mem
                .read_slice(&mut span, GuestAddress(shadow_offset))
            {
                Ok(()) => Reply::Bytes(span),
                Err(e) => error(format!("readback span is outside the backing: {e}")),
            }
        }
        Request::Submit { ctx_id, stream } => match renderer.submit(ctx_id, &stream) {
            Ok(()) => Reply::Ok,
            Err(e) => error(e),
        },
        Request::ReadRect {
            resource_id,
            x,
            y,
            width,
            height,
        } => {
            let mut pixels = Vec::new();
            match renderer.read_rect_bgra(
                resource_id,
                Rect {
                    x,
                    y,
                    width,
                    height,
                },
                &mut pixels,
            ) {
                Ok(()) => Reply::Bytes(pixels),
                Err(e) => error(e),
            }
        }
        Request::Reset => {
            renderer.reset();
            shadows.clear();
            Reply::Ok
        }
        Request::CreateFence { ctx_id, fence_id } => {
            match renderer.create_fence(ctx_id, fence_id) {
                Ok(FenceOutcome::Pending) => Reply::Fence { pending: true },
                Ok(FenceOutcome::Signalled) => Reply::Fence { pending: false },
                Err(e) => error(e),
            }
        }
        // The helper polls its own renderer synchronously: it has no waker and
        // needs none, because the *client's* monitor is what drives this call.
        // `usize::MAX` as "still pending" keeps a renderer that ticks its own
        // monitor (the in-process virgl one, if a helper ever installs a
        // waker) from concluding that nothing is outstanding.
        Request::PollFences => Reply::Fences(renderer.poll_fences(usize::MAX)),
    }
}
