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
//! * [`context`] is what each command does, [`memory`] the memory, buffer,
//!   binding and view commands (stage 5b.1), [`device_objects`] pipelines,
//!   descriptors, render passes, queries and every `vkCmd*`, [`submit`]
//!   submission, fences, semaphores and waits (stages 5b.2 and 5b.3),
//!   [`timeline`] the virtio-gpu fences of a queue's `ring_idx` (stage 5b.3),
//!   [`generated`] the translation all of stage 5b.2 goes through,
//!   [`objects`] the id rules, [`policy`] what the guest is told, [`host`]
//!   the trait the host sits behind.
//!
//! # Memory the guest maps
//!
//! A `HOST_VISIBLE` allocation is our own pages, imported into the driver
//! ([`memory`]'s module docs), and a `HOST3D` blob whose `blob_id` names it
//! is **the same pages** ([`ExecutorFactory`]'s
//! [`SinkFactory::export_memory`]). Their bytes are charged to one
//! renderer-wide budget, [`MAX_HOST_VISIBLE_BYTES`], until the last holder of
//! them — memory object, blob or mapping — is gone.
//!
//! # Memory the guest shares but never maps (stage S1)
//!
//! Device-local memory allocated for export is allocated exportable on the
//! host (`OPAQUE_WIN32`, where the host has `VK_KHR_external_memory_win32`),
//! and its blob is a **handle blob**: a host handle to the allocation, no
//! pages, refused if mapped. Another context attached to the blob imports it
//! into its own device ([`memory`]'s module docs), and a DRM-modifier image
//! over it is the same canonical optimal image on both sides ([`modifier`]).
//! That is the dma-buf GNOME's compositor and its GL clients pass around.
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
//! # Command streams outside the ring (stage 5b.2)
//!
//! A submission larger than the ring's direct size (8 KiB on Mesa's primary
//! ring: a recorded command buffer, a big shader module) arrives as
//! `vkExecuteCommandStreamsMESA`, naming ranges of shared-memory blobs of the
//! same context (`vkr_transport.c`). Each range is bounded by its blob,
//! **copied** out under the blob directory's lock ([`ContextBlobs::read`]) —
//! vkr decodes in place, and the guest may still be writing — and executed
//! command by command through exactly the path a ring command takes, replies
//! included. A stream must hold whole commands; one that carries another
//! `vkExecuteCommandStreamsMESA` is refused, as in vkr; the bytes one call may
//! name are bounded by [`MAX_STREAM_BYTES`]. Mesa 26.0.8 passes no reply
//! positions and no dependencies; both are served anyway — a position seeks
//! the reply stream before its stream, and a dependency must point forward,
//! which executing the streams in order satisfies.
//!
//! # Waits
//!
//! `vkWaitForFences`, `vkWaitSemaphores`, `vkQueueWaitIdle` and
//! `vkDeviceWaitIdle` are waited for
//! on this thread, in slices of [`submit::WAIT_SLICE`] with the context lock
//! released in between ([`submit`]'s module docs). The context's ring monitor
//! keeps `ALIVE` set meanwhile; a ring being torn down stops waiting within a
//! slice, without consuming the command.
//!
//! # A lost device
//!
//! A `VK_ERROR_DEVICE_LOST` from the driver is answered to the guest — the
//! command that met it is replied to and consumed — and then the context is
//! fatal, as if the next command had been refused. Nothing on the host goes
//! down with it: the device's objects are destroyed as ever when the context
//! goes, which Vulkan allows on a lost device.
//!
//! # Fence timelines (stage 5b.3)
//!
//! `vkGetDeviceQueue2` binds each queue to the `ring_idx` the guest names,
//! and a virtio-gpu fence on that timeline
//! ([`SinkFactory::create_ring_fence`]) becomes a host fence on that queue,
//! retired by the queue's fence thread ([`timeline`]); the device keeps one
//! FIFO per `(context, ring_idx)` (`crate::fence`), so the capset says
//! `supports_multiple_timelines` — which Mesa only asserts, and binds queues
//! to timelines 1–63 regardless (spec §6).
//!
//! # Scanout of a handle blob (stage S2b)
//!
//! A guest compositor's frames are handle blobs; the factory reads one back
//! through a device of the renderer's own ([`scanout`]), made the first time
//! one is scanned out, on the GPU the blob was exported from. It is not any
//! context's: no guest teardown reaches it.

