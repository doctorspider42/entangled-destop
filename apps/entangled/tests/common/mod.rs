//! Shared by the integration tests that drive the real CLI against a real guest
//! (`ubuntu_install`, `cdrom_boot`, `guest_reboot`): reading what a serial
//! console wrote.
//!
//! It is one function because two of those tests learned the same lesson twice,
//! in the same afternoon, on the same host — see [`read_transcript`]. The third
//! reads the console from a pipe rather than a file and needs only
//! [`strip_ansi`], which is why each is `allow(dead_code)`: a `tests/<dir>/mod.rs`
//! is compiled separately into every test binary that mentions it.
#![allow(dead_code)]

use std::path::Path;

/// Reads a serial transcript, which is **neither UTF-8 nor plain text**.
///
/// Two facts about a guest console, both of which have cost this project a full
/// acceptance run:
///
/// 1. **It is a byte stream.** An installed Ubuntu sets up its console font by
///    writing every code point from 0x00 to 0xFF, so the log stops being valid
///    UTF-8 partway through. `read_to_string(..).unwrap_or_default()` turns that
///    into an *empty* transcript, and a marker poll over an empty transcript can
///    only ever time out — which it did, for a whole six-minute deadline, on a
///    login prompt that had been printed at 136 s of guest uptime.
/// 2. **It is coloured.** systemd writes its greeting as
///    `ESC[0;1;39mWelcome to ESC[0mESC[1mUbuntu 26.04 LTS`, so the obvious
///    `contains("Welcome to Ubuntu")` can never match. A marker split by an
///    escape sequence is not a marker.
///
/// So: read bytes, convert lossily (the markers are ASCII; a replacement
/// character in the surrounding noise costs nothing), and strip the escapes.
/// What comes back is what a human watching the console would have read.
pub fn read_transcript(path: &Path) -> String {
    std::fs::read(path)
        .map(|bytes| strip_ansi(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

/// Removes ANSI terminal control sequences.
///
/// Three shapes appear in these transcripts: CSI (`ESC [` … final byte in
/// `@`..`~`), string sequences (`ESC ]` / `ESC P` / `ESC _` / `ESC ^` …
/// terminated by BEL or `ESC \`), and bare two-byte escapes such as `ESC M`.
/// Good enough that ASCII markers survive intact, which is all it must be.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if c != '\u{7}' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') | Some('P') | Some('_') | Some('^') => {
                let mut escaped = false;
                for next in chars.by_ref() {
                    if next == '\u{7}' || (escaped && next == '\\') {
                        break;
                    }
                    escaped = next == '\u{1b}';
                }
            }
            // Anything else is a two-byte escape; both characters go.
            _ => {}
        }
    }
    out
}

#[test]
fn ansi_stripping_keeps_the_markers_these_tests_look_for() {
    // The greeting exactly as an installed Ubuntu writes it to ttyS0.
    let greeting =
        "\u{1b}[0;1;39mWelcome to \u{1b}[0m\u{1b}[1mUbuntu 26.04 LTS\u{1b}[0m\u{1b}[0;1;39m!\u{1b}[0m\r\n";
    assert!(strip_ansi(greeting).contains("Welcome to Ubuntu 26.04 LTS!"));

    // A status line, plus an OSC and a DCS string — neither may swallow the text
    // that follows it.
    let status =
        "[\u{1b}[0;32m  OK  \u{1b}[0m] Started \u{1b}[0;1;39mserial-getty@ttyS0.service\u{1b}[0m\n";
    assert!(strip_ansi(status).contains("Started serial-getty@ttyS0.service"));
    assert_eq!(
        strip_ansi("a\u{1b}]3008;user=root\u{7}b\u{1b}P+q6E\u{1b}\\c\u{1b}Md"),
        "abcd"
    );

    // Plain text, including a Windows-shaped EFI path, is untouched.
    assert_eq!(
        strip_ansi("FSOpen: Open '\\EFI\\ubuntu\\shimx64.efi' Success"),
        "FSOpen: Open '\\EFI\\ubuntu\\shimx64.efi' Success"
    );
}
