//! Intercept Windows shell shortcuts only while this VM owns keyboard focus.
//! The low-level hook runs on a dedicated message thread; rendering must never
//! delay a hook callback. It records no text and forwards ordinary keys to winit.

use std::cell::RefCell;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::*;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::system_keys::{message_lparam, SystemKeys};

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    hwnd: isize,
    epoch: Arc<AtomicU64>,
    seen_epoch: u64,
    keys: SystemKeys,
}

pub(crate) struct KeyboardHook {
    epoch: Arc<AtomicU64>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
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
    }
}

unsafe extern "system" fn callback(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(
            wp.0 as u32,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
    {
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
                unsafe {
                    PostMessageW(
                        Some(HWND(state.hwnd as *mut _)),
                        if down { WM_KEYDOWN } else { WM_KEYUP },
                        WPARAM(key.vkCode as usize),
                        LPARAM(bits as isize),
                    )
                }
                .is_ok()
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
