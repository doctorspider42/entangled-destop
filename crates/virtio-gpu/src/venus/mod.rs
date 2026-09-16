//! Venus: the protocol a guest's Mesa Vulkan driver speaks to a host
//! ([ADR-0004](../../../../docs/adr/0004-virtio-gpu-3d.md), EPIC 20).
//!
//! Everything in here is **pure logic over bytes** — what the host advertises,
//! and what it makes of the structures a guest hands it. No Vulkan, no GPU, no
//! host renderer: those sit above this module, and keeping them out is what
//! lets the parts a malicious guest can reach be tested on every host,
//! including one with no graphics at all.
//!
//! The two pieces here are the two a guest touches first, in order:
//!
//! * [`capset`] — the 160 bytes the guest reads *before* it will speak to us at
//!   all. Its `wire_format_version` must match the guest's exactly or Mesa's
//!   ICD refuses to load, so this is the one structure where being wrong is
//!   silent.
//! * [`ring`] — the command ring's layout, which the guest describes and we
//!   validate. Five byte ranges inside one shared-memory resource, named by
//!   offsets the guest chose. It is the densest security surface in the
//!   protocol: everything the guest later sends arrives through a ring whose
//!   shape it proposed.

pub mod capset;
pub mod ring;