pub mod context;
pub mod device_objects;
pub mod generated;
pub mod host;
pub mod memory;
pub mod modifier;
pub mod objects;
pub mod policy;
pub mod scanout;
pub mod submit;
pub mod timeline;

#[cfg(test)]
mod ext_tests;
#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod generated_tests;
#[cfg(test)]
pub(crate) mod harness;
#[cfg(test)]
mod memory_tests;
#[cfg(test)]
mod query_tests;
#[cfg(test)]
pub(crate) mod recording;
#[cfg(test)]
mod s1_tests;
#[cfg(test)]
mod s2b_tests;
#[cfg(test)]
mod submit_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use thiserror::Error;

pub use context::{ExecError, VulkanContext};
pub use host::{HostDeviceInfo, HostVulkan};
pub use policy::GuestDevice;

use super::protocol::{command_type_name, Command, ProtocolError};
use super::pump::{Batch, Consumed, RingSink};
#[cfg(doc)]
use super::renderer::VenusRenderer;
use super::renderer::{
    BlobRef, ContextBlobs, ExportedMemory, ReplyBlobError, RingEnv, ScanoutRelease, ScanoutTarget,
    SinkFactory,
};
use super::service::StopSignal;
use super::shmem::PageBudget;
#[cfg(doc)]
use super::shmem::RingPages;
use super::transport::{
    CommandStreamDependency, CommandStreamDescription, Opcode, TransportCommand, TransportError,
    TransportStream,
};
use super::wire::{Decoder, WireError, COMMAND_HEADER_BYTES};

/// The most bytes one `vkExecuteCommandStreamsMESA` may have the executor
/// copy and run: 64 MiB, eight times Mesa's whole command-stream pool
/// (`vn_instance.c:328-329`) and far past any recorded command buffer or
/// shader module a guest sends.
pub const MAX_STREAM_BYTES: u64 = 64 << 20;

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
    /// `vkExecuteCommandStreamsMESA` inside a stream it runs.
    #[error("vkExecuteCommandStreamsMESA inside a command stream (nested execution)")]
    NestedStreams,
    /// `vkExecuteCommandStreamsMESA` naming no stream (`vkr_transport.c`).
    #[error("vkExecuteCommandStreamsMESA names no stream")]
    NoStreams,
    /// More bytes than [`MAX_STREAM_BYTES`].
    #[error(
        "vkExecuteCommandStreamsMESA names {total:#x} bytes, past the {MAX_STREAM_BYTES:#x} it may"
    )]
    StreamsTooLarge {
        /// What it named.
        total: u64,
    },
    /// A stream range this context may not read.
    #[error("command stream {index}: {error}")]
    Stream {
        /// Which stream.
        index: usize,
        /// Why.
        error: ReplyBlobError,
    },
    /// A dependency that is not an edge from an earlier stream to a later
    /// one of the same call.
    #[error("a stream dependency {src} -> {dst} among {streams} streams")]
    Dependency {
        /// `srcCommandStream`.
        src: u32,
        /// `dstCommandStream`.
        dst: u32,
        /// How many streams the call names.
        streams: usize,
    },
    /// A stream that ends inside a command.
    #[error("command stream {index} ends inside a command")]
    TruncatedStream {
        /// Which stream.
        index: usize,
    },
    /// A command inside a stream that could not be answered.
    #[error("command stream {index}, {command} (opcode {opcode}) at {at:#x}: {error}")]
    InStream {
        /// Which stream.
        index: usize,
        /// The command's opcode.
        opcode: u32,
        /// Its name.
        command: &'static str,
        /// Where in the stream.
        at: usize,
        /// Why.
        error: Box<SinkError>,
    },
}

