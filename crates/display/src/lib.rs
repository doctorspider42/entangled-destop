//! Host presentation and input capture (backlog EPIC 7, host half of EPIC 9).
//!
//! One [`DisplayHost`] owns one `winit` window, one `wgpu` device and one
//! `Bgra8Unorm` texture holding the guest scanout. Device threads never touch
//! that state: they publish pixels through a [`DisplayHandle`] and consume
//! captured input from an [`InputQueue`].
//!
//! ```text
//!  virtio-gpu thread            main thread (event loop)
//!  ────────────────────         ─────────────────────────────────────────────
//!  update_scanout(rect) ──▶ Scanout mirror (BGRA + dirty rect)
//!                  wake ──▶ RedrawRequested ─▶ write_texture(dirty rect)
//!                                            ─▶ draw into letterbox(viewport)
//!  virtio-input thread
//!  ────────────────────
//!  InputQueue::drain()   ◀── winit key/pointer events (SYN_REPORT batches)
//!  ControlQueue::drain() ◀── Ctrl+Alt+G / Ctrl+Alt+Q (never sent to the guest)
//! ```
//!
//! # Layout
//!
//! - [`viewport`]: pure geometry — [`letterbox`], [`Viewport`], [`DisplayConfig`].
//! - [`ux`]: window UX policy — [`ScaleMode`], cursor visibility, title text,
//!   initial geometry (EPIC 15).
//! - [`scanout`]: the CPU-side BGRA mirror, dirty rects and PNG screenshots.
//! - `renderer` (private): the `wgpu` surface, scanout texture and pipeline.
//! - [`input`]: winit events → [`virtio_input::InputEvent`] batches, plus the
//!   grab state machine and reserved shortcuts.
//! - [`keymap`]: winit physical key → Linux `KEY_*` table (MVP-902).
//!
//! # Window UX (backlog EPIC 15)
//!
//! Host input reaches the guest only while the *input grab* is active, so the
//! window behaves like every other window on the desktop until the user asks
//! otherwise:
//!
//! | Action | Result |
//! |---|---|
//! | click inside the guest image | grab input; host cursor hidden over the image |
//! | `Ctrl+Alt` (nothing else pressed in between) | release the grab, cursor back |
//! | `Ctrl+Alt+G` | explicit grab toggle |
//! | `Ctrl+Alt+Q` | ask the VM to shut down |
//! | `F11` | borderless fullscreen toggle |
//! | `Ctrl+Alt+O` | 1:1 pixel mode toggle ([`ScaleMode`]) |
//! | focus loss | grab released, every held key released towards the guest |
//!
//! The reserved shortcuts above never reach the guest; every other combination
//! does while grabbed — including `Ctrl+Alt+F2` and friends. The window title
//! always states which of the two input states the window is in.
//!
//! # Manual verification
//!
//! Everything except the window itself is unit-tested headlessly; the windowed
//! path has to be looked at. Under WSL with WSLg (or any Linux desktop):
//!
//! ```bash
//! # animated 1920x1080 test pattern driven only through the public API
//! cargo run --release -p display --example demo
//!
//! # force a backend if the default pick misbehaves (llvmpipe/lavapipe on WSLg)
//! WGPU_BACKEND=vulkan cargo run --release -p display --example demo
//! WGPU_BACKEND=gl     cargo run --release -p display --example demo
//!
//! # see the per-second FPS / copy statistics (MVP-708)
//! RUST_LOG=display=debug cargo run --release -p display --example demo
//! ```
//!
//! In the demo window: resize it (the image stays 16:9 with black bars),
//! minimize and restore it (presenting stops and resumes), then click the image
//! to grab input — only then do the guest-side keys work: `S` for a PNG
//! screenshot, `R` to cycle the guest resolution. `Ctrl+Alt` releases the grab,
//! `Ctrl+Alt+G` toggles it, `F11` goes fullscreen, `Ctrl+Alt+O` switches to 1:1
//! and `Ctrl+Alt+Q` quits.

#![deny(missing_docs)]

mod error;
mod handle;
mod host;
pub mod input;
pub mod keymap;
mod renderer;
pub mod scanout;
mod sync;
pub mod ux;
pub mod viewport;

pub use error::DisplayError;
pub use handle::DisplayHandle;
pub use host::DisplayHost;
pub use input::{ControlEvent, ControlQueue, InputCapture, InputQueue, KeyOutcome, WindowAction};
pub use renderer::FrameStats;
pub use scanout::{Scanout, ScanoutStats, SharedScanout};
pub use ux::{viewport_for, ScaleMode, WindowStatus};
pub use viewport::{letterbox, DisplayConfig, Viewport};

/// Upper bound on one scanout, shared with `virtio-gpu`'s resource limit: a
/// guest cannot make the host allocate an absurd framebuffer.
pub const MAX_SCANOUT_PIXELS: u64 = virtio_gpu::MAX_RESOURCE_PIXELS;

/// The pixel format of the scanout texture, matching the only byte layout the
/// guest gets ([`virtio_gpu::FORMAT_B8G8R8A8_UNORM`] and its alpha-less twin
/// [`virtio_gpu::FORMAT_B8G8R8X8_UNORM`]) so pixels are copied verbatim.
pub const SCANOUT_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanout_format_matches_the_guest_format() {
        // Both accepted guest formats are 32-bit B, G, R, A/X in that byte
        // order, which is exactly `Bgra8Unorm`.
        assert!(virtio_gpu::is_supported_format(
            virtio_gpu::FORMAT_B8G8R8A8_UNORM
        ));
        assert!(virtio_gpu::is_supported_format(
            virtio_gpu::FORMAT_B8G8R8X8_UNORM
        ));
        assert_eq!(SCANOUT_TEXTURE_FORMAT, wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(
            SCANOUT_TEXTURE_FORMAT.block_copy_size(None),
            Some(virtio_gpu::BYTES_PER_PIXEL),
            "a guest pixel and a texel must be the same 4 bytes"
        );
    }
}
