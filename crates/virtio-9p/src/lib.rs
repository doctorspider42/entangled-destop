//! `virtio-9p` — shared folders: a host directory served to the guest over
//! 9P2000.L on a virtio transport, which Linux mounts with
//! `mount -t 9p -o trans=virtio <tag> <dir>`.
//!
//! * [`wire`] — the protocol's encoding, bounds-checked.
//! * [`server`] — the file server: fids, the confinement rules that keep
//!   every access inside the shared directory, and each operation on
//!   portable `std::fs`.
//! * [`device`] — the virtio device: one queue, the mount tag in config space.
//!
//! Builds and tests on both hosts (ADR-0002); nothing here is OS-specific
//! except three small helpers in `server` (positional I/O, inode numbers and
//! mode bits).

pub mod device;
pub mod server;
pub mod wire;

pub use device::{valid_tag, NinePDevice, MAX_TAG_LEN};
pub use server::Server;
