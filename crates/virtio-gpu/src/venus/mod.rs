//! Venus: the protocol a guest's Mesa Vulkan driver speaks to a host
//! ([ADR-0004](../../../../docs/adr/0004-virtio-gpu-3d.md), EPIC 20).
//!
//! Almost everything in here is **pure logic over bytes** — what the host
//! advertises, and what it makes of the structures a guest hands it. No
//! Vulkan, no GPU, no host renderer: those sit above this module, and keeping
//! them out is what lets the parts a malicious guest can reach be tested on
//! every host, including one with no graphics at all. [`shmem`] is the one
//! exception and says why.
//!
//! # The layers, in the order a guest meets them
//!
//! * [`capset`] — the 160 bytes the guest reads *before* it will speak to us
//!   at all. Its `wire_format_version` must match the guest's exactly or
//!   Mesa's ICD refuses to load, so this is the one structure where being
//!   wrong is silent.
//! * [`wire`] — the byte primitives every command is built from. Venus is
//!   host-native little-endian and 4-byte granular with no inter-field
//!   padding, so 64-bit fields routinely land at 4-mod-8 and must be read
//!   unaligned; and there is no length field anywhere, so a command this layer
//!   cannot decode is not a command that can be *skipped* — it is the end of
//!   the stream.
//! * [`transport`] — the ten commands that arrive on the *context* command
//!   stream rather than through the ring, and that set the ring up in the
//!   first place. The only ones a host must understand before a ring exists.
//! * [`ring`] — the command ring's layout, which the guest describes and we
//!   validate. Five byte ranges inside one shared-memory resource, named by
//!   offsets the guest chose. The densest security surface in the protocol:
//!   everything the guest later sends arrives through a ring whose shape it
//!   proposed.
//! * [`shmem`] — the host memory that ring actually lives in. The renderer
//!   allocates it and keeps the pointer; `ShmBacking::map_host` only puts it
//!   in front of the guest. So this is where the ring's atomics are, and the
//!   only module here with `unsafe` in it.
//! * [`pump`] — the head/tail protocol over a validated [`ring::RingLayout`]:
//!   what the host consumes, what it publishes, and the private shadow copy
//!   that makes decoding safe while a guest is still writing the ring.
//! * [`service`] — *when* the pump runs: one worker thread per ring that keeps
//!   polling for the guest's `idleTimeout` before it publishes `IDLE`, and one
//!   monitor thread per context that keeps setting `ALIVE` for the guest's
//!   watchdog. The only threads in this module family, and both take ADR-0005's
//!   `Quiesce` gate.
//!
//! * [`renderer`] — the [`crate::Renderer3d`] that ties the seven together: it
//!   advertises the capset, accepts a venus context, decodes the transport
//!   stream, allocates the ring's pages and starts their service. It executes
//!   no Vulkan; what it does with the bytes is a [`pump::RingSink`] per ring
//!   that someone else supplies.
//!
//! # What is deliberately *not* here
//!
//! The execution half: an object table, host Vulkan, and the per-command
//! decoders generated from `vk.xml`. Those need a GPU to be worth anything,
//! and everything above needs none — which is the whole reason the seam is
//! drawn here.

pub mod capset;
pub mod protocol;
pub mod pump;
pub mod renderer;
pub mod ring;
pub mod service;
pub mod shmem;
pub mod transport;
pub mod wire;
