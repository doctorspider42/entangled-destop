//! Where the generated host calls ([`super::calls`]) keep the `ash` twins of
//! a command's structures while the driver reads them.
//!
//! Every value handed to [`Arena::one`] or [`Arena::slice`] is moved into a
//! box the arena owns, and the pointer returned points at that box's
//! contents: a box's contents never move when the box does, so the pointer
//! stays valid for as long as the arena lives, however many values are added
//! after it. Nothing is ever taken out or dropped before the arena itself, so
//! a pointer is never left dangling while the arena is alive. Creating the
//! pointers is safe; dereferencing them is the driver's business, inside the
//! one `unsafe` call each generated function makes, while the arena is still
//! in scope.

use std::any::Any;
use std::ffi::c_char;
use std::fmt;

pub use crate::venus::executor::host::CallError;

/// The arena. See the module docs.
#[derive(Default)]
pub struct Arena {
    keep: Vec<Box<dyn Any>>,
}

impl fmt::Debug for Arena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arena")
            .field("values", &self.keep.len())
            .finish()
    }
}

impl Arena {
    /// Keep `value`; a pointer to it, valid while the arena lives.
    pub fn one<T: 'static>(&mut self, value: T) -> *mut T {
        self.keep.push(Box::new(value));
        self.keep
            .last_mut()
            .and_then(|b| b.downcast_mut::<T>())
            .map_or(std::ptr::null_mut(), |r| r as *mut T)
    }

    /// Keep `values`; a pointer to the first, valid while the arena lives.
    /// Null for an empty array, which is how Vulkan spells "no elements".
    pub fn slice<T: 'static>(&mut self, values: Vec<T>) -> *mut T {
        if values.is_empty() {
            return std::ptr::null_mut();
        }
        self.keep.push(Box::new(values));
        self.keep
            .last_mut()
            .and_then(|b| b.downcast_mut::<Vec<T>>())
            .map_or(std::ptr::null_mut(), Vec::as_mut_ptr)
    }

    /// A C string of `text` up to its first NUL (the wire carries none, but
    /// a guest may put one inside), terminated.
    pub fn cstr(&mut self, text: &[u8]) -> *const c_char {
        let end = text.iter().position(|b| *b == 0).unwrap_or(text.len());
        let mut bytes: Vec<u8> = text.get(..end).unwrap_or_default().to_vec();
        bytes.push(0);
        self.slice(bytes).cast_const().cast()
    }
}

/// Most handles one call may create: the object table's own bound.
pub const MAX_OUTPUT_HANDLES: u64 = crate::venus::executor::objects::MAX_OBJECTS_PER_CONTEXT as u64;

/// Most bytes one call may write back to the guest (query results, pipeline
/// cache data): far past any reply window, and far below host memory.
pub const MAX_OUTPUT_BYTES: u64 = 64 << 20;

/// `len` must be `count`.
///
/// # Errors
/// [`CallError::Count`].
pub fn same(len: usize, count: u64, what: &'static str) -> Result<(), CallError> {
    if !u64::try_from(len).is_ok_and(|len| len == count) {
        return Err(CallError::Count { what });
    }
    Ok(())
}

/// `count` output handles, bounded by [`MAX_OUTPUT_HANDLES`].
///
/// # Errors
/// [`CallError::TooLarge`].
pub fn bounded(count: u64, what: &'static str) -> Result<usize, CallError> {
    if count > MAX_OUTPUT_HANDLES {
        return Err(CallError::TooLarge { what });
    }
    usize::try_from(count).map_err(|_| CallError::TooLarge { what })
}

/// `count` output bytes, bounded by [`MAX_OUTPUT_BYTES`].
///
/// # Errors
/// [`CallError::TooLarge`].
pub fn bounded_bytes(count: u64, what: &'static str) -> Result<usize, CallError> {
    if count > MAX_OUTPUT_BYTES {
        return Err(CallError::TooLarge { what });
    }
    usize::try_from(count).map_err(|_| CallError::TooLarge { what })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_stay_where_they_were_while_the_arena_grows() {
        let mut a = Arena::default();
        let first = a.one(0x1234_5678_u32);
        let slice = a.slice(vec![1u64, 2, 3]);
        for i in 0..1000u32 {
            a.one(i);
        }
        // SAFETY: both point into boxes the arena still owns.
        unsafe {
            assert_eq!(*first, 0x1234_5678);
            assert_eq!(*slice.add(2), 3);
        }
        assert!(a.slice(Vec::<u8>::new()).is_null());
    }

    #[test]
    fn a_c_string_stops_at_the_first_nul_and_is_terminated() {
        let mut a = Arena::default();
        let s = a.cstr(b"main\0tail");
        // SAFETY: `cstr` returns a NUL-terminated copy the arena owns.
        let back = unsafe { std::ffi::CStr::from_ptr(s) };
        assert_eq!(back.to_bytes(), b"main");
    }
}
