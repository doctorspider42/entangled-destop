//! Typing text into the guest as keystrokes (2026-10-13): the window's "type
//! the clipboard" button and the control channel's `paste <text>`.
//!
//! Real clipboard sharing needs an agent in the guest, and the usual one
//! (spice-vdagent) has no clipboard under a Wayland session. Keystrokes need
//! nothing: they work at a login screen, in GRUB, on a text console and in any
//! application, which is where a password or a long command is wanted most.
//!
//! **Layout.** Key codes are physical keys, and the guest's keymap decides what
//! they mean. The map here is the US layout's, so text types exactly on a guest
//! set to `us` — and to every layout that keeps the US positions for ASCII
//! (Polish programmers', for one). Characters it has no key for (anything
//! outside printable ASCII, tab and newline) are skipped and counted, never
//! guessed.
//!
//! **Pacing.** Each character is two or four `SYN`-terminated batches (Shift
//! down, key down, key up, Shift up), queued here and handed out a few at a
//! time ([`Typist::take`]) by a thread that paces them — a burst of thousands
//! would overflow the device's pending-event bound and lose keys. One request
//! is capped at [`MAX_TYPED_CHARS`], and the queue as a whole at
//! [`MAX_QUEUED_BATCHES`].

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{ev, InputEvent};

/// Characters one request types; the rest is dropped and counted.
pub const MAX_TYPED_CHARS: usize = 4096;
/// Batches waiting in the queue, over every request.
pub const MAX_QUEUED_BATCHES: usize = 4 * MAX_TYPED_CHARS;

const KEY_LEFTSHIFT: u16 = 42;

/// The US-layout key for `c`, and whether it needs Shift.
pub fn key_for(c: char) -> Option<(u16, bool)> {
    const DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    Some(match c {
        'a'..='z' => (LETTERS[(c as u8 - b'a') as usize], false),
        'A'..='Z' => (LETTERS[(c as u8 - b'A') as usize], true),
        '0'..='9' => (DIGITS[(c as u8 - b'0') as usize], false),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        _ => return None,
    })
}

fn key_batch(code: u16, down: bool) -> Vec<InputEvent> {
    vec![
        InputEvent {
            event_type: ev::KEY,
            code,
            value: u32::from(down),
        },
        InputEvent::SYN_REPORT,
    ]
}

/// The batches that type `text`, and how many characters had no key. A
/// carriage return is dropped (a `\r\n` is one Enter).
pub fn keystrokes(text: &str) -> (Vec<Vec<InputEvent>>, usize) {
    let mut batches = Vec::new();
    let mut skipped = 0;
    for c in text.chars() {
        if c == '\r' {
            continue;
        }
        let Some((code, shift)) = key_for(c) else {
            skipped += 1;
            continue;
        };
        if shift {
            batches.push(key_batch(KEY_LEFTSHIFT, true));
        }
        batches.push(key_batch(code, true));
        batches.push(key_batch(code, false));
        if shift {
            batches.push(key_batch(KEY_LEFTSHIFT, false));
        }
    }
    (batches, skipped)
}

/// What one request typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TypeOutcome {
    /// Characters queued.
    pub typed: usize,
    /// Characters with no key on the US layout.
    pub skipped: usize,
    /// Characters past [`MAX_TYPED_CHARS`] or a full queue.
    pub truncated: usize,
}

/// The queue between whoever asks for text to be typed and the thread that
/// paces it into the keyboard. Cloning gives another handle to the same queue.
#[derive(Clone, Default)]
pub struct Typist {
    inner: Arc<(Mutex<VecDeque<Vec<InputEvent>>>, Condvar)>,
}

