//! Typed failures of a single virtio-gpu control command.
//!
//! Every variant here is *guest-caused* and answered **in band**: the device
//! writes a `VIRTIO_GPU_RESP_ERR_*` header into the chain's device-writable
//! part and carries on serving the queue. Nothing in this file is a host error
//! — a malformed GPU command must never take the device (let alone the VMM)
//! down, which is the EPIC 8 acceptance criterion "VM cannot force copies
//! outside its memory".

use thiserror::Error;

use crate::protocol::{resp, Rect};

/// Why one control command was rejected, and which response code the guest
/// gets for it.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error("command {kind:#06x} is truncated: {len} bytes, {expected} required")]
    Truncated {
        kind: u32,
        len: usize,
        expected: usize,
    },

    #[error("resource id 0 is reserved and never valid")]
    ZeroResourceId,

    #[error("no such resource: {0}")]
    UnknownResource(u32),

    #[error("resource {0} already exists")]
    DuplicateResource(u32),

    #[error(
        "pixel format {0} is not supported; the MVP implements \
         VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM only"
    )]
    UnsupportedFormat(u32),

    #[error("resource geometry {width}x{height} is zero-sized or beyond the host limit")]
    BadGeometry { width: u32, height: u32 },

    #[error("host resource memory is exhausted")]
    OutOfMemory,

    #[error("rect {rect:?} does not fit inside the {width}x{height} resource")]
    RectOutOfBounds { rect: Rect, width: u32, height: u32 },

    #[error("resource {0} has no backing pages attached")]
    NoBacking(u32),

    #[error("backing store is too short: {need} bytes needed, {have} attached")]
    ShortBacking { need: u64, have: u64 },

    #[error("backing entry count {0} exceeds the per-resource limit")]
    TooManyEntries(u32),

    #[error("command of {0} bytes exceeds the largest command this device accepts")]
    RequestTooLarge(u64),

    #[error("attach_backing with zero entries")]
    NoEntries,

    #[error("guest memory at {addr:#x} is not readable: {reason}")]
    Unreadable { addr: u64, reason: String },

    #[error("no such scanout: {0}")]
    UnknownScanout(u32),

    #[error("the display rejected the update: {0}")]
    Display(String),

    #[error("command {0:#06x} is not implemented")]
    UnsupportedCommand(u32),
}

impl CommandError {
    /// The `VIRTIO_GPU_RESP_ERR_*` code sent back to the guest.
    pub fn resp_code(&self) -> u32 {
        match self {
            // Bad geometry, formats, offsets and backing sizes are all
            // parameter problems as far as the driver is concerned.
            Self::Truncated { .. }
            | Self::UnsupportedFormat(_)
            | Self::BadGeometry { .. }
            | Self::RectOutOfBounds { .. }
            | Self::NoBacking(_)
            | Self::ShortBacking { .. }
            | Self::TooManyEntries(_)
            | Self::RequestTooLarge(_)
            | Self::NoEntries
            | Self::Unreadable { .. }
            | Self::Display(_) => resp::ERR_INVALID_PARAMETER,

            Self::ZeroResourceId | Self::UnknownResource(_) | Self::DuplicateResource(_) => {
                resp::ERR_INVALID_RESOURCE_ID
            }

            Self::UnknownScanout(_) => resp::ERR_INVALID_SCANOUT_ID,
            Self::OutOfMemory => resp::ERR_OUT_OF_MEMORY,
            Self::UnsupportedCommand(_) => resp::ERR_UNSPEC,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_codes_follow_the_spec_categories() {
        assert_eq!(
            CommandError::ZeroResourceId.resp_code(),
            resp::ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            CommandError::UnknownResource(9).resp_code(),
            resp::ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            CommandError::UnknownScanout(3).resp_code(),
            resp::ERR_INVALID_SCANOUT_ID
        );
        assert_eq!(
            CommandError::OutOfMemory.resp_code(),
            resp::ERR_OUT_OF_MEMORY
        );
        assert_eq!(
            CommandError::UnsupportedCommand(0x0200).resp_code(),
            resp::ERR_UNSPEC
        );
        assert_eq!(
            CommandError::UnsupportedFormat(67).resp_code(),
            resp::ERR_INVALID_PARAMETER
        );
        assert_eq!(
            CommandError::ShortBacking { need: 10, have: 2 }.resp_code(),
            resp::ERR_INVALID_PARAMETER
        );
    }

    #[test]
    fn messages_name_the_offending_value() {
        let err = CommandError::UnsupportedFormat(67);
        assert!(err.to_string().contains("67"));
        let err = CommandError::Truncated {
            kind: 0x0105,
            len: 8,
            expected: 56,
        };
        let text = err.to_string();
        assert!(text.contains("0x0105") && text.contains("56"), "{text}");
    }
}
