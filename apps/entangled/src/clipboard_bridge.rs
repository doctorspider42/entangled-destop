//! The host half of the shared clipboard (ADR-0008): the guest agent's port
//! on one side, the host clipboard on the other.
//!
//! Text only, both ways. The host clipboard is looked at every
//! [`POLL`]; a change is sent to the guest. What the guest sends is put on
//! the host clipboard. Each side remembers the last text it exchanged, so
//! nothing bounces back and forth. The wire format is the agent's own
//! (`guest/windows-agent/src/protocol.rs`, included here so the two cannot
//! drift apart).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

#[path = "../../../guest/windows-agent/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

/// The port the agent opens.
pub(super) const PORT_NAME: &str = protocol::PORT_NAME;

/// How often the host clipboard is looked at.
const POLL: Duration = Duration::from_millis(400);

/// The bridge's thread; stopped and joined on drop.
pub(super) struct Bridge {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Bridge {
    pub(super) fn start(port: virtio_console::PortHandle) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        // What the two sides last agreed on.
        let last = Arc::new(Mutex::new(None::<String>));
        // The guest's messages, applied on the port's thread: setting the
        // host clipboard is quick, and keeping it there keeps the order.
        {
            let last = Arc::clone(&last);
            let decoder = Mutex::new(protocol::Decoder::default());
            port.receive(Arc::new(move |bytes: &[u8]| {
                let messages = decoder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(bytes);
                for (kind, payload) in messages {
                    if kind != protocol::KIND_CLIPBOARD_TEXT {
                        continue;
                    }
                    let Ok(text) = String::from_utf8(payload) else {
                        continue;
                    };
                    *last.lock().unwrap_or_else(PoisonError::into_inner) = Some(text.clone());
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                        Ok(()) => tracing::debug!("guest clipboard copied to the host"),
                        Err(error) => tracing::warn!(%error, "cannot set the host clipboard"),
                    }
                }
            }));
        }
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("clipboard-bridge".into())
                .spawn(move || watch_host(&port, &last, &stop))?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch_host(port: &virtio_console::PortHandle, last: &Mutex<Option<String>>, stop: &AtomicBool) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            tracing::warn!(%error, "no host clipboard; the guest's clipboard stays its own");
            return;
        }
    };
    let mut was_open = false;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(POLL);
        let open = port.is_open();
        if open != was_open {
            tracing::info!(connected = open, "guest agent");
            was_open = open;
            if open {
                // A fresh agent knows nothing: offer it the host's text.
                *last.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
        }
        if !open {
            continue;
        }
        let Ok(text) = clipboard.get_text() else {
            continue;
        };
        if text.len() > protocol::MAX_PAYLOAD {
            continue;
        }
        let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_deref() == Some(text.as_str()) {
            continue;
        }
        if port.send(&protocol::encode(
            protocol::KIND_CLIPBOARD_TEXT,
            text.as_bytes(),
        )) {
            *last = Some(text);
        }
    }
}