impl Typist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `text` to be typed.
    pub fn type_text(&self, text: &str) -> TypeOutcome {
        let total = text.chars().filter(|c| *c != '\r').count();
        let head: String = text
            .chars()
            .filter(|c| *c != '\r')
            .take(MAX_TYPED_CHARS)
            .collect();
        let (batches, skipped) = keystrokes(&head);
        let mut outcome = TypeOutcome {
            typed: 0,
            skipped,
            truncated: total.saturating_sub(MAX_TYPED_CHARS),
        };
        let Ok(mut queue) = self.inner.0.lock() else {
            return outcome;
        };
        let room = MAX_QUEUED_BATCHES.saturating_sub(queue.len());
        if batches.len() > room {
            // Never half a character: whole requests only past the bound.
            outcome.truncated = total.saturating_sub(skipped);
            return outcome;
        }
        outcome.typed = head.chars().count().saturating_sub(skipped);
        queue.extend(batches);
        self.inner.1.notify_all();
        outcome
    }

    /// Up to `batches` batches, flattened into one event stream.
    pub fn take(&self, batches: usize) -> Vec<InputEvent> {
        let Ok(mut queue) = self.inner.0.lock() else {
            return Vec::new();
        };
        let n = batches.min(queue.len());
        queue.drain(..n).flatten().collect()
    }

    /// Waits up to `timeout` for something to type; answers whether there is.
    pub fn wait(&self, timeout: Duration) -> bool {
        let Ok(queue) = self.inner.0.lock() else {
            return false;
        };
        if !queue.is_empty() {
            return true;
        }
        match self.inner.1.wait_timeout(queue, timeout) {
            Ok((queue, _)) => !queue.is_empty(),
            Err(_) => false,
        }
    }

    /// Batches still waiting.
    pub fn pending(&self) -> usize {
        self.inner.0.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Forgets everything waiting — a machine reset.
    pub fn clear(&self) {
        if let Ok(mut queue) = self.inner.0.lock() {
            queue.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<(u16, u32)> {
        keystrokes(text)
            .0
            .into_iter()
            .flatten()
            .filter(|e| e.event_type == ev::KEY)
            .map(|e| (e.code, e.value))
            .collect()
    }

    #[test]
    fn a_shifted_character_is_wrapped_in_shift() {
        assert_eq!(keys("a"), vec![(30, 1), (30, 0)]);
        assert_eq!(keys("A"), vec![(42, 1), (30, 1), (30, 0), (42, 0)]);
        assert_eq!(keys("!"), vec![(42, 1), (2, 1), (2, 0), (42, 0)]);
        assert_eq!(keys("\r\n"), vec![(28, 1), (28, 0)], "CRLF is one Enter");
    }

    #[test]
    fn every_printable_ascii_character_has_a_key() {
        for b in 0x20u8..0x7f {
            assert!(key_for(b as char).is_some(), "{:?}", b as char);
        }
        assert_eq!(key_for('ą'), None);
        assert_eq!(keystrokes("zażółć").1, 4);
    }

    #[test]
    fn every_batch_ends_with_a_syn_report() {
        for batch in keystrokes("Hello, World!\n").0 {
            assert_eq!(batch.last(), Some(&InputEvent::SYN_REPORT));
        }
    }

    #[test]
    fn requests_are_bounded_and_paced_out() {
        let typist = Typist::new();
        let outcome = typist.type_text(&"x".repeat(MAX_TYPED_CHARS + 10));
        assert_eq!(outcome.typed, MAX_TYPED_CHARS);
        assert_eq!(outcome.truncated, 10);
        assert_eq!(typist.pending(), 2 * MAX_TYPED_CHARS);
        assert_eq!(typist.take(3).len(), 6, "three batches of two events");
        // The queue's own bound refuses a whole request rather than half.
        for _ in 0..3 {
            typist.type_text(&"y".repeat(MAX_TYPED_CHARS));
        }
        let refused = typist.type_text(&"z".repeat(MAX_TYPED_CHARS));
        assert_eq!(refused.typed, 0);
        assert!(typist.pending() <= MAX_QUEUED_BATCHES);
        typist.clear();
        assert!(!typist.wait(Duration::from_millis(1)));
    }
}
