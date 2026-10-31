//! Thread-local autorelease scopes for Metal-backed Vulkan implementations.
//! Native APIs own their returned Vulkan handles. No autoreleased Objective-C
//! references enter or leave this module; only temporary driver objects drain.

pub(super) struct Pool {
    #[cfg(target_os = "macos")]
    token: *mut std::ffi::c_void,
    // A pool is popped on the creating thread, including during unwinding.
    #[cfg(target_os = "macos")]
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Pool {
    #[inline]
    pub(super) fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            // SAFETY: returns a fresh opaque token for the current thread.
            // The private guard cannot escape a Vulkan operation, and its
            // thread-affine Drop pops it before any enclosing pool is popped.
            token: unsafe { objc2::ffi::objc_autoreleasePoolPush() },
            #[cfg(target_os = "macos")]
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for Pool {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: this token was pushed on this thread by new(). The guard
        // is not Send/Sync, cannot be cloned, and lexical scopes preserve
        // last-in-first-out nesting. No pool-borrowed Rust references exist.
        unsafe {
            objc2::ffi::objc_autoreleasePoolPop(self.token)
        };
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol};

    #[test]
    fn temporary_native_objects_release_at_the_inner_operation_boundary() {
        // This private object is never published, so its retain count has no
        // concurrent owners and can test the native pool's actual release.
        let object = NSObject::new();
        let baseline = object.retainCount();
        for _ in 0..64 {
            let outer = Pool::new();
            let _ = Retained::autorelease_ptr(object.clone());
            assert_eq!(object.retainCount(), baseline + 1);
            let inner = Pool::new();
            let _ = Retained::autorelease_ptr(object.clone());
            assert_eq!(object.retainCount(), baseline + 2);
            drop(inner);
            assert_eq!(object.retainCount(), baseline + 1);
            drop(outer);
            assert_eq!(object.retainCount(), baseline);
        }
    }
}
