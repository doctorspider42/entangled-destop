//! The Venus **executor** (EPIC 20 stage 5a.3): the [`RingSink`] that stops
//! capturing and starts answering — it decodes each command in a ring,
//! executes it on the host GPU through [`HostVulkan`], writes the reply into
//! the reply window the guest named, and only then lets `head` move past it.
//!
//! # The shape
//!
//! * [`ExecutorFactory`] is the [`SinkFactory`] a [`VenusRenderer`] is built
//!   with. It keeps one [`VulkanContext`] per venus context — the object
//!   table and the host objects behind it — shared by that context's rings
//!   behind a lock, and hands every ring an [`ExecutingSink`].
//! * [`ExecutingSink`] is the per-ring loop: decode with
//!   [`Command::decode_next`], handle the two transport commands a ring may
//!   carry (`vkSetReplyCommandStreamMESA`, `vkSeekReplyCommandStreamMESA`),
//!   execute, encode the reply when the header asks for one, write it, and
//!   report those bytes consumed.
//! * [`context`] is what each command does, [`objects`] the id rules,
//!   [`policy`] what the guest is told, [`host`] the trait the host sits
//!   behind.
//!
//! # What makes a ring fatal
//!
//! Anything the sink cannot answer — bytes that do not decode, a command
//! with no generated decoder, one this stage does not implement, an id or a
//! value the rules refuse, a reply with no window or one that does not fit —
//! ends the ring with [`Batch::fatal_after`] **without consuming that
//! command**, logged with its opcode name, and marks the whole context fatal
//! as virglrenderer's per-context `cs_fatal_error` does: every ring of it
//! refuses from then on. `head` never passes a command whose reply was not
//! written, because the guest reads a moved `head` as "your reply is ready".
//!
//! A command the batch ends inside of waits for the next pass, as the
//! capture sinks do.
//!
//! # Replies
//!
//! A reply window is a range of a `HOST3D` blob of the same context
//! ([`ContextBlobs::bind`]); every write is bounded by the window, which is
//! stricter than virglrenderer (it bounds by the end of the resource) and
//! enough, because Mesa sizes each window exactly. The bytes go in through
//! [`RingPages::write_bytes`](super::shmem::RingPages::write_bytes) under the
//! blob directory's lock, so a blob destroyed while it is the window is never
//! written afterwards; the next reply that needs it is fatal, as in vkr. The
//! pump's `SeqCst` store of `head`, after the sink returns, is what orders
//! the reply bytes before the guest sees them.
//!
//! # What this stage does not do
//!
//! * `vkExecuteCommandStreamsMESA` (a command too large for the ring, copied
//!   to a separate blob) is refused as unimplemented. Mesa only sends it for
//!   a command over 8 KiB, and nothing in the bring-up is (spec §1.1).
//! * Fence timelines. `vkGetDeviceQueue2` records the `ring_idx` the guest
//!   binds each queue to, and that is all. The capset still says
//!   `supports_multiple_timelines = false`, and **release Mesa binds queues
//!   to timelines 1–63 regardless** (spec §6): the stage that implements
//!   `vkQueueSubmit` must give `virtio_gpu::fence` one FIFO per `ring_idx`
//!   (the `ring_idx` recorded here) before it can retire a guest fence, and
//!   only then flip the capset bit.
//! * Memory. `vkAllocateMemory` is not generated; the memory policy
//!   ([`policy::guest_memory`]) already describes the memory the next stage
//!   will back with imported host pages.

pub mod context;
pub mod host;
pub mod objects;
pub mod policy;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
pub(crate) mod harness;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;

pub use context::{ExecError, VulkanContext};
pub use host::{HostDeviceInfo, HostVulkan};
pub use policy::GuestDevice;

use super::protocol::{command_type_name, Command, ProtocolError};
use super::pump::{Batch, Consumed, RingSink};
#[cfg(doc)]
use super::renderer::VenusRenderer;
use super::renderer::{BlobRef, ContextBlobs, ReplyBlobError, RingEnv, SinkFactory};
use super::transport::{Opcode, TransportCommand, TransportError, TransportStream};
use super::wire::{Decoder, WireError, COMMAND_HEADER_BYTES};

