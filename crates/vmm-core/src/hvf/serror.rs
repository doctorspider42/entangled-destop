//! SError state access was added in macOS 27. Resolve both entry points at
//! runtime so the backend still links against older SDKs. Never assume that
//! unreadable exception state is empty when saving or restoring a CPU.
use super::{dlsym, HvfError};
use std::ffi::{c_void, CStr};
use std::sync::OnceLock;

pub(super) struct SerrorApi {
    pub get: unsafe extern "C" fn(u64, *mut bool) -> i32,
    pub set: unsafe extern "C" fn(u64, bool) -> i32,
}

impl SerrorApi {
    pub fn load() -> Result<&'static Self, HvfError> {
        static API: OnceLock<Option<SerrorApi>> = OnceLock::new();
        API.get_or_init(|| {
            Self::resolve(|name| {
                // SAFETY: RTLD_DEFAULT and a static SDK symbol. Hypervisor is
                // linked for the process lifetime; absent symbols return null.
                unsafe { dlsym((-2isize) as *mut c_void, name.as_ptr()) }
            })
        })
        .as_ref()
        .ok_or(HvfError::SnapshotUnsupported)
    }

    fn resolve(mut lookup: impl FnMut(&CStr) -> *mut c_void) -> Option<Self> {
        let get = lookup(c"hv_vcpu_get_serror");
        let set = lookup(c"hv_vcpu_set_serror");
        if get.is_null() || set.is_null() {
            return None;
        }
        Some(Self {
            // SAFETY: the exact SDK entry point has this C signature, with
            // hv_vcpu_t = uint64_t and a writable C bool output.
            get: unsafe {
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u64, *mut bool) -> i32>(get)
            },
            // SAFETY: the exact SDK setter takes hv_vcpu_t and C bool. Its
            // framework remains loaded for the cached pointer's lifetime.
            set: unsafe {
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u64, bool) -> i32>(set)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn get(cpu: u64, pending: *mut bool) -> i32 {
        assert_eq!(cpu, 17);
        // SAFETY: the test passes a writable bool local.
        unsafe { pending.write(true) };
        23
    }

    unsafe extern "C" fn set(cpu: u64, pending: bool) -> i32 {
        assert_eq!(cpu, 17);
        assert!(pending);
        29
    }

    #[test]
    fn missing_either_entry_point_refuses_partial_exception_state() {
        for missing in ["hv_vcpu_get_serror", "hv_vcpu_set_serror", "both"] {
            assert!(SerrorApi::resolve(|name| {
                if missing == "both" || name.to_bytes() == missing.as_bytes() {
                    std::ptr::null_mut()
                } else if name == c"hv_vcpu_get_serror" {
                    get as *const () as *mut c_void
                } else {
                    set as *const () as *mut c_void
                }
            })
            .is_none());
        }
    }

    #[test]
    fn resolved_pair_preserves_pending_state_and_native_status() {
        let api = SerrorApi::resolve(|name| {
            if name == c"hv_vcpu_get_serror" {
                get as *const () as *mut c_void
            } else {
                set as *const () as *mut c_void
            }
        })
        .unwrap();
        let mut pending = false;
        // SAFETY: these are the test functions above, with the exact types
        // and a writable output. No real vCPU or native state is involved.
        unsafe {
            assert_eq!((api.get)(17, &mut pending), 23);
            assert!(pending);
            assert_eq!((api.set)(17, pending), 29);
        }
    }
}
