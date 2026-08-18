//! Locking helper shared by the pieces the event loop and the device threads
//! both touch.

use std::sync::{Mutex, MutexGuard};

/// Locks a mutex, recovering from poisoning instead of panicking.
///
/// Every critical section in this crate is a bounded, panic-free `memcpy` or
/// queue push, so a poisoned lock means an unrelated thread died while holding
/// it — the data is still structurally valid and the VM must keep running
/// (`unwrap()` on a runtime path is a workspace hard rule violation).
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(lock = what, "mutex was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}