/// Why a ring's sink gave up. Every variant is fatal to the ring and to its
/// context.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SinkError {
    /// A transport command in the ring that did not decode.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A Vulkan command that did not decode, or has no generated decoder.
    #[error("{command} (opcode {opcode}): {error}")]
    Decode {
        /// The opcode at the front of the command.
        opcode: u32,
        /// Its name, when it is a Venus command at all.
        command: &'static str,
        /// Why.
        error: ProtocolError,
    },
    /// The executor refused the command.
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// A transport command a ring may not carry, or one this stage does not
    /// implement.
    #[error("{0} is not served on a ring by this renderer")]
    NotOnRing(&'static str),
    /// A transport command that asked for a reply of its own.
    #[error("{0} asked for a reply, and it has none")]
    TransportReply(&'static str),
    /// A command that asked for a reply with no reply window bound — never
    /// bound, or its blob since destroyed (`vkr_cs_encoder_acquire`).
    #[error("{0} asked for a reply and no reply window is bound")]
    NoReplyWindow(&'static str),
    /// `vkSetReplyCommandStreamMESA` naming a window this context may not use.
    #[error(transparent)]
    ReplyWindow(#[from] ReplyBlobError),
    /// `vkSeekReplyCommandStreamMESA` outside the window, or with none bound.
    #[error("a reply-stream seek to {position:#x} is outside the {size:#x}-byte window")]
    SeekOutsideWindow {
        /// Where the guest asked for.
        position: u64,
        /// The window's size (0 with none bound).
        size: u64,
    },
    /// The reply did not fit what is left of the window.
    #[error("the reply to {command} does not fit the {room:#x} bytes left in its window")]
    ReplyTooLarge {
        /// The command.
        command: &'static str,
        /// What was left.
        room: u64,
    },
    /// The host could not encode a reply faithfully (a count the executor
    /// set that disagrees with its array — a host bug).
    #[error("the reply to {command} could not be encoded: {error}")]
    Encode {
        /// The command.
        command: &'static str,
        /// Why.
        error: ProtocolError,
    },
}

/// The reply window a ring's encoder points at (`vkr_cs_encoder`'s stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplyWindow {
    blob: BlobRef,
    offset: u64,
    size: u64,
    cursor: u64,
}

/// Lock a context, taking it back from a panicking holder rather than
/// panicking in turn: this is on a path a guest steers.
fn lock<H: HostVulkan>(context: &Mutex<VulkanContext<H>>) -> MutexGuard<'_, VulkanContext<H>> {
    context
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The executing sink of one ring. See the module docs.
pub struct ExecutingSink<H: HostVulkan> {
    ctx_id: u32,
    ring: u64,
    context: Arc<Mutex<VulkanContext<H>>>,
    blobs: ContextBlobs,
    window: Option<ReplyWindow>,
}

impl<H: HostVulkan> std::fmt::Debug for ExecutingSink<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutingSink")
            .field("ctx_id", &self.ctx_id)
            .field("ring", &format_args!("{:#x}", self.ring))
            .field("window", &self.window)
            .finish()
    }
}

/// What one step of the loop did.
enum Step {
    /// A whole command was handled; this many bytes are consumed.
    Done(usize),
    /// The batch ends inside a command: wait for more.
    Incomplete,
    /// The ring is dead: `opcode` could not be answered.
    Fatal { opcode: u32, error: SinkError },
}

impl<H: HostVulkan> ExecutingSink<H> {
    fn new(env: RingEnv, context: Arc<Mutex<VulkanContext<H>>>) -> Self {
        Self {
            ctx_id: env.ctx_id,
            ring: env.ring,
            context,
            blobs: env.blobs,
            window: None,
        }
    }

