//! The guest's resolution follows the window (2026-10-12).
//!
//! When the ordinary window changes size — a drag or maximise — the display
//! asks the guest for a desktop of the window's size in physical pixels: the
//! virtio-gpu device reports it as the preferred mode in `GET_DISPLAY_INFO`
//! and the EDID and raises `VIRTIO_GPU_EVENT_DISPLAY`
//! ([`virtio_gpu::ScanoutSink::set_display_listener`]). Linux re-reads the
//! display, and a compositor that honours the connector's preferred mode
//! (mutter does for virtual outputs) switches to it, so the image fills the
//! window at 1:1 instead of being scaled.
//!
//! A drag delivers dozens of sizes a second; the guest is asked once the size
//! has held for [`SETTLE`]. `[display] follow_window = false` keeps the
//! profile's resolution whatever the window does.
//! F11 only changes presentation: changing the guest's display mode underneath
//! a fullscreen game can leave its cursor and render coordinates out of sync.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use virtio_gpu::DisplayListener;

/// How long a window size has to hold before the guest is asked for it.
pub const SETTLE: Duration = Duration::from_millis(350);

/// Smallest resolution the guest is asked for: below it a desktop is unusable,
/// and the window cannot be that small anyway ([`crate::ux::MIN_WINDOW_WIDTH`]).
pub const MIN: (u32, u32) = (640, 480);

/// Largest: what one scanout resource may be ([`crate::MAX_SCANOUT_PIXELS`],
/// 4096×2304) — a 4K window (3840×2160) fits.
pub const MAX: (u32, u32) = (4096, 2304);

/// The resolution to ask the guest for, for a window of `width`×`height`
/// physical pixels: clamped to [`MIN`]..=[`MAX`] and rounded down to even, which
/// every mode-setting path accepts.
pub fn guest_size_for_window(width: u32, height: u32) -> (u32, u32) {
    let w = width.clamp(MIN.0, MAX.0) & !1;
    let h = height.clamp(MIN.1, MAX.1) & !1;
    (w, h)
}

#[derive(Default)]
struct State {
    preferred: Option<(u32, u32)>,
    listener: Option<DisplayListener>,
}

/// The resolution the window would like the guest to use, shared by the
/// device-facing handle (which reports it) and the event loop (which sets it).
#[derive(Clone, Default)]
pub struct ResolutionRequest(Arc<Mutex<State>>);

impl std::fmt::Debug for ResolutionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ResolutionRequest")
            .field(&self.preferred())
            .finish()
    }
}

impl ResolutionRequest {
    /// Fullscreen changes presentation, never the guest's display mode. Check
    /// again when the debounce expires, since F11 may follow a queued resize.
    pub(crate) fn follow_window(
        &self,
        enabled: bool,
        fullscreen: bool,
        width: u32,
        height: u32,
    ) -> bool {
        if !enabled || fullscreen || width == 0 || height == 0 {
            return false;
        }
        let (width, height) = guest_size_for_window(width, height);
        self.request(width, height)
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The requested resolution, if the window has asked for one.
    pub fn preferred(&self) -> Option<(u32, u32)> {
        self.lock().preferred
    }

    /// Who to tell when it changes (the virtio-gpu device).
    pub fn set_listener(&self, listener: DisplayListener) {
        self.lock().listener = Some(listener);
    }

    /// Asks for `width`×`height` (already a [`guest_size_for_window`]).
    /// Answers whether that changed anything; the listener is called, outside
    /// the lock, only when it did.
    pub fn request(&self, width: u32, height: u32) -> bool {
        let listener = {
            let mut state = self.lock();
            if state.preferred == Some((width, height)) {
                return false;
            }
            state.preferred = Some((width, height));
            state.listener.clone()
        };
        tracing::info!(
            width,
            height,
            "asking the guest for the window's resolution"
        );
        if let Some(listener) = listener {
            listener();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f11_and_a_pending_resize_do_not_change_a_games_display_mode() {
        let request = ResolutionRequest::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        request.set_listener(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(request.follow_window(true, false, 1920, 1017));
        assert!(!request.follow_window(true, true, 1920, 1080));
        assert!(!request.follow_window(true, true, 3840, 2160));
        assert_eq!(request.preferred(), Some((1920, 1016)));
        assert!(!request.follow_window(true, false, 1920, 1017));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(request.follow_window(true, false, 1600, 900));
        assert!(!request.follow_window(false, false, 1280, 720));
        assert!(!request.follow_window(true, false, 0, 0));
        assert_eq!(request.preferred(), Some((1600, 900)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn window_sizes_are_clamped_and_even() {
        assert_eq!(guest_size_for_window(1921, 1017), (1920, 1016));
        assert_eq!(guest_size_for_window(100, 100), MIN);
        assert_eq!(guest_size_for_window(20_000, 20_000), MAX);
        let (w, h) = MAX;
        assert!(u64::from(w) * u64::from(h) <= crate::MAX_SCANOUT_PIXELS);
    }

    #[test]
    fn a_request_tells_the_listener_once_per_change() {
        let request = ResolutionRequest::default();
        assert_eq!(request.preferred(), None);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        request.set_listener(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(request.request(1600, 900));
        assert!(
            !request.request(1600, 900),
            "the same size again is no change"
        );
        assert!(request.request(1920, 1080));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(request.preferred(), Some((1920, 1080)));
    }
}