/// The renderer-wide cap on host pages behind `HOST_VISIBLE` guest memory:
/// 1 GiB.
///
/// Every byte of it is host RAM the guest chose the size of, pinned by the
/// host driver while imported, so it is bounded like every other guest-sized
/// allocation — per allocation by the whole cap and across allocations by
/// what is left of it. 1 GiB is four times the shared-memory window a guest
/// can map at once ([`super::renderer::VENUS_HOST_VISIBLE_BYTES`], 256 MiB),
/// which leaves room for staging memory that is allocated but not mapped,
/// and is a loss a 16 GiB host can afford to a hostile guest. Past it an
/// allocation answers `VK_ERROR_OUT_OF_DEVICE_MEMORY`: to the guest the
/// host-visible heap is full. Device-local memory is not charged; the
/// driver's own heap bounds it.
pub const MAX_HOST_VISIBLE_BYTES: u64 = 1 << 30;

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
    stop: StopSignal,
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
    /// A whole command was handled and answered, and the device is lost:
    /// consume it, then end the ring and the context.
    DoneThenFatal(usize),
    /// The batch ends inside a command: wait for more.
    Incomplete,
    /// The ring is being torn down in the middle of a wait: consume nothing
    /// more.
    Stopped,
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
            stop: StopSignal::never(),
        }
    }

    /// One command at the front of `rest`. `nested`: `rest` is a stream a
    /// `vkExecuteCommandStreamsMESA` named, not the ring.
    fn step(&mut self, rest: &[u8], nested: bool) -> Step {
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
            return self.transport(rest, nested);
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
        let executed = if submit::is_wait(&command) {
            self.wait(&mut command)
        } else {
            let mut context = lock(&self.context);
            context.stop = Some(self.stop.clone());
            context.execute(&mut command).map(|()| true)
        };
        match executed {
            Err(error) => return fatal(error.into()),
            Ok(false) => return Step::Stopped,
            Ok(true) => {}
        }
        if header.wants_reply() {
            if let Err(error) = self.reply(&command) {
                return fatal(error);
            }
        }
        if lock(&self.context).take_lost() {
            return Step::DoneThenFatal(used);
        }
        Step::Done(used)
    }

    /// A wait, one slice at a time with the context lock released between
    /// slices ([`submit`]). `Ok(false)`: the ring is being torn down.
    fn wait(&mut self, command: &mut Command<'_>) -> Result<bool, ExecError> {
        let limit = submit::wait_limit(command);
        let start = Instant::now();
        loop {
            let slice = limit.map_or(submit::WAIT_SLICE, |limit| {
                limit
                    .saturating_sub(start.elapsed())
                    .min(submit::WAIT_SLICE)
            });
            let done = {
                let mut context = lock(&self.context);
                context.stop = Some(self.stop.clone());
                context.execute_wait(command, slice)?
            };
            if done {
                return Ok(true);
            }
            if limit.is_some_and(|limit| start.elapsed() >= limit) {
                submit::time_out(command);
                return Ok(true);
            }
            if self.stop.is_stopping() {
                return Ok(false);
            }
        }
    }

    /// A transport command at the front of `rest`.
    fn transport(&mut self, rest: &[u8], nested: bool) -> Step {
        let mut stream = TransportStream::new(rest);
        let request = match stream.next_command() {
            None | Some(Err(TransportError::Wire(WireError::Truncated { .. }))) => {
                return Step::Incomplete
            }
            Some(Err(error)) => {
                return Step::Fatal {
                    opcode: rest
                        .get(..4)
                        .and_then(|b| <[u8; 4]>::try_from(b).ok())
                        .map_or(0, u32::from_le_bytes),
                    error: error.into(),
                }
            }
            Some(Ok(request)) => request,
        };
        let opcode = request.command.opcode().as_u32();
        let fatal = |error: SinkError| Step::Fatal { opcode, error };
        let name = request.command.opcode().name();
        if request.header.wants_reply() {
            return fatal(SinkError::TransportReply(name));
        }
        match request.command {
            TransportCommand::SetReplyCommandStream { stream: desc } => {
                match self.blobs.bind(desc.resource_id, desc.offset, desc.size) {
                    Ok(blob) => {
                        self.window = Some(ReplyWindow {
                            blob,
                            offset: desc.offset,
                            size: desc.size,
                            cursor: 0,
                        });
                    }
                    Err(error) => return fatal(error.into()),
                }
            }
            TransportCommand::SeekReplyCommandStream { position } => {
                if let Err(error) = self.seek(position) {
                    return fatal(error);
                }
            }
            TransportCommand::ExecuteCommandStreams {
                streams,
                reply_positions,
                dependencies,
                flags: _,
            } => {
                if nested {
                    return fatal(SinkError::NestedStreams);
                }
                return match self.execute_streams(
                    &streams,
                    reply_positions.as_deref(),
                    &dependencies,
                ) {
                    Ok(StepOf::Done(())) => Step::Done(stream.position()),
                    Ok(StepOf::DoneThenFatal(())) => Step::DoneThenFatal(stream.position()),
                    Ok(StepOf::Stopped) => Step::Stopped,
                    Err(error) => fatal(error),
                };
            }
            // The context-stream commands are refused on a ring as vkr
            // refuses them (`is_dispatched_from_vkr_context`), and the rest
            // are not this stage's.
            _ => return fatal(SinkError::NotOnRing(name)),
        }
        Step::Done(stream.position())
    }

    /// `vkSeekReplyCommandStreamMESA`, or a stream's reply position.
    fn seek(&mut self, position: u64) -> Result<(), SinkError> {
        match self.window.as_mut() {
            Some(window) if position <= window.size => {
                window.cursor = position;
                Ok(())
            }
            other => Err(SinkError::SeekOutsideWindow {
                position,
                size: other.map_or(0, |w| w.size),
            }),
        }
    }

    /// `vkExecuteCommandStreamsMESA`: see the module docs.
    fn execute_streams(
        &mut self,
        streams: &[CommandStreamDescription],
        reply_positions: Option<&[u64]>,
        dependencies: &[CommandStreamDependency],
    ) -> Result<StreamStep, SinkError> {
        if streams.is_empty() {
            return Err(SinkError::NoStreams);
        }
        for d in dependencies {
            let n = streams.len();
            let fits = |i: u32| usize::try_from(i).is_ok_and(|i| i < n);
            if !fits(d.src_command_stream)
                || !fits(d.dst_command_stream)
                || d.src_command_stream >= d.dst_command_stream
            {
                return Err(SinkError::Dependency {
                    src: d.src_command_stream,
                    dst: d.dst_command_stream,
                    streams: n,
                });
            }
        }
        let total = streams
            .iter()
            .fold(0u64, |sum, s| sum.saturating_add(s.size));
        if total > MAX_STREAM_BYTES {
            return Err(SinkError::StreamsTooLarge { total });
        }
        let mut lost = false;
        for (index, desc) in streams.iter().enumerate() {
            if let Some(position) = reply_positions.and_then(|p| p.get(index)) {
                self.seek(*position)?;
            }
            if desc.size == 0 {
                continue;
            }
            let bytes = self
                .blobs
                .read(desc.resource_id, desc.offset, desc.size)
                .map_err(|error| SinkError::Stream { index, error })?;
            let mut at = 0usize;
            while at < bytes.len() {
                let rest = bytes.get(at..).unwrap_or_default();
                match self.step(rest, true) {
                    Step::Done(used) => at = at.saturating_add(used.max(1)),
                    Step::DoneThenFatal(used) => {
                        at = at.saturating_add(used.max(1));
                        lost = true;
                    }
                    Step::Stopped => return Ok(StepOf::Stopped),
                    Step::Incomplete => return Err(SinkError::TruncatedStream { index }),
                    Step::Fatal { opcode, error } => {
                        return Err(SinkError::InStream {
                            index,
                            opcode,
                            command: command_type_name(opcode)
                                .or_else(|| Opcode::from_u32(opcode).map(Opcode::name))
                                .unwrap_or("an unknown command"),
                            at,
                            error: Box::new(error),
                        })
                    }
                }
                if lost {
                    return Ok(StepOf::DoneThenFatal(()));
                }
            }
        }
        Ok(StepOf::Done(()))
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

    fn log_fatal(&self, opcode: u32, at: usize, error: &SinkError) {
        tracing::warn!(
            ctx_id = self.ctx_id,
            ring = format_args!("{:#x}", self.ring),
            opcode,
            command = command_type_name(opcode)
                .or_else(|| Opcode::from_u32(opcode).map(Opcode::name))
                .unwrap_or("an unknown command"),
            at,
            %error,
            "a Venus command could not be answered; the ring and its context are fatal"
        );
    }
}

