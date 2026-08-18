//! winit physical key → Linux `KEY_*` mapping (backlog MVP-902).
//!
//! The host forwards *scancodes*, not characters: layout interpretation belongs
//! to the guest (`setxkbmap` inside Debian decides what `KEY_Y` means). winit's
//! [`KeyCode`] is already a physical, US-layout-labelled position code, so the
//! mapping is a flat table from position to the `KEY_*` number in
//! `linux/input-event-codes.h`.
//!
//! The table is the single source of truth: the macro below expands it into both
//! the `O(1)` [`linux_keycode`] match used at runtime and the [`KEYMAP`] slice
//! used by the tests.

use winit::keyboard::KeyCode;

macro_rules! keymap {
    ($(($winit:ident, $linux:expr)),* $(,)?) => {
        /// Every physical key the host forwards, as
        /// (winit [`KeyCode`], Linux `KEY_*` code) pairs.
        pub const KEYMAP: &[(KeyCode, u16)] = &[$((KeyCode::$winit, $linux)),*];

        /// Maps a winit physical key to its Linux `KEY_*` code, or `None` for
        /// keys with no evdev equivalent (`Fn`, `Meta`, vendor media keys).
        pub fn linux_keycode(code: KeyCode) -> Option<u16> {
            match code {
                $(KeyCode::$winit => Some($linux),)*
                _ => None,
            }
        }
    };
}

