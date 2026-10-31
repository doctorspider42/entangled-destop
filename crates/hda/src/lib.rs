//! An Intel High Definition Audio controller with one output codec
//! (ADR-0008: sound for Windows guests).
//!
//! virtio-win has no released virtio-snd driver, and Windows has had an
//! in-box HD Audio driver since Vista, so this is how a Windows guest gets
//! sound with nothing to install: the bus driver (`HDAudBus.sys`) binds the
//! controller, the class driver (`HDAudio.sys`) the codec, and the guest
//! sees a "Speakers" endpoint. Linux's `snd-hda-intel` binds it too.
//!
//! Portable device logic only: [`controller`] is the register file, the
//! command rings and the output engine, [`codec`] the codec. The PCI
//! function around it (identity, BAR, MSI) is the machine's, and the host
//! sink is virtio-sound's ([`virtio_sound::AudioSink`]), shared with the
//! virtio sound card.

pub mod codec;
pub mod controller;

pub use controller::{Hda, Raise, BAR_SIZE};

/// PCI identity: Intel ICH6 HD Audio, the controller every in-box driver
/// knows.
pub const VENDOR_ID: u16 = 0x8086;
/// See [`VENDOR_ID`].
pub const DEVICE_ID: u16 = 0x2668;
/// Class 04 (multimedia), subclass 03 (HD Audio).
pub const CLASS_HDA: u32 = 0x0403_0000;
/// The register BAR.
pub const BAR_INDEX: u8 = 0;