/// [`Step`] for a whole `vkExecuteCommandStreamsMESA`, whose length the
/// caller knows.
type StreamStep = StepOf<()>;

/// A [`Step`] carrying `T` where it carries a length.
enum StepOf<T> {
    Done(T),
    DoneThenFatal(T),
    Stopped,
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
            match self.step(rest, false) {
                Step::Done(used) => done = done.saturating_add(used.max(1)),
                Step::Incomplete | Step::Stopped => return batch.consumed(done),
                Step::DoneThenFatal(used) => {
                    lock(&self.context).set_fatal();
                    tracing::warn!(
                        ctx_id = self.ctx_id,
                        ring = format_args!("{:#x}", self.ring),
                        "the host Vulkan device is lost; the ring and its context end after that reply"
                    );
                    return batch.fatal_after(done.saturating_add(used.max(1)));
                }
                Step::Fatal { opcode, error } => {
                    lock(&self.context).set_fatal();
                    self.log_fatal(opcode, done, &error);
                    return batch.fatal_after(done);
                }
            }
        }
    }

    fn attach_stop(&mut self, stop: StopSignal) {
        self.stop = stop;
    }
}

/// The [`SinkFactory`] of the executing renderer. See the module docs.
pub struct ExecutorFactory<H: HostVulkan> {
    host: Arc<H>,
    contexts: HashMap<u32, Arc<Mutex<VulkanContext<H>>>>,
    budget: Arc<PageBudget>,
    /// The renderer's own scanout device (stage S2b), once a handle blob
    /// has been scanned out.
    scanout: Option<scanout::ScanoutDevice<H>>,
}

