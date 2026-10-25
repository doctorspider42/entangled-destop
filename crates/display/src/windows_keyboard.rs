//! Intercept Windows shell shortcuts only while this VM owns keyboard focus.
//! The low-level hook runs on a dedicated message thread; rendering must never
//! delay a hook callback. It records no text and forwards ordinary keys to winit.

use std::cell::RefCell;
use std::sync::{
    atomic::{AtomicI32, AtomicU64, Ordering},
    mpsc, Arc,
};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_REMOVE};
use windows::Win32::UI::WindowsAndMessaging::*;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::system_keys::{message_lparam, SystemKeys};

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

static CALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Winit enables raw keyboard input together with the raw mouse. This display
/// consumes only raw mouse motion; keyboard events must keep the legacy path.
pub(crate) fn retain_legacy_keyboard() -> Result<(), String> {
    let keyboard = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 6,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    // SAFETY: one valid descriptor; removal requires a null target window.
    unsafe { RegisterRawInputDevices(&[keyboard], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        .map_err(|error| error.to_string())
}

#[derive(Default)]
struct Diagnostics {
    calls: AtomicU64,
    tabs: AtomicU64,
    foreground: AtomicU64,
    tab_epoch: AtomicU64,
    forwarded: AtomicU64,
    post_error: AtomicI32,
}

struct State {
    hwnd: isize,
    epoch: Arc<AtomicU64>,
    seen_epoch: u64,
    keys: SystemKeys,
    diagnostics: Arc<Diagnostics>,
}

pub(crate) struct KeyboardHook {
    epoch: Arc<AtomicU64>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    hwnd: isize,
    starting_callbacks: u64,
    diagnostics: Arc<Diagnostics>,
}

impl KeyboardHook {
    pub(crate) fn new(window: &winit::window::Window) -> Result<Self, String> {
        let RawWindowHandle::Win32(handle) =
            window.window_handle().map_err(|e| e.to_string())?.as_raw()
        else {
            return Err("a Windows window handle is required".into());
        };
        let hwnd = handle.hwnd.get();
        Self::install(hwnd)
    }

    fn install(hwnd: isize) -> Result<Self, String> {
        let epoch = Arc::new(AtomicU64::new(0));
        let shared_epoch = Arc::clone(&epoch);
        let diagnostics = Arc::new(Diagnostics::default());
        let worker_diagnostics = Arc::clone(&diagnostics);
        let starting_callbacks = CALLBACKS.load(Ordering::Relaxed);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("guest-keyboard".into())
            .spawn(move || {
                STATE.with(|slot| {
                    *slot.borrow_mut() = Some(State {
                        hwnd,
                        epoch: shared_epoch,
                        seen_epoch: 0,
                        keys: SystemKeys::default(),
                        diagnostics: worker_diagnostics,
                    })
                });
                // SAFETY: NULL names the current executable; this module outlives the hook.
                let module = unsafe { GetModuleHandleW(None) };
                let hook = module.and_then(|m| {
                    // SAFETY: callback has the required ABI and lives for the process lifetime.
                    // WH_KEYBOARD_LL invokes it on this thread, never in a foreign process.
                    unsafe {
                        SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), Some(HINSTANCE(m.0)), 0)
                    }
                });
                let hook = match hook {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let mut message = MSG::default();
                // SAFETY: message points to writable storage. Peek creates this thread's
                // message queue before Drop may post WM_QUIT; the thread ID is our own.
                let thread_id = unsafe {
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                    GetCurrentThreadId()
                };
                if ready_tx.send(Ok(thread_id)).is_ok() {
                    loop {
                        // SAFETY: writable MSG; this thread owns its message queue. The hook
                        // executes during GetMessage, and no application messages are dispatched.
                        if unsafe { GetMessageW(&mut message, None, 0, 0) }.0 <= 0 {
                            break;
                        }
                    }
                }
                // SAFETY: this thread successfully installed this still-owned hook above.
                let _ = unsafe { UnhookWindowsHookEx(hook) };
                STATE.with(|slot| *slot.borrow_mut() = None);
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                epoch,
                thread_id,
                thread: Some(thread),
                hwnd,
                starting_callbacks,
                diagnostics,
            }),
            result => {
                let _ = thread.join();
                Err(match result {
                    Ok(Err(e)) => e,
                    _ => "keyboard hook thread exited".into(),
                })
            }
        }
    }

    pub(crate) fn set_grab(&self, grabbed: bool) {
        if (self.epoch.load(Ordering::SeqCst) & 1 != 0) != grabbed {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        self.set_grab(false);
        // SAFETY: ready handshake proved this live worker owns a message queue.
        // WM_QUIT carries no pointers and causes it to unhook before exiting.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        tracing::debug!(
            hwnd = self.hwnd,
            callbacks = CALLBACKS
                .load(Ordering::Relaxed)
                .saturating_sub(self.starting_callbacks),
            state_calls = self.diagnostics.calls.load(Ordering::Relaxed),
            alt_tabs = self.diagnostics.tabs.load(Ordering::Relaxed),
            tab_foreground = self.diagnostics.foreground.load(Ordering::Relaxed),
            tab_epoch = self.diagnostics.tab_epoch.load(Ordering::Relaxed),
            forwarded = self.diagnostics.forwarded.load(Ordering::Relaxed),
            post_error = self.diagnostics.post_error.load(Ordering::Relaxed),
            "Windows shortcut capture diagnostics (counts only)"
        );
    }
}

