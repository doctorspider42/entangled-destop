//! The Windows half: the port (overlapped I/O, so reading and writing do not
//! wait for each other) and the clipboard (`CF_UNICODETEXT`).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HGLOBAL};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

use crate::protocol::{self, Decoder, KIND_CLIPBOARD_TEXT};

/// How often the clipboard is looked at.
const POLL: Duration = Duration::from_millis(300);
/// How long to wait before opening the port again.
const RETRY: Duration = Duration::from_secs(5);

/// The port's handle, shared by the reader thread and the writer.
struct Port(HANDLE);

// SAFETY: a file handle is a kernel object reference; overlapped I/O on it
// from two threads, each with its own OVERLAPPED and event, is what Windows
// documents overlapped handles for.
unsafe impl Send for Port {}
// SAFETY: as above; nothing in `Port` is mutated after it is made.
unsafe impl Sync for Port {}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// What the agent last put on, or sent from, the clipboard — never echoed.
#[derive(Default)]
struct Seen {
    text: Option<String>,
}

pub fn run() {
    loop {
        match open_port() {
            Some(port) => serve(Arc::new(port)),
            None => std::thread::sleep(RETRY),
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_port() -> Option<Port> {
    let path = wide(&format!(r"\\.\Global\{}", protocol::PORT_NAME));
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the
    // call; the other arguments are plain values.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .ok()?;
    Some(Port(handle))
}

/// Runs one connection until the port fails.
fn serve(port: Arc<Port>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reader = {
        let port = Arc::clone(&port);
        let seen = Arc::clone(&seen);
        let alive = Arc::clone(&alive);
        std::thread::spawn(move || {
            read_loop(&port, &seen);
            alive.store(false, std::sync::atomic::Ordering::Release);
        })
    };
    // SAFETY: a plain query with no arguments.
    let mut sequence = unsafe { GetClipboardSequenceNumber() };
    while alive.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(POLL);
        // SAFETY: as above.
        let now = unsafe { GetClipboardSequenceNumber() };
        if now == sequence {
            continue;
        }
        sequence = now;
        let Some(text) = clipboard_text() else {
            continue;
        };
        {
            let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
            if seen.text.as_deref() == Some(text.as_str()) {
                continue;
            }
            seen.text = Some(text.clone());
        }
        if !write_all(
            &port,
            &protocol::encode(KIND_CLIPBOARD_TEXT, text.as_bytes()),
        ) {
            break;
        }
    }
    drop(port);
    let _ = reader.join();
}

fn read_loop(port: &Port, seen: &Mutex<Seen>) {
    let mut decoder = Decoder::default();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let Some(n) = overlapped(port, |handle, overlapped| {
            // SAFETY: `buffer` and `overlapped` outlive the operation, which
            // `overlapped()` waits for before either is touched again.
            unsafe { ReadFile(handle, Some(&mut buffer), None, Some(overlapped)) }
        }) else {
            return;
        };
        for (kind, payload) in decoder.push(&buffer[..n]) {
            if kind != KIND_CLIPBOARD_TEXT {
                continue;
            }
            let Ok(text) = String::from_utf8(payload) else {
                continue;
            };
            seen.lock().unwrap_or_else(PoisonError::into_inner).text = Some(text.clone());
            set_clipboard_text(&text);
        }
    }
}

fn write_all(port: &Port, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let chunk = bytes;
        let Some(n) = overlapped(port, |handle, overlapped| {
            // SAFETY: `chunk` and `overlapped` outlive the operation, which
            // `overlapped()` waits for.
            unsafe { WriteFile(handle, Some(chunk), None, Some(overlapped)) }
        }) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        bytes = &bytes[n.min(bytes.len())..];
    }
    true
}

/// Starts one overlapped operation and waits for it; answers the bytes
/// transferred, or `None` when the port has failed.
fn overlapped(
    port: &Port,
    start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> windows::core::Result<()>,
) -> Option<usize> {
    // SAFETY: a manual-reset, unsignalled, unnamed event.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.ok()?;
    let mut ov = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    let started = start(port.0, &mut ov);
    let pending = match &started {
        Ok(()) => true,
        // ERROR_IO_PENDING: started, finishing later.
        Err(error) => error.code().0 as u32 == 0x8007_03e5,
    };
    let mut transferred = 0u32;
    // SAFETY: `ov` is the structure the operation was started with and lives
    // until this returns; waiting is what makes reusing it safe.
    let done =
        pending && unsafe { GetOverlappedResult(port.0, &ov, &mut transferred, true) }.is_ok();
    // SAFETY: the event was created above and is closed once.
    let _ = unsafe { CloseHandle(event) };
    done.then_some(transferred as usize)
}

/// The clipboard's text, when it has some.
fn clipboard_text() -> Option<String> {
    // SAFETY: open for this thread; every return below closes it.
    unsafe { OpenClipboard(None) }.ok()?;
    let text = (|| {
        // SAFETY: the clipboard is open; the handle is the clipboard's, read
        // under GlobalLock and never freed here.
        let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }.ok()?;
        let global = HGLOBAL(handle.0);
        // SAFETY: as above.
        let data = unsafe { GlobalLock(global) } as *const u16;
        if data.is_null() {
            return None;
        }
        // SAFETY: CF_UNICODETEXT is NUL-terminated UTF-16; the walk stops at
        // the terminator and at the 4 MiB the protocol carries.
        let units = unsafe {
            let mut len = 0usize;
            while len < crate::protocol::MAX_PAYLOAD / 2 && *data.add(len) != 0 {
                len += 1;
            }
            std::slice::from_raw_parts(data, len).to_vec()
        };
        // SAFETY: locked above.
        let _ = unsafe { GlobalUnlock(global) };
        Some(String::from_utf16_lossy(&units))
    })();
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    text
}

fn set_clipboard_text(text: &str) {
    let units = wide(text);
    let bytes = units.len() * 2;
    // SAFETY: open for this thread; closed below on every path.
    if unsafe { OpenClipboard(None) }.is_err() {
        return;
    }
    // SAFETY: the clipboard is open. The allocation is filled under
    // GlobalLock and handed to SetClipboardData, which owns it on success.
    unsafe {
        let _ = EmptyClipboard();
        if let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let target = GlobalLock(global) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
                let _ = GlobalUnlock(global);
                let _ = SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0)));
            }
        }
        let _ = CloseClipboard();
    }
}
