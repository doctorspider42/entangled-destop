//! The boot framebuffer as the BIOS's VBE display (ADR-0012).
//!
//! A BIOS-era guest has no driver for the machine's linear framebuffer
//! (`crate::framebuffer`), but it knows how to ask a video BIOS for one:
//! VESA's `int 10h, AH=4Fh`. The BIOS (`pc_bios`) answers those calls, and
//! this is what it drives — the same Bochs-DISPI device a UEFI firmware's
//! GOP driver programs, set here by the host on the guest's behalf.

use std::sync::Arc;

use crate::virtio_pci::VirtioPciBus;

/// The framebuffer function on `bus`, as a VBE display.
pub struct FramebufferVbe {
    bus: Arc<VirtioPciBus>,
}

impl FramebufferVbe {
    /// The framebuffer on `bus`. With none there, every call refuses.
    pub fn new(bus: Arc<VirtioPciBus>) -> Self {
        Self { bus }
    }
}

impl pc_bios::Vbe for FramebufferVbe {
    fn memory_bytes(&self) -> u64 {
        self.bus
            .framebuffer()
            .map_or(0, |fb| fb.device.vram_bytes())
    }

    fn framebuffer(&mut self) -> Option<u32> {
        // A base above 4 GiB cannot be named to a VBE caller.
        self.bus
            .framebuffer_vram_base()
            .and_then(|base| u32::try_from(base).ok())
    }

    fn set_mode(&mut self, width: u16, height: u16, clear: bool) -> bool {
        // The VRAM must be decoded before the guest is told to draw in it.
        if self.framebuffer().is_none() {
            return false;
        }
        match self.bus.framebuffer() {
            Some(fb) => {
                fb.device.set_mode(width, height, 32, clear);
                fb.device.scanout().is_some()
            }
            None => false,
        }
    }

    fn disable(&mut self) {
        if let Some(fb) = self.bus.framebuffer() {
            fb.device.disable();
        }
    }
}