    fn step(&mut self, rest: &[u8]) -> Step {
        let Some(opcode) = rest
            .get(..4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
        else {
            return Step::Incomplete;
        };
        if rest.len() < COMMAND_HEADER_BYTES {
            return Step::Incomplete;
        }
        let fatal = |error: SinkError| Step::Fatal { opcode, error };
        if lock(&self.context).is_fatal() {
            return fatal(ExecError::ContextFatal(self.ctx_id).into());
        }
        if Opcode::from_u32(opcode).is_some() {
            return match self.transport(rest) {
                Ok(Some(used)) => Step::Done(used),
                Ok(None) => Step::Incomplete,
                Err(error) => fatal(error),
            };
        }

        let mut dec = Decoder::new(rest);
        let (header, mut command) = match Command::decode_next(&mut dec) {
            Ok(decoded) => decoded,
            Err(ProtocolError::Wire(WireError::Truncated { .. })) => return Step::Incomplete,
            Err(error) => {
                return fatal(SinkError::Decode {
                    opcode,
                    command: command_type_name(opcode).unwrap_or("an unknown command"),
                    error,
                })
            }
        };
        let used = dec.position();
        let name = command.name();
        // A reply with nowhere to go is refused before anything runs, so a
        // refused command has no side effect on the host either.
        if header.wants_reply() && self.window.is_none() {
            return fatal(SinkError::NoReplyWindow(name));
        }
        if let Err(error) = lock(&self.context).execute(&mut command) {
            return fatal(error.into());
        }
        if header.wants_reply() {
            if let Err(error) = self.reply(&command) {
                return fatal(error);
            }
        }
        Step::Done(used)
    }

    /// A transport command found in the ring. `Ok(None)`: incomplete.
    fn transport(&mut self, rest: &[u8]) -> Result<Option<usize>, SinkError> {
        let mut stream = TransportStream::new(rest);
        let request = match stream.next_command() {
            None | Some(Err(TransportError::Wire(WireError::Truncated { .. }))) => return Ok(None),
            Some(Err(error)) => return Err(error.into()),
            Some(Ok(request)) => request,
        };
        let name = request.command.opcode().name();
        match request.command {
            TransportCommand::SetReplyCommandStream { stream: desc } => {
                if request.header.wants_reply() {
                    return Err(SinkError::TransportReply(name));
                }
                let blob = self.blobs.bind(desc.resource_id, desc.offset, desc.size)?;
                self.window = Some(ReplyWindow {
                    blob,
                    offset: desc.offset,
                    size: desc.size,
                    cursor: 0,
                });
            }
            TransportCommand::SeekReplyCommandStream { position } => {
                if request.header.wants_reply() {
                    return Err(SinkError::TransportReply(name));
                }
                match self.window.as_mut() {
                    Some(window) if position <= window.size => window.cursor = position,
                    other => {
                        return Err(SinkError::SeekOutsideWindow {
                            position,
                            size: other.map_or(0, |w| w.size),
                        })
                    }
                }
            }
            // The context-stream commands are refused on a ring as vkr
            // refuses them (`is_dispatched_from_vkr_context`), and the rest
            // are not this stage's.
            _ => return Err(SinkError::NotOnRing(name)),
        }
        Ok(Some(stream.position()))
    }

    /// Encode `command`'s reply into the window and advance the cursor.
    fn reply(&mut self, command: &Command<'_>) -> Result<(), SinkError> {
        let name = command.name();
        let window = self.window.as_mut().ok_or(SinkError::NoReplyWindow(name))?;
        let room = window.size.saturating_sub(window.cursor);
        let bytes = command
            .reply_bytes(usize::try_from(room).unwrap_or(usize::MAX))
            .map_err(|error| match error {
                ProtocolError::Wire(WireError::ReplyTooLong { .. }) => SinkError::ReplyTooLarge {
                    command: name,
                    room,
                },
                error => SinkError::Encode {
                    command: name,
                    error,
                },
            })?;
        let at = window.offset.saturating_add(window.cursor);
        match self.blobs.write(window.blob, at, &bytes) {
            Ok(()) => {
                window.cursor = window.cursor.saturating_add(bytes.len() as u64);
                Ok(())
            }
            Err(ReplyBlobError::Gone(_)) => {
                // vkr unbinds a stream whose resource went away; the reply
                // that needed it is then fatal. Same outcome, same order.
                self.window = None;
                Err(SinkError::NoReplyWindow(name))
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl<H: HostVulkan> RingSink for ExecutingSink<H> {
    fn consume(&mut self, batch: Batch<'_>) -> Consumed {
        let bytes = batch.bytes();
        let mut done = 0usize;
        loop {
            let rest = match bytes.get(done..) {
                Some(rest) if !rest.is_empty() => rest,
                _ => return batch.consumed(done),
            };
            match self.step(rest) {
                Step::Done(used) => done = done.saturating_add(used.max(1)),
                Step::Incomplete => return batch.consumed(done),
                Step::Fatal { opcode, error } => {
                    lock(&self.context).set_fatal();
                    tracing::warn!(
                        ctx_id = self.ctx_id,
                        ring = format_args!("{:#x}", self.ring),
                        opcode,
                        command = command_type_name(opcode).unwrap_or("an unknown command"),
                        at = done,
                        %error,
                        "a Venus command could not be answered; the ring and its context are fatal"
                    );
                    return batch.fatal_after(done);
                }
            }
        }
    }
}

/// The [`SinkFactory`] of the executing renderer. See the module docs.
pub struct ExecutorFactory<H: HostVulkan> {
    host: Arc<H>,
    contexts: HashMap<u32, Arc<Mutex<VulkanContext<H>>>>,
}

impl<H: HostVulkan> std::fmt::Debug for ExecutorFactory<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorFactory")
            .field("contexts", &self.contexts.len())
            .field("host_objects", &self.host_objects())
            .finish()
    }
}

impl<H: HostVulkan> ExecutorFactory<H> {
    /// An executor over `host`.
    pub fn new(host: Arc<H>) -> Self {
        Self {
            host,
            contexts: HashMap::new(),
        }
    }

    /// The host this executor drives.
    #[must_use]
    pub fn host(&self) -> &Arc<H> {
        &self.host
    }

    /// Guest-visible objects across every context (physical devices
    /// included) — every one of them a host object or a binding to one.
    #[must_use]
    pub fn host_objects(&self) -> usize {
        self.contexts
            .values()
            .map(|context| lock(context).object_count())
            .sum()
    }

    /// Whether context `ctx_id` went fatal.
    #[must_use]
    pub fn context_fatal(&self, ctx_id: u32) -> Option<bool> {
        self.contexts
            .get(&ctx_id)
            .map(|context| lock(context).is_fatal())
    }

    fn context(&mut self, ctx_id: u32) -> Arc<Mutex<VulkanContext<H>>> {
        let host = &self.host;
        Arc::clone(
            self.contexts.entry(ctx_id).or_insert_with(|| {
                Arc::new(Mutex::new(VulkanContext::new(ctx_id, Arc::clone(host))))
            }),
        )
    }
}

impl<H: HostVulkan> SinkFactory for ExecutorFactory<H> {
    type Sink = ExecutingSink<H>;

    fn sink_for(&mut self, ctx_id: u32, ring: u64) -> io::Result<Self::Sink> {
        Err(io::Error::other(format!(
            "ring {ring:#x} of venus context {ctx_id}: an executing sink needs its context's blobs"
        )))
    }

    fn sink_for_ring(&mut self, env: RingEnv) -> io::Result<Self::Sink> {
        let context = self.context(env.ctx_id);
        Ok(ExecutingSink::new(env, context))
    }

    fn context_created(&mut self, ctx_id: u32) {
        let _ = self.context(ctx_id);
    }

    fn context_destroyed(&mut self, ctx_id: u32) {
        if let Some(context) = self.contexts.remove(&ctx_id) {
            lock(&context).destroy_all();
        }
    }

    fn reset(&mut self) {
        for (_, context) in self.contexts.drain() {
            lock(&context).destroy_all();
        }
    }

    fn snapshot_refusal(&self) -> Option<String> {
        let objects = self.host_objects();
        (objects > 0).then(|| {
            format!(
                "the Venus executor holds {objects} host Vulkan objects a guest driver believes \
                 in, and host GPU objects cannot be written to a snapshot"
            )
        })
    }
}

/// What [`probe`] found: the devices a guest would be shown.
///
/// # Errors
/// A sentence saying why no device would be: the loader, an instance that
/// would not come up, or every device hidden (each with its reason).
pub fn probe<H: HostVulkan>(host: &H) -> Result<Vec<GuestDevice>, String> {
    let version = host
        .instance_version()
        .map_err(|r| format!("vkEnumerateInstanceVersion failed ({r})"))?;
    if version < policy::MIN_API_VERSION {
        return Err("the host Vulkan loader is older than 1.1".into());
    }
    let instance = host
        .create_instance(&host::InstanceRequest {
            application_name: Some("entangled venus probe".into()),
            ..Default::default()
        })
        .map_err(|r| format!("vkCreateInstance failed ({r})"))?;
    let result = (|| {
        let handles = host
            .enumerate_physical_devices(&instance)
            .map_err(|r| format!("vkEnumeratePhysicalDevices failed ({r})"))?;
        let mut shown = Vec::new();
        let mut hidden = Vec::new();
        for handle in handles {
            let info = host.describe_physical_device(&instance, handle);
            let name =
                String::from_utf8_lossy(policy::c_name(&info.properties.properties.device_name))
                    .into_owned();
            match policy::expose(info) {
                Ok(device) => shown.push(device),
                Err(why) => hidden.push(format!("{name}: {why}")),
            }
        }
        if shown.is_empty() {
            let why = if hidden.is_empty() {
                "the host has no Vulkan device at all".to_owned()
            } else {
                format!("every host Vulkan device is hidden ({})", hidden.join("; "))
            };
            return Err(why);
        }
        Ok(shown)
    })();
    host.destroy_instance(instance);
    result
}