impl<H: HostVulkan> std::fmt::Debug for ExecutorFactory<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorFactory")
            .field("contexts", &self.contexts.len())
            .field("host_objects", &self.host_objects())
            .field("host_visible_bytes", &self.budget.used())
            .finish()
    }
}

impl<H: HostVulkan> ExecutorFactory<H> {
    /// An executor over `host`.
    pub fn new(host: Arc<H>) -> Self {
        Self::with_budget(host, MAX_HOST_VISIBLE_BYTES)
    }

    /// An executor over `host` whose host-visible memory is capped at
    /// `limit` bytes instead of [`MAX_HOST_VISIBLE_BYTES`].
    pub fn with_budget(host: Arc<H>, limit: u64) -> Self {
        Self {
            host,
            contexts: HashMap::new(),
            budget: PageBudget::new(limit),
            scanout: None,
        }
    }

    /// The scanout device for a blob exported on the GPU `target` names,
    /// opened now if there is none or it is on another GPU (stage S2b).
    fn scanout_device(
        &mut self,
        target: &ScanoutTarget,
    ) -> Result<&mut scanout::ScanoutDevice<H>, String> {
        let uuids = target
            .handle
            .0
            .downcast_ref::<memory::HandleExport<H>>()
            .ok_or("a handle this host did not make")?
            .uuids;
        if self.scanout.as_ref().is_some_and(|d| d.uuids() != uuids) {
            self.scanout = None;
        }
        if self.scanout.is_none() {
            self.scanout = Some(scanout::ScanoutDevice::open(
                Arc::clone(&self.host),
                Arc::clone(&self.budget),
                uuids,
            )?);
        }
        self.scanout
            .as_mut()
            .ok_or_else(|| "no scanout device".to_owned())
    }

    /// The scanout device, for a test.
    #[cfg(test)]
    pub(crate) fn scanout(&self) -> Option<&scanout::ScanoutDevice<H>> {
        self.scanout.as_ref()
    }