keymap! {
    // Row 1: escape, digits, punctuation, backspace.
    (Escape, 1),
    (Digit1, 2),
    (Digit2, 3),
    (Digit3, 4),
    (Digit4, 5),
    (Digit5, 6),
    (Digit6, 7),
    (Digit7, 8),
    (Digit8, 9),
    (Digit9, 10),
    (Digit0, 11),
    (Minus, 12),
    (Equal, 13),
    (Backspace, 14),

    // Row 2.
    (Tab, 15),
    (KeyQ, 16),
    (KeyW, 17),
    (KeyE, 18),
    (KeyR, 19),
    (KeyT, 20),
    (KeyY, 21),
    (KeyU, 22),
    (KeyI, 23),
    (KeyO, 24),
    (KeyP, 25),
    (BracketLeft, 26),
    (BracketRight, 27),
    (Enter, 28),

    // Row 3.
    (ControlLeft, 29),
    (KeyA, 30),
    (KeyS, 31),
    (KeyD, 32),
    (KeyF, 33),
    (KeyG, 34),
    (KeyH, 35),
    (KeyJ, 36),
    (KeyK, 37),
    (KeyL, 38),
    (Semicolon, 39),
    (Quote, 40),
    (Backquote, 41),

    // Row 4.
    (ShiftLeft, 42),
    (Backslash, 43),
    (KeyZ, 44),
    (KeyX, 45),
    (KeyC, 46),
    (KeyV, 47),
    (KeyB, 48),
    (KeyN, 49),
    (KeyM, 50),
    (Comma, 51),
    (Period, 52),
    (Slash, 53),
    (ShiftRight, 54),

    // Row 5 and lock keys.
    (AltLeft, 56),
    (Space, 57),
    (CapsLock, 58),
    (NumLock, 69),
    (ScrollLock, 70),

    // Function keys. F13..F24 continue at KEY_F13 = 183.
    (F1, 59),
    (F2, 60),
    (F3, 61),
    (F4, 62),
    (F5, 63),
    (F6, 64),
    (F7, 65),
    (F8, 66),
    (F9, 67),
    (F10, 68),
    (F11, 87),
    (F12, 88),
    (F13, 183),
    (F14, 184),
    (F15, 185),
    (F16, 186),
    (F17, 187),
    (F18, 188),
    (F19, 189),
    (F20, 190),
    (F21, 191),
    (F22, 192),
    (F23, 193),
    (F24, 194),

    // Numpad.
    (NumpadMultiply, 55),
    (Numpad7, 71),
    (Numpad8, 72),
    (Numpad9, 73),
    (NumpadSubtract, 74),
    (Numpad4, 75),
    (Numpad5, 76),
    (Numpad6, 77),
    (NumpadAdd, 78),
    (Numpad1, 79),
    (Numpad2, 80),
    (Numpad3, 81),
    (Numpad0, 82),
    (NumpadDecimal, 83),
    (NumpadEnter, 96),
    (NumpadDivide, 98),
    (NumpadEqual, 117),
    (NumpadComma, 121),

    // Right-hand modifiers, navigation cluster and arrows.
    (ControlRight, 97),
    (PrintScreen, 99),   // KEY_SYSRQ
    (AltRight, 100),
    (Home, 102),
    (ArrowUp, 103),
    (PageUp, 104),
    (ArrowLeft, 105),
    (ArrowRight, 106),
    (End, 107),
    (ArrowDown, 108),
    (PageDown, 109),
    (Insert, 110),
    (Delete, 111),
    (Pause, 119),
    (SuperLeft, 125),    // KEY_LEFTMETA
    (SuperRight, 126),   // KEY_RIGHTMETA
    (ContextMenu, 127),  // KEY_COMPOSE

    // Keys that only exist on non-US physical layouts but have fixed positions.
    (IntlBackslash, 86), // KEY_102ND
    (IntlRo, 89),        // KEY_RO
    (IntlYen, 124),      // KEY_YEN
    (Convert, 92),       // KEY_HENKAN
    (NonConvert, 94),    // KEY_MUHENKAN
    (KanaMode, 93),      // KEY_KATAKANAHIRAGANA
    (Katakana, 90),
    (Hiragana, 91),
    (Lang1, 122),        // KEY_HANGEUL
    (Lang2, 123),        // KEY_HANJA

    // Editing and system keys some keyboards expose as discrete positions.
    (Again, 129),
    (Props, 130),
    (Undo, 131),
    (Copy, 133),
    (Open, 134),
    (Paste, 135),
    (Find, 136),
    (Cut, 137),
    (Help, 138),
    (Power, 116),
    (Sleep, 142),
    (WakeUp, 143),
    (Select, 353),       // KEY_SELECT

    // Media / browser keys, mapped to their standard evdev codes.
    (AudioVolumeMute, 113),
    (AudioVolumeDown, 114),
    (AudioVolumeUp, 115),
    (LaunchMail, 155),
    (BrowserFavorites, 156),
    (BrowserBack, 158),
    (BrowserForward, 159),
    (Eject, 161),
    (MediaTrackNext, 163),
    (MediaPlayPause, 164),
    (MediaTrackPrevious, 165),
    (MediaStop, 166),
    (BrowserHome, 172),
    (BrowserRefresh, 173),
    (BrowserStop, 128),  // KEY_STOP
    (BrowserSearch, 217),
    (MediaSelect, 226),  // KEY_MEDIA
    (LaunchApp1, 148),   // KEY_PROG1
    (LaunchApp2, 149),   // KEY_PROG2
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn spot_checks_match_input_event_codes_h() {
        assert_eq!(linux_keycode(KeyCode::Escape), Some(1));
        assert_eq!(linux_keycode(KeyCode::KeyA), Some(30));
        assert_eq!(linux_keycode(KeyCode::KeyZ), Some(44));
        assert_eq!(linux_keycode(KeyCode::Digit0), Some(11));
        assert_eq!(linux_keycode(KeyCode::Enter), Some(28));
        assert_eq!(linux_keycode(KeyCode::Space), Some(57));
        assert_eq!(linux_keycode(KeyCode::ControlLeft), Some(29));
        assert_eq!(linux_keycode(KeyCode::AltLeft), Some(56));
        assert_eq!(linux_keycode(KeyCode::ShiftRight), Some(54));
        assert_eq!(linux_keycode(KeyCode::F1), Some(59));
        assert_eq!(linux_keycode(KeyCode::F12), Some(88));
        assert_eq!(linux_keycode(KeyCode::ArrowUp), Some(103));
        assert_eq!(linux_keycode(KeyCode::Numpad0), Some(82));
        assert_eq!(linux_keycode(KeyCode::NumpadEnter), Some(96));
        assert_eq!(linux_keycode(KeyCode::Delete), Some(111));
    }

    #[test]
    fn table_and_lookup_agree() {
        for &(code, expected) in KEYMAP {
            assert_eq!(linux_keycode(code), Some(expected), "{code:?}");
        }
    }

    #[test]
    fn distinct_keys_map_to_distinct_codes() {
        let mut seen: HashSet<u16> = HashSet::new();
        for &(code, linux) in KEYMAP {
            assert!(linux > 0, "{code:?} maps to the reserved code 0");
            assert!(
                seen.insert(linux),
                "{code:?} reuses Linux keycode {linux}, which another key already claims"
            );
        }
    }

    #[test]
    fn the_full_us_layout_is_covered() {
        // Letters, digits, F1..F12, arrows, nav cluster, modifiers, numpad.
        let required = [
            KeyCode::KeyA,
            KeyCode::KeyB,
            KeyCode::KeyC,
            KeyCode::KeyD,
            KeyCode::KeyE,
            KeyCode::KeyF,
            KeyCode::KeyG,
            KeyCode::KeyH,
            KeyCode::KeyI,
            KeyCode::KeyJ,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyM,
            KeyCode::KeyN,
            KeyCode::KeyO,
            KeyCode::KeyP,
            KeyCode::KeyQ,
            KeyCode::KeyR,
            KeyCode::KeyS,
            KeyCode::KeyT,
            KeyCode::KeyU,
            KeyCode::KeyV,
            KeyCode::KeyW,
            KeyCode::KeyX,
            KeyCode::KeyY,
            KeyCode::KeyZ,
            KeyCode::Digit0,
            KeyCode::Digit1,
            KeyCode::Digit2,
            KeyCode::Digit3,
            KeyCode::Digit4,
            KeyCode::Digit5,
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
            KeyCode::Minus,
            KeyCode::Equal,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Backslash,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Backquote,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::Escape,
            KeyCode::Tab,
            KeyCode::CapsLock,
            KeyCode::ShiftLeft,
            KeyCode::ShiftRight,
            KeyCode::ControlLeft,
            KeyCode::ControlRight,
            KeyCode::AltLeft,
            KeyCode::AltRight,
            KeyCode::SuperLeft,
            KeyCode::SuperRight,
            KeyCode::Space,
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::F1,
            KeyCode::F2,
            KeyCode::F3,
            KeyCode::F4,
            KeyCode::F5,
            KeyCode::F6,
            KeyCode::F7,
            KeyCode::F8,
            KeyCode::F9,
            KeyCode::F10,
            KeyCode::F11,
            KeyCode::F12,
            KeyCode::ArrowUp,
            KeyCode::ArrowDown,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Insert,
            KeyCode::Delete,
            KeyCode::PrintScreen,
            KeyCode::ScrollLock,
            KeyCode::Pause,
            KeyCode::NumLock,
            KeyCode::Numpad0,
            KeyCode::Numpad1,
            KeyCode::Numpad2,
            KeyCode::Numpad3,
            KeyCode::Numpad4,
            KeyCode::Numpad5,
            KeyCode::Numpad6,
            KeyCode::Numpad7,
            KeyCode::Numpad8,
            KeyCode::Numpad9,
            KeyCode::NumpadAdd,
            KeyCode::NumpadSubtract,
            KeyCode::NumpadMultiply,
            KeyCode::NumpadDivide,
            KeyCode::NumpadDecimal,
            KeyCode::NumpadEnter,
        ];
        for code in required {
            assert!(
                linux_keycode(code).is_some(),
                "{code:?} is missing from the keymap"
            );
        }
        assert!(KEYMAP.len() > required.len());
    }

    #[test]
    fn keys_without_an_evdev_equivalent_are_dropped() {
        assert_eq!(linux_keycode(KeyCode::Fn), None);
        assert_eq!(linux_keycode(KeyCode::FnLock), None);
        assert_eq!(linux_keycode(KeyCode::Meta), None);
        assert_eq!(linux_keycode(KeyCode::Hyper), None);
    }
}
