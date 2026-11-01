//! An 8042 keyboard controller with a PS/2 keyboard and no mouse
//! (ADR-0008 §5), at the legacy ports `0x60`/`0x64` on IRQ 1.
//!
//! It exists for the screens that run before any virtio driver: Windows'
//! recovery environment (WinRE has no `vioinput`, so without this a guest
//! that boots into it takes no keyboard at all) and anything else that only
//! knows a PS/2 keyboard. Once the guest drives the virtio keyboard the host
//! sends its keys there instead (`apps/entangled`), so nothing is typed twice.
//!
//! The keyboard speaks scancode set 1 — what the controller hands the guest
//! with translation on, which is how every PC OS runs it; a guest that asks
//! for another set is told it has it, and still gets set 1. Bytes the guest
//! cannot see yet wait in a bounded buffer, and a full buffer drops keys, as a
//! real keyboard's does. Controller command `0xfe` (pulse the reset line) is
//! answered by [`I8042::io_write`] returning `true`: the caller owns the
//! machine reset (`crate::reset`).

use std::collections::VecDeque;
use std::sync::Arc;

use virtio_core::interrupt::IrqLine;

/// The data port: keyboard bytes in, commands' parameters and keyboard
/// commands out.
pub const PORT_DATA: u16 = 0x60;
/// The status (read) and command (write) port.
pub const PORT_COMMAND: u16 = 0x64;
/// The keyboard's ISA interrupt.
pub const KEYBOARD_IRQ: u32 = 1;

/// Bytes waiting for the guest, at most. A real keyboard holds 16 keys.
const MAX_QUEUED: usize = 64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_SYSTEM: u8 = 1 << 2;
const STATUS_LAST_WAS_COMMAND: u8 = 1 << 3;
const STATUS_NOT_INHIBITED: u8 = 1 << 4;

const CONFIG_KEYBOARD_IRQ: u8 = 1 << 0;
const CONFIG_SYSTEM: u8 = 1 << 2;
const CONFIG_KEYBOARD_DISABLED: u8 = 1 << 4;
const CONFIG_AUX_DISABLED: u8 = 1 << 5;
const CONFIG_TRANSLATE: u8 = 1 << 6;
/// Interrupts on, translation on, no mouse.
const CONFIG_POWER_ON: u8 =
    CONFIG_KEYBOARD_IRQ | CONFIG_SYSTEM | CONFIG_AUX_DISABLED | CONFIG_TRANSLATE;

/// The keyboard's acknowledgement.
const ACK: u8 = 0xfa;

/// Version of [`I8042::save`]'s bytes.
const STATE_VERSION: u8 = 1;

/// The controller and its keyboard.
pub struct I8042 {
    interrupt: Arc<dyn IrqLine>,
    queue: VecDeque<u8>,
    /// What a read of an empty data port returns: the last byte read.
    last: u8,
    config: u8,
    /// Whether the last write went to the command port.
    last_was_command: bool,
    /// A controller command waiting for its data byte.
    controller_pending: Option<u8>,
    /// A keyboard command waiting for its parameter.
    keyboard_pending: Option<u8>,
    /// The keyboard sends keys (`0xf4`/`0xf5`).
    scanning: bool,
}

impl std::fmt::Debug for I8042 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("I8042")
            .field("queued", &self.queue.len())
            .field("config", &format_args!("{:#04x}", self.config))
            .field("scanning", &self.scanning)
            .finish_non_exhaustive()
    }
}

impl I8042 {
    /// A controller at power-on, raising `interrupt` for every byte it makes
    /// readable.
    pub fn new(interrupt: Arc<dyn IrqLine>) -> Self {
        Self {
            interrupt,
            queue: VecDeque::new(),
            last: 0,
            config: CONFIG_POWER_ON,
            last_was_command: false,
            controller_pending: None,
            keyboard_pending: None,
            scanning: true,
        }
    }

    /// True for the two ports this controller decodes.
    pub fn contains(port: u16) -> bool {
        port == PORT_DATA || port == PORT_COMMAND
    }

    /// Machine reset (ADR-0005): power-on registers, nothing queued.
    pub fn reset(&mut self) {
        let interrupt = Arc::clone(&self.interrupt);
        *self = Self::new(interrupt);
    }

    /// Whether the guest would see a key now: the keyboard is scanning and
    /// the controller has not disabled it.
    pub fn accepts_keys(&self) -> bool {
        self.scanning && self.config & CONFIG_KEYBOARD_DISABLED == 0
    }

    /// A key, by its Linux input code (`KEY_*`, what virtio-input carries),
    /// pressed or released. Keys with no set-1 code are dropped.
    pub fn key(&mut self, code: u16, pressed: bool) {
        if !self.accepts_keys() {
            return;
        }
        let Some((extended, make)) = set1(code) else {
            return;
        };
        let code = if pressed { make } else { make | 0x80 };
        let bytes: &[u8] = if extended { &[0xe0, code] } else { &[code] };
        if self.queue.len() + bytes.len() > MAX_QUEUED {
            return;
        }
        self.queue_bytes(bytes);
    }