    /// Bytes of host pages behind host-visible memory right now — every
    /// allocation whose last holder is still alive.
    #[must_use]
    pub fn host_visible_bytes(&self) -> u64 {
        self.budget.used()
    }

    /// Run `f` on context `ctx_id`, for a test that reaches past the ring.
    #[cfg(test)]
    pub(crate) fn with_context<T>(
        &self,
        ctx_id: u32,
        f: impl FnOnce(&VulkanContext<H>) -> T,
    ) -> Option<T> {
        self.contexts.get(&ctx_id).map(|context| {
            let guard = lock(context);
            f(&guard)
        })
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
        let budget = &self.budget;
        Arc::clone(self.contexts.entry(ctx_id).or_insert_with(|| {
            Arc::new(Mutex::new(VulkanContext::with_budget(
                ctx_id,
                Arc::clone(host),
                Arc::clone(budget),
            )))
        }))
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
        lock(&context)
            .blobs
            .get_or_insert_with(|| env.blobs.clone());
        Ok(ExecutingSink::new(env, context))
    }

    fn context_created(&mut self, ctx_id: u32) {
        let _ = self.context(ctx_id);
    }

    fn export_memory(
        &mut self,
        ctx_id: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<ExportedMemory, String> {
        // Only a context that exists: a blob of another context's memory
        // finds nothing, because each context has its own table.
        let context = self
            .contexts
            .get(&ctx_id)
            .ok_or_else(|| format!("venus context {ctx_id} holds no Vulkan memory"))?;
        lock(context).export_memory(blob_id, size)
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
        // Stage S2b: every import goes; the scanout device stays (it holds
        // nothing of any guest after this — `scanout`'s module docs).
        if let Some(device) = self.scanout.as_mut() {
            device.clear();
        }
    }

    fn prepare_scanout(&mut self, target: &ScanoutTarget) -> Result<(), String> {
        let device = self.scanout_device(target)?;
        match device.prepare(target) {
            Ok(()) => Ok(()),
            Err(scanout::ScanoutError::Lost(why)) => {
                self.scanout = None;
                Err(format!("the scanout device is lost ({why})"))
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn read_scanout(
        &mut self,
        target: &ScanoutTarget,
        release: ScanoutRelease,
        rect: crate::protocol::Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let device = self.scanout_device(target)?;
        match device.read(target, release, rect, out) {
            Ok(()) => Ok(()),
            Err(scanout::ScanoutError::Lost(why)) => {
                // Dropped now, made again by the next scanout.
                tracing::warn!(%why, "the venus scanout device is lost; it will be made again");
                self.scanout = None;
                Err(format!("the scanout device is lost ({why})"))
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn forget_scanout(&mut self, resource_id: u32) {
        if let Some(device) = self.scanout.as_mut() {
            device.forget(resource_id);
        }
    }

    fn scanout_targets(&self) -> usize {
        self.scanout
            .as_ref()
            .map_or(0, scanout::ScanoutDevice::target_count)
    }

    fn retires_ring_fences(&self) -> bool {
        true
    }

    fn create_ring_fence(
        &mut self,
        fence: super::renderer::RingFence,
        retire: &super::renderer::FenceRetirer,
    ) -> Result<crate::renderer::FenceOutcome, String> {
        let context = self
            .contexts
            .get(&fence.ctx_id)
            .ok_or_else(|| format!("venus context {} has no Vulkan", fence.ctx_id))?;
        lock(context).create_ring_fence(fence, retire)
    }

    fn pending_ring_fences(&self) -> usize {
        self.contexts
            .values()
            .map(|context| lock(context).objects.pending_ring_fences())
            .sum()
    }

    fn snapshot_refusal(&self) -> Option<String> {
        let objects = self.host_objects();
        let pages = self.budget.used();
        if objects == 0 && pages > 0 {
            // Memory freed while a blob of it lives keeps its pages, and the
            // guest may still be looking at them.
            return Some(format!(
                "the Venus executor still holds {pages:#x} bytes of host-visible Vulkan memory \
                 pages a guest can map, and they cannot be written to a snapshot"
            ));
        }
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
