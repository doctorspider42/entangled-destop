//! A PC BIOS whose services run in the host (ADR-0012).
//!
//! A guest that boots the way a PC did before UEFI — Windows XP's `NTLDR`,
//! a CD's El Torito loader — calls `int 10h`, `13h`, `15h`, `16h` and `1Ah`
//! and expects a BIOS behind them. This crate is that BIOS, in two halves:
//!
//! - [`rom`] is what the guest sees at `F000:0000`: a stub per interrupt
//!   vector and one routine that saves the registers on the guest's stack and
//!   writes the stack's address to two I/O ports. A few hundred bytes of
//!   machine code, assembled in Rust.
//! - [`Bios`] is what answers: on that port write it reads the saved
//!   registers out of guest memory, performs the service — a sector read is a
//!   file read into guest memory, a character is two bytes at `B800:0000` —
//!   and writes the registers back for the stub to pop.
//!
//! So there is no 16-bit firmware to build, the services are ordinary Rust
//! with ordinary tests, and nothing in real mode ever touches a device: on a
//! host whose hypervisor cannot emulate memory-mapped I/O from real mode (WHP,
//! see `vmm-core`'s `whp_smoke`), that is the difference between booting and
//! not.
//!
//! It takes **one interrupt**, the 8254's tick on IRQ 0, and handles it in
//! the ROM without calling the host: the tick is counted at `0040:006C` and
//! `int 1Ch` is called, because a loader hooks that to time itself (Windows
//! XP's does, before anything else). Every other 8259 line stays masked; the
//! keys come from whoever feeds [`Bios::scancode`].
//!
//! The guest is untrusted here as everywhere: every pointer, count and drive
//! number in a call is the guest's, every access to its memory is checked,
//! and a call that makes no sense fails with the BIOS's own error codes.

mod bios;
pub mod font;
pub mod media;
pub mod rom;

pub use bios::{
    read_text_screen, Bios, BiosError, MemoryRange, Paging, Vbe, VideoModes, TEXT_BASE,
};
pub use media::{Drive, DriveKind};
pub use rom::Status;