unsafe extern "system" fn callback(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(
            wp.0 as u32,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
    {
        CALLBACKS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: for HC_ACTION Windows provides a KBDLLHOOKSTRUCT valid for this callback.
        let key = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        let taken = STATE
            .try_with(|slot| {
                let Ok(mut slot) = slot.try_borrow_mut() else {
                    return false;
                };
                let Some(state) = slot.as_mut() else {
                    return false;
                };
                state.diagnostics.calls.fetch_add(1, Ordering::Relaxed);
                let epoch = state.epoch.load(Ordering::SeqCst);
                if epoch != state.seen_epoch {
                    state.keys = SystemKeys::default();
                    state.seen_epoch = epoch;
                }
                // SAFETY: both APIs are pointer-free queries of the current desktop.
                // Ctrl is queried for a different key (Escape), never for its own transition.
                let (foreground, ctrl) = unsafe {
                    (
                        GetForegroundWindow().0 as isize,
                        GetAsyncKeyState(i32::from(VK_CONTROL.0)) < 0,
                    )
                };
                let active = epoch & 1 != 0 && foreground == state.hwnd;
                if key.vkCode == 9 && key.flags.contains(LLKHF_ALTDOWN) {
                    state.diagnostics.tabs.fetch_add(1, Ordering::Relaxed);
                    state
                        .diagnostics
                        .foreground
                        .store(foreground as u64, Ordering::Relaxed);
                    state.diagnostics.tab_epoch.store(epoch, Ordering::Relaxed);
                }
                let down = !key.flags.contains(LLKHF_UP);
                let Some(repeat) = state.keys.route(
                    active,
                    key.vkCode,
                    down,
                    key.flags.contains(LLKHF_ALTDOWN),
                    ctrl,
                ) else {
                    return false;
                };
                let bits = message_lparam(
                    key.scanCode,
                    key.flags.contains(LLKHF_EXTENDED),
                    down,
                    repeat,
                );
                // SAFETY: hwnd is this still-focused window. This integer-only
                // packet carries no pointers. The SAME queue as ordinary winit
                // keys preserves ordering with Alt/Shift/Ctrl and their releases.
                // Normal KEY messages bypass DefWindowProc's SYSKEY actions.
                let posted = unsafe {
                    PostMessageW(
                        Some(HWND(state.hwnd as *mut _)),
                        if down { WM_KEYDOWN } else { WM_KEYUP },
                        WPARAM(key.vkCode as usize),
                        LPARAM(bits as isize),
                    )
                };
                match posted {
                    Ok(()) => {
                        state.diagnostics.forwarded.fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    Err(error) => {
                        state
                            .diagnostics
                            .post_error
                            .store(error.code().0, Ordering::Relaxed);
                        false
                    }
                }
            })
            .unwrap_or(false);
        if taken {
            return LRESULT(1);
        }
    }
    // SAFETY: unchanged callback parameters; NULL is valid for the ignored hook handle.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;

    #[test]
    fn raw_mouse_is_retained_while_unused_raw_keyboard_is_removed() {
        use windows::Win32::UI::Input::{GetRegisteredRawInputDevices, RIDEV_DEVNOTIFY};
        // SAFETY: message-only system-class window; never visible or focused.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Raw input registration test"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_DEVNOTIFY,
                hwndTarget: window,
            },
            RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_DEVNOTIFY,
                hwndTarget: window,
            },
        ];
        let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
        // SAFETY: both descriptors name this test's live window and are sized correctly.
        unsafe { RegisterRawInputDevices(&devices, size) }.unwrap();
        retain_legacy_keyboard().unwrap();
        let mut found = [RAWINPUTDEVICE::default(); 8];
        let mut count = found.len() as u32;
        // SAFETY: the buffer holds count descriptors and the OS receives its exact size.
        let used =
            unsafe { GetRegisteredRawInputDevices(Some(found.as_mut_ptr()), &mut count, size) };
        assert!(used <= found.len() as u32, "raw input query failed");
        let found = &found[..used as usize];
        assert!(found
            .iter()
            .any(|d| d.usUsagePage == 1 && d.usUsage == 2 && d.hwndTarget == window));
        assert!(!found.iter().any(|d| d.usUsagePage == 1 && d.usUsage == 6));
        let remove = RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        };
        // SAFETY: removal requires null target; this process owns the registration.
        unsafe { RegisterRawInputDevices(&[remove], size) }.unwrap();
        // SAFETY: still this test's live message-only window.
        unsafe { DestroyWindow(window) }.unwrap();
    }

    #[test]
    fn a_real_hook_worker_can_be_repeatedly_installed_disarmed_and_joined() {
        // SAFETY: STATIC is a system class. A message-only window has no visible
        // surface and never gains focus, so the test cannot capture desktop input.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Entangled keyboard lifetime test"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        for _ in 0..4 {
            let hook = KeyboardHook::install(window.0 as isize).unwrap();
            hook.set_grab(true);
            assert_eq!(hook.epoch.load(Ordering::SeqCst) & 1, 1);
            hook.set_grab(false);
            assert_eq!(hook.epoch.load(Ordering::SeqCst) & 1, 0);
            drop(hook); // joins: a missing message queue/unhook would hang here
        }
        // SAFETY: this test created and still owns the message-only window.
        unsafe { DestroyWindow(window) }.unwrap();
    }

    #[test]
    fn a_new_grab_can_replace_a_hook_whose_worker_was_lost() {
        // SAFETY: an invisible message-only STATIC window never takes focus
        // or captures real desktop input. It is owned and destroyed here.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Hook recovery test"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        let old = KeyboardHook::install(window.0 as isize).unwrap();
        old.set_grab(true);
        // SAFETY: the ready handshake established this owned thread's queue.
        unsafe { PostThreadMessageW(old.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !old.thread.as_ref().unwrap().is_finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(old.thread.as_ref().unwrap().is_finished());
        drop(old);
        let next = KeyboardHook::install(window.0 as isize).unwrap();
        next.set_grab(true);
        assert!(!next.thread.as_ref().unwrap().is_finished());
        assert_eq!(next.epoch.load(Ordering::SeqCst) & 1, 1);
        drop(next);
        // SAFETY: still this test's live message-only window.
        unsafe { DestroyWindow(window) }.unwrap();
    }
}