    /// A guest read.
    pub fn io_read(&mut self, port: u16) -> u8 {
        if port == PORT_COMMAND {
            let mut status = STATUS_SYSTEM | STATUS_NOT_INHIBITED;
            if !self.queue.is_empty() {
                status |= STATUS_OUTPUT_FULL;
            }
            if self.last_was_command {
                status |= STATUS_LAST_WAS_COMMAND;
            }
            return status;
        }
        if let Some(byte) = self.queue.pop_front() {
            self.last = byte;
            if !self.queue.is_empty() {
                self.raise();
            }
        }
        self.last
    }

    /// A guest write. `true`: the guest pulsed the reset line, and the
    /// machine must reset.
    pub fn io_write(&mut self, port: u16, value: u8) -> bool {
        if port == PORT_COMMAND {
            self.last_was_command = true;
            return self.controller_command(value);
        }
        self.last_was_command = false;
        match self.controller_pending.take() {
            Some(0x60) => {
                self.config = value;
                false
            }
            // Write output port: bit 0 low holds the CPU in reset.
            Some(0xd1) => value & 1 == 0,
            // Write the keyboard's output buffer: as if the keyboard sent it.
            Some(0xd2) => {
                self.queue_bytes(&[value]);
                false
            }
            // Bytes for a mouse that is not there.
            Some(_) => false,
            None => {
                self.keyboard_command(value);
                false
            }
        }
    }

    fn controller_command(&mut self, command: u8) -> bool {
        match command {
            0x20 => self.queue_bytes(&[self.config]),
            0x60 | 0xd1 | 0xd2 | 0xd3 | 0xd4 => self.controller_pending = Some(command),
            0xa7 => self.config |= CONFIG_AUX_DISABLED,
            0xa8 => self.config &= !CONFIG_AUX_DISABLED,
            // The mouse port's clock line is stuck: there is no mouse.
            0xa9 => self.queue_bytes(&[0x01]),
            0xaa => {
                self.config = CONFIG_POWER_ON;
                self.queue_bytes(&[0x55]);
            }
            0xab => self.queue_bytes(&[0x00]),
            0xad => self.config |= CONFIG_KEYBOARD_DISABLED,
            0xae => self.config &= !CONFIG_KEYBOARD_DISABLED,
            // Output port: reset line high, A20 on.
            0xd0 => self.queue_bytes(&[0x03]),
            // Pulse output lines: bit 0 low is the reset line.
            0xf0..=0xff => return command & 1 == 0,
            _ => {}
        }
        false
    }

    fn keyboard_command(&mut self, value: u8) {
        if let Some(command) = self.keyboard_pending.take() {
            if command == 0xf0 && value == 0 {
                let set = if self.config & CONFIG_TRANSLATE != 0 {
                    0x41
                } else {
                    0x02
                };
                self.queue_bytes(&[ACK, set]);
            } else {
                self.queue_bytes(&[ACK]);
            }
            return;
        }
        match value {
            // LEDs, scancode set, typematic rate: a parameter follows.
            0xed | 0xf0 | 0xf3 => {
                self.keyboard_pending = Some(value);
                self.queue_bytes(&[ACK]);
            }
            0xee => self.queue_bytes(&[0xee]),
            0xf2 => {
                let id = if self.config & CONFIG_TRANSLATE != 0 {
                    0x41
                } else {
                    0x83
                };
                self.queue_bytes(&[ACK, 0xab, id]);
            }
            0xf4 => {
                self.scanning = true;
                self.queue_bytes(&[ACK]);
            }
            0xf5 => {
                self.scanning = false;
                self.queue_bytes(&[ACK]);
            }
            0xf6..=0xfd => self.queue_bytes(&[ACK]),
            0xff => {
                self.scanning = true;
                self.queue_bytes(&[ACK, 0xaa]);
            }
            _ => self.queue_bytes(&[0xfe]),
        }
    }

    fn queue_bytes(&mut self, bytes: &[u8]) {
        let was_empty = self.queue.is_empty();
        for &byte in bytes {
            if self.queue.len() < MAX_QUEUED {
                self.queue.push_back(byte);
            }
        }
        if was_empty && !self.queue.is_empty() {
            self.raise();
        }
    }

    fn raise(&self) {
        if self.config & CONFIG_KEYBOARD_IRQ != 0 {
            if let Err(error) = self.interrupt.trigger() {
                tracing::warn!(%error, "the keyboard interrupt could not be raised");
            }
        }
    }

    /// The controller's state, for a snapshot (ADR-0006).
    pub fn save(&self) -> Vec<u8> {
        let flags = u8::from(self.scanning) | (u8::from(self.last_was_command) << 1);
        let mut out = vec![
            STATE_VERSION,
            self.config,
            self.last,
            flags,
            self.controller_pending.unwrap_or(0),
            self.keyboard_pending.unwrap_or(0),
            u8::try_from(self.queue.len()).unwrap_or(0),
        ];
        out.extend(self.queue.iter().copied());
        out
    }

