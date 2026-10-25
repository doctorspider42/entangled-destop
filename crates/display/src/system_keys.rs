//! Only keys Windows would consume before winit receives them. Ordinary input
//! stays on winit's path, including its keyboard-layout and AltGr handling.

/// Normal WM_KEYDOWN/UP bits. Do not set the system-key context bit:
/// DefWindowProc must not perform Alt+F4/Alt+Space on the host a second time.
pub(crate) fn message_lparam(scan: u32, extended: bool, down: bool, repeat: bool) -> u32 {
    1 | ((scan & 0xff) << 16)
        | (u32::from(extended) << 24)
        | (u32::from(repeat || !down) << 30)
        | (u32::from(!down) << 31)
}

pub(crate) struct SystemKeys {
    captured: [bool; 256],
}

impl Default for SystemKeys {
    fn default() -> Self {
        Self {
            captured: [false; 256],
        }
    }
}

impl SystemKeys {
    /// `Some(repeat)` means swallow the host event and deliver it to the guest.
    /// Keep owning its release even if Alt/Ctrl/Win were released first.
    pub(crate) fn route(
        &mut self,
        active: bool,
        vk: u32,
        down: bool,
        alt: bool,
        ctrl: bool,
    ) -> Option<bool> {
        if !active {
            self.captured.fill(false);
            return None;
        }
        let index = usize::try_from(vk)
            .ok()
            .filter(|&i| i < self.captured.len())?;
        let held = self.captured[index];
        let windows = matches!(vk, 0x5b | 0x5c);
        let windows_held = self.captured[0x5b] || self.captured[0x5c];
        let reserved = windows
            || windows_held
            || (alt && matches!(vk, 0x09 | 0x1b | 0x20 | 0x73))
            || (ctrl && vk == 0x1b);
        if held || (down && reserved) {
            self.captured[index] = down;
            Some(down && held)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_messages_keep_scan_codes_repeat_and_extended_key_releases() {
        assert_eq!(message_lparam(0x0f, false, true, false), 0x000f_0001);
        assert_eq!(message_lparam(0x0f, false, true, true), 0x400f_0001);
        assert_eq!(message_lparam(0x0f, false, false, false), 0xc00f_0001);
        assert_eq!(message_lparam(0x5b, true, true, false), 0x015b_0001);
        assert_eq!(message_lparam(0x5b, true, false, false), 0xc15b_0001);
    }

    #[test]
    fn alt_tab_and_its_release_stay_in_the_guest_after_alt_is_released() {
        let mut keys = SystemKeys::default();
        assert_eq!(keys.route(true, 0xa4, true, false, false), None);
        assert_eq!(keys.route(true, 9, true, true, false), Some(false));
        assert_eq!(keys.route(true, 9, true, true, false), Some(true));
        assert_eq!(keys.route(true, 0xa4, false, false, false), None);
        assert_eq!(keys.route(true, 9, false, false, false), Some(false));
        assert_eq!(keys.route(true, 9, true, false, false), None);
    }

    #[test]
    fn windows_chords_do_not_open_host_menus_and_have_balanced_releases() {
        let mut keys = SystemKeys::default();
        for win in [0x5b, 0x5c] {
            assert_eq!(keys.route(true, win, true, false, false), Some(false));
            assert_eq!(keys.route(true, 0x45, true, false, false), Some(false));
            assert_eq!(keys.route(true, win, false, false, false), Some(false));
            assert_eq!(keys.route(true, 0x45, false, false, false), Some(false));
            assert_eq!(keys.route(true, 0x45, true, false, false), None);
        }
    }

    #[test]
    fn focus_loss_or_ungrab_immediately_returns_the_keyboard_to_the_host() {
        let mut keys = SystemKeys::default();
        assert_eq!(keys.route(true, 0x5b, true, false, false), Some(false));
        assert_eq!(keys.route(false, 0x45, true, false, false), None);
        assert_eq!(keys.route(false, 0x5b, false, false, false), None);
        assert_eq!(keys.route(true, 0x45, true, false, false), None);
    }

    #[test]
    fn only_system_shortcuts_are_intercepted_and_escape_chord_stays_on_winit() {
        let mut keys = SystemKeys::default();
        for vk in [0x09, 0x1b, 0x20, 0x73] {
            assert_eq!(keys.route(true, vk, true, true, false), Some(false));
            assert_eq!(keys.route(true, vk, false, false, false), Some(false));
        }
        assert_eq!(keys.route(true, 0x1b, true, false, true), Some(false));
        assert_eq!(keys.route(true, 0x1b, false, false, false), Some(false));
        for vk in [0x41, 0x09, 0x70, 0x7a, 0xa2, 0xa4, 0xa5] {
            assert_eq!(keys.route(true, vk, true, false, false), None);
            assert_eq!(keys.route(true, vk, false, false, false), None);
        }
        assert_eq!(keys.route(true, u32::MAX, true, true, true), None);
    }
}
