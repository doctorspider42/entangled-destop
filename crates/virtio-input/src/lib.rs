//! virtio-input devices (backlog EPIC 9): a keyboard and an absolute pointer
//! (tablet-style, so the guest cursor tracks the host window 1:1 without
//! pointer grabs).
//!
//! Current state: the Linux input event model the devices will emit. Device
//! queues and host keycode mapping (winit → evdev) land with the transport.

/// Linux input event types (`EV_*` from `linux/input-event-codes.h`).
pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const REL: u16 = 0x02;
    pub const ABS: u16 = 0x03;
}

/// Absolute axes for the pointer device.
pub mod abs {
    pub const X: u16 = 0x00;
    pub const Y: u16 = 0x01;
}

/// Mouse buttons (`BTN_*`).
pub mod btn {
    pub const LEFT: u16 = 0x110;
    pub const RIGHT: u16 = 0x111;
    pub const MIDDLE: u16 = 0x112;
}

/// Range advertised for ABS_X/ABS_Y; host window coordinates are rescaled
/// into this range so guest position matches the window exactly (MVP-903).
pub const ABS_AXIS_MAX: u32 = 32767;

/// One event as carried in the virtio-input event queue (matches
/// `struct virtio_input_event`: all fields little-endian on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: u32,
}

impl InputEvent {
    pub const SYN_REPORT: InputEvent = InputEvent {
        event_type: ev::SYN,
        code: 0,
        value: 0,
    };

    /// Scales a host window coordinate into the ABS axis range.
    pub fn abs_from_window(axis: u16, pos: f64, window_extent: f64) -> Self {
        let clamped = pos.clamp(0.0, window_extent);
        let value = if window_extent > 0.0 {
            ((clamped / window_extent) * f64::from(ABS_AXIS_MAX)).round() as u32
        } else {
            0
        };
        InputEvent {
            event_type: ev::ABS,
            code: axis,
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_scaling_endpoints_and_center() {
        let e = InputEvent::abs_from_window(abs::X, 0.0, 1920.0);
        assert_eq!(e.value, 0);
        let e = InputEvent::abs_from_window(abs::X, 1920.0, 1920.0);
        assert_eq!(e.value, ABS_AXIS_MAX);
        let e = InputEvent::abs_from_window(abs::Y, 960.0, 1920.0);
        assert_eq!(e.value, ABS_AXIS_MAX / 2 + 1); // 16384, rounding up from 16383.5
    }

    #[test]
    fn out_of_window_positions_clamp() {
        assert_eq!(InputEvent::abs_from_window(abs::X, -5.0, 1920.0).value, 0);
        assert_eq!(
            InputEvent::abs_from_window(abs::X, 5000.0, 1920.0).value,
            ABS_AXIS_MAX
        );
        assert_eq!(InputEvent::abs_from_window(abs::X, 10.0, 0.0).value, 0);
    }
}