    /// Adopts [`save`](Self::save)'s bytes.
    ///
    /// # Errors
    /// Bytes this version did not write.
    pub fn load(&mut self, bytes: &[u8]) -> Result<(), String> {
        let [version, config, last, flags, controller, keyboard, len, rest @ ..] = bytes else {
            return Err("the keyboard controller's state is truncated".into());
        };
        if *version != STATE_VERSION || rest.len() != usize::from(*len) || rest.len() > MAX_QUEUED {
            return Err("the keyboard controller's state is not this version's".into());
        }
        self.config = *config;
        self.last = *last;
        self.scanning = flags & 1 != 0;
        self.last_was_command = flags & 2 != 0;
        self.controller_pending = (*controller != 0).then_some(*controller);
        self.keyboard_pending = (*keyboard != 0).then_some(*keyboard);
        self.queue = rest.iter().copied().collect();
        Ok(())
    }
}

/// Scancode set 1 for a Linux key code: whether it takes the `0xe0` prefix,
/// and its make code. Codes 1–88 are set 1's own numbering.
fn set1(code: u16) -> Option<(bool, u8)> {
    let extended = match code {
        1..=88 => return u8::try_from(code).ok().map(|make| (false, make)),
        96 => 0x1c,  // KP Enter
        97 => 0x1d,  // Right Ctrl
        98 => 0x35,  // KP /
        99 => 0x37,  // SysRq / Print Screen
        100 => 0x38, // Right Alt
        102 => 0x47, // Home
        103 => 0x48, // Up
        104 => 0x49, // Page Up
        105 => 0x4b, // Left
        106 => 0x4d, // Right
        107 => 0x4f, // End
        108 => 0x50, // Down
        109 => 0x51, // Page Down
        110 => 0x52, // Insert
        111 => 0x53, // Delete
        125 => 0x5b, // Left Meta
        126 => 0x5c, // Right Meta
        127 => 0x5d, // Menu
        _ => return None,
    };
    Some((true, extended))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Count(AtomicUsize);
    impl IrqLine for Count {
        fn trigger(&self) -> Result<(), virtio_core::interrupt::InterruptError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn drain(kbc: &mut I8042) -> Vec<u8> {
        let mut out = Vec::new();
        while kbc.io_read(PORT_COMMAND) & STATUS_OUTPUT_FULL != 0 {
            out.push(kbc.io_read(PORT_DATA));
        }
        out
    }

    #[test]
    fn a_key_is_set_1_with_an_interrupt_per_readable_byte() {
        let irq = Arc::new(Count::default());
        let mut kbc = I8042::new(irq.clone());
        kbc.key(30, true); // A
        kbc.key(30, false);
        kbc.key(103, true); // Up
        assert_eq!(drain(&mut kbc), [0x1e, 0x9e, 0xe0, 0x48]);
        assert_eq!(irq.0.load(Ordering::SeqCst), 4);
        kbc.key(0x2ff, true);
        assert!(drain(&mut kbc).is_empty(), "no set-1 code, no byte");
    }

    #[test]
    fn the_controller_and_keyboard_answer_what_windows_and_linux_ask() {
        let mut kbc = I8042::new(Arc::new(Count::default()));
        kbc.io_write(PORT_COMMAND, 0xaa);
        assert_eq!(drain(&mut kbc), [0x55]);
        kbc.io_write(PORT_COMMAND, 0x20);
        assert_eq!(drain(&mut kbc), [CONFIG_POWER_ON]);
        kbc.io_write(PORT_DATA, 0xff);
        assert_eq!(drain(&mut kbc), [ACK, 0xaa]);
        kbc.io_write(PORT_DATA, 0xf2);
        assert_eq!(drain(&mut kbc), [ACK, 0xab, 0x41]);
        kbc.io_write(PORT_DATA, 0xed);
        kbc.io_write(PORT_DATA, 0x02);
        assert_eq!(drain(&mut kbc), [ACK, ACK]);
        kbc.io_write(PORT_COMMAND, 0xad);
        kbc.key(30, true);
        assert!(
            drain(&mut kbc).is_empty(),
            "a disabled keyboard sends nothing"
        );
        assert!(
            kbc.io_write(PORT_COMMAND, 0xfe),
            "0xfe pulses the reset line"
        );
        assert!(!kbc.io_write(PORT_COMMAND, 0xff));
    }

    #[test]
    fn the_buffer_is_bounded_and_the_state_round_trips() {
        let mut kbc = I8042::new(Arc::new(Count::default()));
        for _ in 0..100 {
            kbc.key(30, true);
        }
        let saved = kbc.save();
        let mut other = I8042::new(Arc::new(Count::default()));
        other.load(&saved).unwrap();
        assert_eq!(drain(&mut other).len(), MAX_QUEUED);
        assert!(other.load(&saved[..3]).is_err());
        kbc.reset();
        assert!(drain(&mut kbc).is_empty());
    }
}
