//! Host memory the guest can see: the pages a Venus command ring lives in, the
//! atomics the pump performs on them, and the act of putting them in front of a
//! guest and taking them back (EPIC 20, ADR-0004, VEN-2003).
//!
//! This is the only file in the `venus` module family with `unsafe` in it, and
//! the reason is the one thing about Venus that is easy to get backwards.
//!
//! # The ring does not travel through [`ShmBacking`]
//!
//! [`virtio_core::ShmBacking`] has two halves and they are not the same
//! mechanism. [`read`](ShmBacking::read)/[`write`](ShmBacking::write)/
//! [`fill`](ShmBacking::fill) are a *copying* interface over pages the machine
//! layer owns: no atomics, no shared pointer, and on a
//! [`host_mapped`](ShmBacking::host_mapped) window they fail outright, because
//! the pages they would touch are not the ones the guest sees.
//! [`map_host`](ShmBacking::map_host) is the other half: it *publishes*
//! renderer-owned host pages into the window, after which the guest reads and
//! writes them with no VM exit and no host code anywhere in the path.
//!
//! A command ring needs the second one. The head/tail protocol is three
//! lock-free 32-bit atomics that both sides hammer concurrently; a copying
//! interface cannot express it, and a window that exits to the host on every
//! guest store to `tail` would cost more than the ring saves. So the shape is:
//! **the host allocates the pages, keeps the pointer, performs its own atomic
//! accesses on it, and hands the address to `map_host` so the guest can reach
//! the same bytes.**
//!
//! This is what virglrenderer does. `vkr_context.c:263-290`
//! (`vkr_context_get_blob`) creates the ring's memory as a `memfd`, `mmap`s it
//! into the renderer, and hands the fd out to the VMM to place in the guest's
//! address space; `vkr_ring.c` then operates on the renderer's own mapping with
//! `atomic_load`/`atomic_store`. We allocate instead of `memfd`-ing, because
//! our [`ShmBacking::map_host`] takes a host address rather than a descriptor
//! and is already the portable seam (ADR-0002) — there is no `memfd` on
//! Windows, and there does not need to be.
//!
//! # What is here
//!
//! * [`RingPages`] — an owned, page-aligned, zero-initialised allocation that
//!   stands in for one shared-memory *resource*, and the [`RingBacking`]
//!   implementation over it. Every offset it is handed is resource-relative and
//!   comes from a [`RingLayout`]; see "bounds" below.
//! * [`Publication`] — the proof that pages are currently in front of a guest.
//!   Holding one keeps the pages alive; dropping one takes them back down.
//! * [`RingPages::write_bytes`] / [`RingPages::read_bytes`] — bounded byte
//!   copies into and out of any host blob, which is how the executor's replies
//!   reach a guest's reply window (stage 5a.3) without a `&mut [u8]` ever
//!   being formed over memory the guest can see, and without a second
//!   `unsafe` anywhere else in the family.
//!
//! # The lifetime obligation, and how it is discharged
//!
//! [`ShmBacking::map_host`]'s safety contract is that the span "stays mapped at
//! that address until the matching `unmap_host` has returned". This is not an
//! ordinary use-after-free: the guest reaches these pages through a hypervisor
//! mapping, so freeing them under a live mapping hands the guest whatever the
//! host allocator puts there next, with no fault, no exit and nothing to catch
//! it.
//!
//! So [`RingPages::publish`] is spelled on `&Arc<RingPages>` and hands back a
//! [`Publication`] that *owns* an `Arc` of the same allocation. The pages
//! therefore cannot be freed while a publication exists — not "should not", but
//! cannot, because the refcount the mapping holds is the same one the allocator
//! waits on. [`Publication::drop`] calls `unmap_host` first and releases its
//! `Arc` afterwards (a `Drop` impl runs before the value's fields are dropped),
//! so the order is always *unmap, then free*, and it holds on an unwinding path
//! too.
//!
//! What the type system cannot carry, and what a caller therefore owes:
//!
//! * [`RingPages::as_ptr`] and [`RingPages::host_addr`] hand out a raw pointer
//!   and a raw address. Anything built on those is outside this file's
//!   guarantee; in particular, publishing the address by any route other than
//!   [`RingPages::publish`] re-opens exactly the hole `Publication` closes.
//! * `std::mem::forget`ing a [`Publication`] leaks the pages rather than
//!   freeing them under the guest. That is the right failure — a leak, not a
//!   use-after-free — but it is still a leak, and the mapping stays up.
//! * A [`ShmBacking`] whose `unmap_host` returns before the hypervisor has
//!   actually torn the mapping down breaks the contract on its own side. The
//!   trait documents `unmap_host` as infallible for that reason.
//!
//! # Bounds
//!
//! Every offset that reaches a [`RingBacking`] method came, ultimately, from a
//! [`RingLayout`] built out of guest-proposed numbers. [`super::ring`] already
//! checked those numbers — but it checked them against a `resource_size` it was
//! *told*. If that size is not this allocation's, the proof does not apply, and
//! the guest's offsets are back to being arbitrary.
//!
//! That mismatch would be a bug in us, not in the guest, so it is caught in two
//! places and is never silent:
//!
//! * [`RingPages::accepts`] refuses the layout up front, by name, and
//!   [`RingPages::adopt`] makes that refusal impossible to skip on the way to a
//!   [`RingPump`];
//! * every accessor re-checks anyway, because the [`RingBacking`] trait's
//!   methods are infallible and a `RingPump` can be built without going through
//!   `adopt`. A failed check logs at `error` level, sets
//!   [`RingPages::is_poisoned`], and answers with a value the pump refuses
//!   rather than one it believes.
//!
//! [`RingPages::store_extra`] is the one accessor that is also handed an offset
//! *the guest chose*, rather than one a [`RingLayout`] vouched for:
//! `vkWriteRingExtraMESA` carries one. It therefore separates the two failures
//! instead of treating them alike — a bad offset is a refused command, only a
//! region that is not this allocation's is a host bug worth poisoning over.
//!
//! # Memory ordering
//!
//! [`RingBacking`]'s docs are explicit and this implementation follows them
//! exactly. The reasoning is repeated here so that a later reader with a
//! benchmark does not "optimise" it away:
//!
//! **`status` is never stored whole.** It is the one control word both sides
//! write: the host sets and clears `IDLE`, `FATAL` and `ALIVE`, and the guest
//! clears `ALIVE` with its own atomic AND when its watchdog arms. Every host
//! write to it is therefore [`RingPages::set_status_bits`] or
//! [`RingPages::clear_status_bits`] — `fetch_or`/`fetch_and` of the named bits,
//! as the reference's `vkr_ring_set_status_bits` is — and the [`RingBacking`]
//! trait offers no whole-word store for it at all. A store computed from a
//! host-side picture of the word would resurrect a bit the guest had just
//! cleared or erase one the monitor had just set.
//!
//! **All three control words are accessed [`SeqCst`].** The reference is weaker
//! — release on `head`, acquire on `tail` — and we could be too, *except* that
//! [`RingPump::enter_idle`] publishes `STATUS_IDLE` and then loads `tail`. A
//! store followed by a load of a different location is the one pairing that
//! release/acquire does **not** order: both may be reordered past each other on
//! any machine with a store buffer, including x86-64. If that reordering
//! happens, the host loads a `tail` from before the guest's store while the
//! guest reads a `status` from before the host's, the guest decides no doorbell
//! is needed, and the host parks forever on work that is already there. Nothing
//! in the protocol recovers from that. On both of our hosts `SeqCst` costs a
//! plain `mov` on the load and an `xchg` on the store, so buying the
//! `StoreLoad` barrier outright is cheaper than reasoning per call site about
//! which one needs it.
//!
//! **The `extra` scratch word is stored [`SeqCst`](Ordering::SeqCst) too**, for
//! rather than by analogy: `vkWriteRingExtraMESA` exists so the guest can poll
//! that word while it runs, so it is a second handshake between the same two
//! parties and not host-private state. See [`RingPages::store_extra`].
//!
//! **Command-buffer bytes are read [`Relaxed`].** They carry no ordering of
//! their own and need none: the `SeqCst` load of `tail` in
//! [`RingPump::pump`] happens before the copy and is what orders the guest's
//! writes to `[cur, tail)` against our reads of them. They are read through
//! [`AtomicU8`] rather than as a plain slice because the guest is writing the
//! same bytes at the same time — a plain `&[u8]` read racing with another
//! writer is undefined behaviour, while a racing relaxed atomic load is merely
//! an unspecified *value*, which is exactly what the pump's shadow copy is
//! designed to cope with.

use std::alloc::{alloc_zeroed, dealloc, Layout, LayoutError};
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use thiserror::Error;
use virtio_core::{ShmBacking, ShmMapError};

use super::pump::{PumpError, RingBacking, RingPump};
use super::ring::{GuestWord, HostWord, Region, RingLayout, RingRegion, Writer, CONTROL_WORD_LEN};

/// Largest shared-memory resource this module will allocate for one ring.
///
/// The resource size is guest-chosen (it arrives on `RESOURCE_CREATE_BLOB`
/// before any ring exists), so it is a guest value that names a host
/// allocation, and the workspace rule is that such a value is bounded before it
/// is believed. [`super::ring::MAX_BUFFER_BYTES`] already caps the command
/// buffer at 16 MiB; doubling that leaves ample room for the three control
/// words, the scratch region and anything a later layout wants to put beside
/// them, while keeping a single `vkCreateRingMESA` from asking the host for an
/// arbitrary number of pages.
pub const MAX_RESOURCE_BYTES: u64 = 2 * super::ring::MAX_BUFFER_BYTES;

/// The smallest page size this module will believe, and the fallback when the
/// host will not say.
///
/// Both of our hosts use 4 KiB pages on x86-64, and both hypervisor mapping
/// calls (`KVM_SET_USER_MEMORY_REGION`, `WHvMapGpaRange`) are specified in
/// those units. A host reporting something *smaller* would not change what the
/// hypervisor demands, so the queried value is only ever rounded up.
pub const MIN_PAGE_BYTES: usize = 4096;

/// Why a set of ring pages could not be allocated, or could not be used with a
/// particular [`RingLayout`].
///
/// Mostly not guest errors. A guest's bad ring is [`super::ring`]'s refusal and
/// its bad protocol is [`PumpError`]; the rest of these are either the host
/// being out of memory or the host having validated a layout against a size
/// that is not this allocation's.
///
/// The three exceptions are the `Extra*` variants, and they are exceptions
/// because [`RingPages::store_extra`] is the one entry point here whose
/// *offset* comes straight off the wire rather than out of a [`RingLayout`]:
/// `vkWriteRingExtraMESA` carries one the guest chose. Those three therefore
/// name a bad guest command, are logged as such, and — unlike every other
/// refusal in this file — do **not** set [`RingPages::is_poisoned`]. A latch
/// meaning "a host bug happened" that a guest can set at will would stop
/// meaning anything, and a guest that can poison its own pages on demand owns a
/// switch it should not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShmemError {
    #[error("a shared-memory resource of zero bytes holds no ring")]
    ZeroSized,

    #[error("a {size:#x}-byte shared-memory resource exceeds the {max:#x}-byte limit")]
    TooLarge { size: u64, max: u64 },

    #[error("{size:#x} bytes aligned to {align:#x} is not a valid allocation")]
    UnusableLayout { size: u64, align: u64 },

    #[error("the host could not allocate {size:#x} bytes aligned to {align:#x}")]
    OutOfMemory { size: u64, align: u64 },

    #[error(
        "the {region} region ends at {end:#x}, past the {size:#x}-byte \
         resource these pages are: the layout was validated against a \
         different allocation"
    )]
    RegionOutsideResource {
        region: RingRegion,
        end: u64,
        size: u64,
    },

    #[error(
        "the {region} control word sits at {offset:#x}, which is not \
         {align}-byte aligned and therefore not atomically accessible",
        align = CONTROL_WORD_LEN
    )]
    MisalignedControlWord { region: RingRegion, offset: u64 },

    #[error("this ring declared no extra region, so there is nowhere to store a word")]
    NoExtraRegion,

    #[error(
        "a {len}-byte store at offset {offset:#x} does not fit the \
         {region_len:#x}-byte extra region",
        len = CONTROL_WORD_LEN
    )]
    ExtraWriteOutOfRange { offset: u64, region_len: u64 },

    #[error(
        "a store at offset {offset:#x} of the extra region lands at {at:#x}, \
         which is not {align}-byte aligned and therefore not atomically \
         accessible",
        align = CONTROL_WORD_LEN
    )]
    MisalignedExtraWrite { offset: u64, at: u64 },

    #[error(
        "a {len:#x}-byte copy at offset {offset:#x} does not fit the {size:#x}-byte \
         resource these pages are"
    )]
    BytesOutsideResource { offset: u64, len: u64, size: u64 },

    #[error(transparent)]
    Pump(#[from] PumpError),
}

/// The host's page size: the granularity a hypervisor maps at.
///
/// Queried once and cached. Two things depend on getting this right rather than
/// assuming 4096:
///
/// * the base address handed to [`ShmBacking::map_host`] must be page-aligned,
///   because that is what both hypervisors demand of a host span;
/// * the *length* must be a whole number of pages, and it must be a whole
///   number of pages **we own**. A mapping rounded up past the end of a smaller
///   allocation would put whatever the host allocator put next in front of the
///   guest. On a host with pages larger than we assumed, that is a real
///   disclosure bug, which is why [`RingPages::new`] rounds the allocation up
///   to this value rather than trusting a constant.
///
/// A queried value that is not a power of two, or is below
/// [`MIN_PAGE_BYTES`], is discarded in favour of [`MIN_PAGE_BYTES`]; a larger
/// one is honoured, because rounding *down* is the unsafe direction.
#[must_use]
pub fn page_size() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let queried = query_page_size();
        if queried.is_power_of_two() && queried >= MIN_PAGE_BYTES {
            queried
        } else {
            MIN_PAGE_BYTES
        }
    })
}

/// Ask the OS. Both arms are declared by hand rather than pulled from a crate:
/// `virtio-gpu` does not depend on `libc`, and the `windows` crate it does
/// depend on is built here without `Win32_System_SystemInformation`. Every
/// process already links the library each arm names, so neither adds a
/// dependency to the graph — which is the whole reason this is a two-line FFI
/// rather than a new entry in `Cargo.toml`.
#[cfg(unix)]
fn query_page_size() -> usize {
    // POSIX's `getpagesize` rather than `sysconf(_SC_PAGESIZE)`: it takes no
    // arguments and needs no ABI constant hard-coded on our side, and every
    // libc std links against (glibc, musl, the BSDs, Darwin) exports it.
    extern "C" {
        fn getpagesize() -> core::ffi::c_int;
    }
    // SAFETY: `getpagesize` takes no arguments, reads and writes no memory the
    // caller owns, returns a plain `int` and cannot fail. The symbol comes from
    // the same libc the Rust standard library is already linked against.
    let reported = unsafe { getpagesize() };
    usize::try_from(reported).unwrap_or(0)
}

/// See [`query_page_size`]'s unix twin. `SYSTEM_INFO` is declared here because
/// `GetSystemInfo` writes the whole struct, so a short one would be a stack
/// overwrite; the field list is Win32's, in order, and comes out at the
/// documented 48 bytes on x86-64 (36 on x86).
#[cfg(windows)]
fn query_page_size() -> usize {
    #[repr(C)]
    struct SystemInfo {
        processor_architecture: u16,
        reserved: u16,
        page_size: u32,
        minimum_application_address: *mut core::ffi::c_void,
        maximum_application_address: *mut core::ffi::c_void,
        active_processor_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemInfo(info: *mut SystemInfo);
    }

    let mut info = std::mem::MaybeUninit::<SystemInfo>::zeroed();
    // SAFETY: `GetSystemInfo` fills exactly one `SYSTEM_INFO`, and `info` is a
    // properly aligned, writable allocation of exactly that struct — the
    // declaration above is Win32's field-for-field, so the size the OS writes
    // is the size we reserved. It cannot fail and returns nothing. The struct
    // is plain data with no invalid bit patterns, and it was zeroed before the
    // call, so `assume_init` is sound whatever the OS chooses to leave alone.
    let info = unsafe {
        GetSystemInfo(info.as_mut_ptr());
        info.assume_init()
    };
    let _ = (
        info.processor_architecture,
        info.reserved,
        info.minimum_application_address,
        info.maximum_application_address,
        info.active_processor_mask,
        info.number_of_processors,
        info.processor_type,
        info.allocation_granularity,
        info.processor_level,
        info.processor_revision,
    );
    usize::try_from(info.page_size).unwrap_or(0)
}

/// Neither host we ship on. Nothing here is reachable in production; the arm
/// exists so the crate keeps building everywhere, which is the portability rule
/// in ADR-0002.
#[cfg(not(any(unix, windows)))]
fn query_page_size() -> usize {
    MIN_PAGE_BYTES
}

/// One shared-memory resource, allocated by the host, that a guest may be shown.
///
/// Page-aligned and zero-initialised at construction. Page alignment is
/// load-bearing twice over: the hypervisor maps at page granularity, and
/// [`super::ring`] judges control-word alignment *resource-relative*, on the
/// stated assumption that the resource base is aligned — so a 4-byte-aligned
/// `headOffset` is only a 4-byte-aligned host word because of this.
///
/// The value is `Send + Sync` and all of its accessors take `&self`, because
/// the ring is shared by definition: a pump thread, the device's queue worker
/// and the guest all touch the same bytes. Every one of this type's own
/// accesses to them is atomic.
pub struct RingPages {
    /// Base of the allocation. Aligned to [`page_size`], valid for
    /// `alloc.size()` bytes, never reallocated, freed exactly once in [`Drop`].
    ptr: NonNull<u8>,
    /// The allocation as `alloc_zeroed` was asked for it; `dealloc` needs it
    /// back unchanged.
    alloc: Layout,
    /// The resource size a [`RingLayout`] must have been validated against.
    /// Never larger than `alloc.size()`, and usually smaller — the allocation
    /// is rounded up to a page and this is not.
    declared: u64,
    /// Set the first time an accessor is handed an offset that is not inside
    /// [`Self::declared`]. See [`Self::is_poisoned`].
    poisoned: AtomicBool,
}

// SAFETY: `RingPages` is a plain owned allocation plus two integers. Every
// access to the bytes through a shared reference goes through `AtomicU32` or
// `AtomicU8`, so concurrent use from several threads is data-race free by
// construction; the pointer is set once at construction and never changed, and
// the only `&mut self` method is `drop`, which Rust already makes exclusive.
// The guest writes the same bytes from outside the Rust abstract machine, which
// no `Send`/`Sync` reasoning can cover and which is the reason the pump copies
// into a shadow before decoding.
unsafe impl Send for RingPages {}
// SAFETY: see the `Send` impl directly above.
unsafe impl Sync for RingPages {}

impl RingPages {
    /// Allocate `size` bytes of zeroed, page-aligned host memory for one
    /// shared-memory resource.
    ///
    /// The allocation is rounded up to a whole number of [`page_size`] bytes,
    /// and *that* rounded length is what [`publish`](Self::publish) shows the
    /// guest — so the guest never sees a byte the host does not own. `size`
    /// itself is what a [`RingLayout`] must have been validated against and
    /// what every bounds check below uses.
    ///
    /// # Errors
    ///
    /// [`ShmemError::ZeroSized`] and [`ShmemError::TooLarge`] for a size
    /// outside `1..=`[`MAX_RESOURCE_BYTES`], and [`ShmemError::OutOfMemory`]
    /// when the host allocator refuses. Allocation failure is reported rather
    /// than aborted: a guest that asks for a ring the host cannot afford should
    /// get a failed `vkCreateRingMESA`, not a dead VMM.
    pub fn new(size: u64) -> Result<Self, ShmemError> {
        if size == 0 {
            return Err(ShmemError::ZeroSized);
        }
        if size > MAX_RESOURCE_BYTES {
            return Err(ShmemError::TooLarge {
                size,
                max: MAX_RESOURCE_BYTES,
            });
        }

        let page = page_size();
        let page_u64 = page as u64;
        let bytes = size
            .checked_next_multiple_of(page_u64)
            .and_then(|rounded| usize::try_from(rounded).ok())
            .ok_or(ShmemError::UnusableLayout {
                size,
                align: page_u64,
            })?;
        let alloc = Layout::from_size_align(bytes, page).map_err(|_: LayoutError| {
            ShmemError::UnusableLayout {
                size: bytes as u64,
                align: page_u64,
            }
        })?;

        // SAFETY: `alloc` has a non-zero size (`size >= 1` rounded up to a
        // page), which is `alloc_zeroed`'s one precondition. The returned
        // pointer is either null — handled immediately below — or the base of
        // `alloc.size()` readable, writable, zeroed bytes aligned to
        // `alloc.align()`, owned by this value until `Drop` hands the same
        // `Layout` back to `dealloc`.
        let raw = unsafe { alloc_zeroed(alloc) };
        let ptr = NonNull::new(raw).ok_or(ShmemError::OutOfMemory {
            size: bytes as u64,
            align: page_u64,
        })?;

        Ok(Self {
            ptr,
            alloc,
            declared: size,
            poisoned: AtomicBool::new(false),
        })
    }

    /// The resource size a [`RingLayout`] must have been validated against for
    /// its offsets to mean anything here.
    #[must_use]
    pub fn resource_len(&self) -> u64 {
        self.declared
    }

    /// How many bytes are actually allocated, and therefore how many are
    /// published to the guest: [`resource_len`](Self::resource_len) rounded up
    /// to [`page_size`].
    #[must_use]
    pub fn mapped_len(&self) -> u64 {
        self.alloc.size() as u64
    }

    /// The base pointer, for a renderer that wants to do its own accesses.
    ///
    /// Valid for [`mapped_len`](Self::mapped_len) bytes for as long as this
    /// value lives, and aligned to [`page_size`]. Everything done through it is
    /// outside this module's guarantees — in particular, the guest may be
    /// writing any of these bytes at any time, so a plain non-atomic read of
    /// them is a data race.
    #[must_use]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The base address as the integer [`ShmBacking::map_host`] takes.
    ///
    /// Prefer [`publish`](Self::publish), which pairs the address with the
    /// lifetime obligation that goes with it; this exists for diagnostics and
    /// for a machine layer that has its own reason to see the number.
    #[must_use]
    pub fn host_addr(&self) -> u64 {
        self.ptr.as_ptr() as usize as u64
    }

    /// Whether any accessor has been handed an offset outside
    /// [`resource_len`](Self::resource_len).
    ///
    /// Always `false` for a layout that passed [`accepts`](Self::accepts),
    /// which is the point: a `true` here means a [`RingLayout`] reached these
    /// pages without being checked against them, and the ring's results from
    /// that moment on are not to be believed. The condition is also logged at
    /// `error` level as it happens.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }

    /// Check that `layout` was validated against *these* pages.
    ///
    /// [`super::ring`] proves a layout's regions are in bounds — but only of
    /// the `resource_size` it was handed. This re-checks the same regions
    /// against the size that was actually allocated, and additionally re-checks
    /// the three control words' 4-byte alignment, because that one is a
    /// soundness precondition here (a `u32` atomic must be 4-byte aligned) and
    /// not merely a protocol rule.
    ///
    /// # Errors
    ///
    /// [`ShmemError::RegionOutsideResource`] naming the first region that does
    /// not fit, or [`ShmemError::MisalignedControlWord`] naming the first word
    /// that is not atomically accessible.
    pub fn accepts(&self, layout: &RingLayout) -> Result<(), ShmemError> {
        let regions = [
            (RingRegion::Head, Some(layout.head().region())),
            (RingRegion::Tail, Some(layout.tail().region())),
            (RingRegion::Status, Some(layout.status().region())),
            (RingRegion::Buffer, Some(layout.buffer())),
            // An absent scratch region owns no bytes and has nothing to check;
            // `RingLayout` has already discarded its offset.
            (RingRegion::Extra, layout.extra()),
        ];
        for (region, range) in regions.into_iter().filter_map(|(r, x)| x.map(|x| (r, x))) {
            if range.end() > self.declared {
                return Err(ShmemError::RegionOutsideResource {
                    region,
                    end: range.end(),
                    size: self.declared,
                });
            }
        }
        for (region, offset) in [
            (RingRegion::Head, layout.head().offset()),
            (RingRegion::Tail, layout.tail().offset()),
            (RingRegion::Status, layout.status().offset()),
        ] {
            if offset % CONTROL_WORD_LEN != 0 {
                return Err(ShmemError::MisalignedControlWord { region, offset });
            }
        }
        Ok(())
    }

    /// Check the layout against these pages and start pumping it.
    ///
    /// The only reason to build a [`RingPump`] any other way is a test that
    /// wants to drive the failure path on purpose: going through here is what
    /// makes "validated against a different allocation" a refusal with a name
    /// instead of a poisoned ring nobody notices.
    ///
    /// # Errors
    ///
    /// Whatever [`accepts`](Self::accepts) makes of the layout, then whatever
    /// [`RingPump::new`] makes of the guest's control words (wrapped in
    /// [`ShmemError::Pump`]).
    pub fn adopt(&self, layout: RingLayout) -> Result<RingPump, ShmemError> {
        self.accepts(&layout)?;
        Ok(RingPump::new(layout, self)?)
    }

    /// Put these pages in front of a guest at `offset` inside `window`.
    ///
    /// The whole allocation is published — [`mapped_len`](Self::mapped_len)
    /// bytes, a whole number of pages, every one of them ours and zeroed. The
    /// returned [`Publication`] is what keeps the pages alive; dropping it
    /// calls [`ShmBacking::unmap_host`] and only then lets the allocation go.
    ///
    /// # Errors
    ///
    /// [`ShmMapError::Unsupported`] for a window that is not
    /// [`host_mapped`](ShmBacking::host_mapped) — such a window shows its own
    /// pages and has nowhere to put ours — and whatever the machine layer says
    /// about the span otherwise.
    pub fn publish(
        self: &Arc<Self>,
        window: Arc<dyn ShmBacking>,
        offset: u64,
    ) -> Result<Publication, ShmMapError> {
        // Branch on the mode rather than discover it from an error, as
        // `ShmBacking::host_mapped`'s docs require.
        if !window.host_mapped() {
            return Err(ShmMapError::Unsupported);
        }
        let len = self.mapped_len();
        // SAFETY: `host_addr()` is the base of exactly `len` readable, writable
        // bytes — `alloc_zeroed` gave us them and nothing hands them back until
        // `Drop`. They stay mapped at that address until the matching
        // `unmap_host` returns because the `Publication` built below owns an
        // `Arc<Self>`: `Publication::drop` calls `unmap_host` and its fields —
        // including that `Arc`, and therefore the earliest possible `dealloc` —
        // are dropped only after that call has returned. A publication that is
        // leaked rather than dropped leaks the pages too, which keeps the
        // mapping valid rather than invalidating it.
        unsafe { window.map_host(offset, self.host_addr(), len)? };
        Ok(Publication {
            pages: Arc::clone(self),
            window,
            offset,
            len,
        })
    }

    /// Store one 32-bit word into the ring's `extra` scratch region, `offset`
    /// bytes into it.
    ///
    /// This is `vkWriteRingExtraMESA`: the guest asks the host to put a value
    /// somewhere it can poll for it, and picks the place. `extra` is the region
    /// [`RingLayout::extra`] handed out — passed as the `Option` it came back
    /// as, so that "the guest declared no scratch region" is answered here,
    /// once, with a name, instead of every caller inventing its own refusal for
    /// a `None` it cannot use.
    ///
    /// The two parties are told apart, because they are not the same kind of
    /// wrong:
    ///
    /// * `offset` is a **guest** number, so an offset that leaves the region or
    ///   lands off its 4-byte grid is an ordinary refused command — logged at
    ///   `debug` and *not* poisoning. See [`ShmemError`] on why that last part
    ///   matters.
    /// * that the region itself fits these pages is a **host** claim, checked
    ///   again here for the reason in the module docs: `super::ring` proved it
    ///   against a `resource_size` it was told, and if that was not this
    ///   allocation's the proof is about some other memory. That failure logs
    ///   at `error` and latches [`is_poisoned`](Self::is_poisoned), exactly as
    ///   the control-word accessor does.
    ///
    /// The store is [`SeqCst`](Ordering::SeqCst), like the control words:
    /// the guest polls this word while it runs, so it is a handshake with
    /// another agent rather than host-private state.
    ///
    /// # Errors
    ///
    /// [`ShmemError::NoExtraRegion`] when the ring declared none,
    /// [`ShmemError::ExtraWriteOutOfRange`] or
    /// [`ShmemError::MisalignedExtraWrite`] for an offset the guest chose
    /// badly, and [`ShmemError::RegionOutsideResource`] when the region is not
    /// this allocation's. Nothing is written on any of them.
    pub fn store_extra(
        &self,
        extra: Option<Region>,
        offset: u64,
        value: u32,
    ) -> Result<(), ShmemError> {
        let Some(extra) = extra else {
            tracing::debug!(
                offset,
                "a Venus guest asked the host to write a ring's extra region, \
                 but its ring declared none"
            );
            return Err(ShmemError::NoExtraRegion);
        };
        // Guest-relative first: `contains_range` is checked arithmetic, so an
        // `offset` near `u64::MAX` answers `false` rather than wrapping back
        // into the region.
        if !extra.contains_range(offset, CONTROL_WORD_LEN) {
            tracing::debug!(
                offset,
                extra_start = extra.start(),
                extra_len = extra.len(),
                "a Venus guest asked to write past the end of its ring's extra \
                 region"
            );
            return Err(ShmemError::ExtraWriteOutOfRange {
                offset,
                region_len: extra.len(),
            });
        }

        // Now the host's half of the claim. `at + 4 <= declared` is the bound
        // the store below actually needs, so it is the one checked, rather than
        // `extra.end() <= declared` plus a step of reasoning; the two coincide
        // given the check above.
        let at = extra.start().checked_add(offset).filter(|at| {
            at.checked_add(CONTROL_WORD_LEN)
                .is_some_and(|end| end <= self.declared)
                && usize::try_from(*at).is_ok()
        });
        let Some(at) = at else {
            self.poison();
            tracing::error!(
                offset,
                extra_start = extra.start(),
                extra_len = extra.len(),
                resource = self.declared,
                "a Venus ring's extra region falls outside the pages it was \
                 validated against; the layout and the allocation disagree"
            );
            return Err(ShmemError::RegionOutsideResource {
                region: RingRegion::Extra,
                end: extra.end(),
                size: self.declared,
            });
        };

        // The alignment a `u32` atomic needs, judged on the address that will
        // be formed rather than on either half of it. `super::ring` aligns
        // every region to 4, so in practice this can only fail on a guest's
        // `offset` — which is why it is not a poisoning failure.
        if at % CONTROL_WORD_LEN != 0 {
            tracing::debug!(
                offset,
                at,
                extra_start = extra.start(),
                "a Venus guest asked to write its ring's extra region off the \
                 4-byte grid an atomic store needs"
            );
            return Err(ShmemError::MisalignedExtraWrite { offset, at });
        }

        // No second `unsafe` store path: every precondition `word` checks has
        // just been established for `at` — `at + 4 <= self.declared`, `at % 4
        // == 0`, and `at` fits a `usize` — so this cannot be the `None` arm,
        // and forming the pointer stays in the one place that argues for it.
        // The arm is still written out rather than unwrapped, because nothing
        // on a guest-steered path in this crate may panic, and `word` has
        // already poisoned and logged if it is ever reached.
        let Some(slot) = self.word(at, Writer::Host) else {
            return Err(ShmemError::RegionOutsideResource {
                region: RingRegion::Extra,
                end: extra.end(),
                size: self.declared,
            });
        };
        slot.store(value, Ordering::SeqCst);
        Ok(())
    }

    /// Copy `src` into these pages at resource offset `offset`: how a Venus
    /// reply reaches the guest (EPIC 20 stage 5a.3).
    ///
    /// A reply window is a range of a host blob the guest named with
    /// `vkSetReplyCommandStreamMESA`; the caller has already bounded the write
    /// by that window, and this bounds it again by the resource, so no
    /// combination of guest numbers can reach past the allocation.
    ///
    /// The bytes are stored one [`AtomicU8`] at a time, [`Relaxed`]: the guest
    /// may be reading (or, if it is hostile, writing) the same bytes right
    /// now, and a plain `&mut [u8]` over guest-visible memory would be a data
    /// race. No ordering is needed here because none is promised here: the
    /// ring worker stores `head` [`SeqCst`](Ordering::SeqCst) only after every
    /// reply of the batch is written, and that store is what orders these
    /// bytes before the guest's acquire load of `head` (spec §5).
    ///
    /// # Errors
    ///
    /// [`ShmemError::BytesOutsideResource`] when `offset + src.len()` passes
    /// [`resource_len`](Self::resource_len); nothing is written then.
    pub fn write_bytes(&self, offset: u64, src: &[u8]) -> Result<(), ShmemError> {
        let base = self.byte_range(offset, src.len())?;
        // SAFETY: `byte_range` established `base + src.len() <=
        // self.declared <= self.alloc.size()`, so every byte touched below is
        // inside our own live allocation and `add(base + i)` stays within one
        // allocated object for every `i` in `0..src.len()`. `AtomicU8` needs no
        // alignment beyond a byte. The stores are atomic because the guest may
        // be touching these bytes concurrently; a racing relaxed atomic store
        // is defined, a plain write through a slice would not be.
        unsafe {
            let base = self.ptr.as_ptr().add(base);
            for (i, byte) in src.iter().enumerate() {
                AtomicU8::from_ptr(base.add(i)).store(*byte, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Copy `dst.len()` bytes out of these pages at resource offset `offset`,
    /// with the same bounds and the same relaxed atomic loads as
    /// [`RingBacking::read_buffer`]. The values may be anything a guest wrote.
    ///
    /// # Errors
    ///
    /// [`ShmemError::BytesOutsideResource`]; `dst` is left as it was.
    pub fn read_bytes(&self, offset: u64, dst: &mut [u8]) -> Result<(), ShmemError> {
        let base = self.byte_range(offset, dst.len())?;
        // SAFETY: as in `write_bytes`: `byte_range` keeps every byte inside
        // the live allocation, and the loads are relaxed atomics because the
        // guest may be storing to the same bytes.
        unsafe {
            let base = self.ptr.as_ptr().add(base);
            for (i, slot) in dst.iter_mut().enumerate() {
                *slot = AtomicU8::from_ptr(base.add(i)).load(Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// `offset` as a host index, once `offset + len` is proven inside the
    /// declared resource.
    fn byte_range(&self, offset: u64, len: usize) -> Result<usize, ShmemError> {
        let len64 = len as u64;
        offset
            .checked_add(len64)
            .filter(|end| *end <= self.declared)
            .and_then(|_| usize::try_from(offset).ok())
            .ok_or(ShmemError::BytesOutsideResource {
                offset,
                len: len64,
                size: self.declared,
            })
    }

    /// A test playing the guest: store a 32-bit word the way the guest's
    /// driver stores `tail`, through the same accessor the host's own words
    /// use. Refuses (returns `false`) rather than panicking on a bad offset.
    #[cfg(test)]
    pub(crate) fn guest_store_word(&self, offset: u64, value: u32) -> bool {
        match self.word(offset, Writer::Guest) {
            Some(word) => {
                word.store(value, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// A test playing the guest: load a 32-bit word (`head`, `status`).
    #[cfg(test)]
    pub(crate) fn guest_load_word(&self, offset: u64) -> Option<u32> {
        self.word(offset, Writer::Guest)
            .map(|word| word.load(Ordering::SeqCst))
    }

    /// One control word as an atomic, or `None` — loudly — if the offset is not
    /// inside these pages.
    ///
    /// The offset always came from a [`RingLayout`], so `None` means the layout
    /// was validated against some other allocation. That is a host bug, so it
    /// is logged at `error` and latched in [`is_poisoned`](Self::is_poisoned)
    /// rather than merely returned.
    ///
    /// `writer` is only a label for that log line. It is the word's own
    /// [`ControlWord::writer`](super::ring::ControlWord::writer) rather than a
    /// [`RingRegion`], because the [`RingBacking`] trait hands over a
    /// [`HostWord`] without saying whether it is `head` or `status` — and
    /// inventing a name for it would put a wrong one in the log.
    ///
    /// [`store_extra`](Self::store_extra) forms its pointer through here too,
    /// having made the same checks itself first: that keeps the argument for
    /// the `unsafe` below in one place, and is why making the scratch region
    /// writable added no `unsafe` block anywhere.
    fn word(&self, offset: u64, writer: Writer) -> Option<&AtomicU32> {
        let fits = offset
            .checked_add(CONTROL_WORD_LEN)
            .is_some_and(|end| end <= self.declared);
        let aligned = offset % CONTROL_WORD_LEN == 0;
        let byte = usize::try_from(offset).ok();
        let (true, true, Some(byte)) = (fits, aligned, byte) else {
            self.poison();
            tracing::error!(
                writer = ?writer,
                offset,
                resource = self.declared,
                "a Venus ring control word falls outside the pages it was \
                 validated against, or is misaligned in them; the layout and \
                 the allocation disagree"
            );
            return None;
        };

        // SAFETY: `byte + 4 <= self.declared <= self.alloc.size()`, so the
        // pointer and the four bytes after it are inside our own live
        // allocation, which stays put for as long as `&self` does — and that is
        // the lifetime the returned reference borrows. `self.ptr` is aligned to
        // at least `MIN_PAGE_BYTES` (4096) and `byte` is a multiple of 4, so
        // the result is 4-byte aligned, which is `align_of::<AtomicU32>()`.
        // Nothing in this module ever touches these four bytes non-atomically;
        // the guest does, from outside the Rust abstract machine, which is a
        // hazard no `unsafe` block can discharge and the reason the protocol
        // treats every value read here as untrusted.
        Some(unsafe { AtomicU32::from_ptr(self.ptr.as_ptr().add(byte).cast::<u32>()) })
    }

    /// Latch the "a layout reached us unchecked" condition.
    fn poison(&self) {
        self.poisoned.store(true, Ordering::Relaxed);
    }
}

impl fmt::Debug for RingPages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately no pointer: it is a host address, it appears in logs,
        // and printing it in every `{:?}` is free ASLR defeat for anything that
        // can read them.
        f.debug_struct("RingPages")
            .field("resource_len", &self.declared)
            .field("mapped_len", &self.mapped_len())
            .field("page_size", &self.alloc.align())
            .field("poisoned", &self.is_poisoned())
            .finish()
    }
}

impl Drop for RingPages {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` came from `alloc_zeroed(self.alloc)` in `new`, has
        // not been freed (this runs once, and `RingPages` is not `Clone`), and
        // `self.alloc` is the same `Layout` it was allocated with. No mapping
        // of these pages can still be live: the only way to publish them is
        // `publish`, which hands the resulting `Publication` an `Arc<Self>`, so
        // reaching this drop at all means every publication has already been
        // dropped and every `unmap_host` has already returned.
        unsafe { dealloc(self.ptr.as_ptr(), self.alloc) }
    }
}

impl RingPages {
    /// Atomically OR `bits` into a ring's `status` word, returning the value it
    /// held before (or [`POISON_WORD`] if the word is not inside these pages).
    ///
    /// `status` is the one control word both sides write — the host sets and
    /// clears `IDLE`, `FATAL` and `ALIVE`, the guest clears `ALIVE` with its own
    /// `atomic_fetch_and` when its watchdog arms (`vn_common.c:229-243`) — so
    /// every host write to it is a read-modify-write of named bits, never a
    /// store of a whole value. [`SeqCst`](Ordering::SeqCst), like every other
    /// control-word access here: the idle handshake is this write followed by a
    /// load of `tail`, and only a full barrier orders those two.
    ///
    /// Callable from any thread: the ring worker and the context's monitor
    /// both use it on the same word at once, which is exactly the case a
    /// read-modify-write exists for.
    pub fn set_status_bits(&self, status: &HostWord, bits: u32) -> u32 {
        self.word(status.store_offset(), status.writer())
            .map_or(POISON_WORD, |w| w.fetch_or(bits, Ordering::SeqCst))
    }

    /// Atomically clear `bits` in a ring's `status` word, returning the value
    /// it held before. The counterpart of
    /// [`set_status_bits`](Self::set_status_bits), with the same reasoning.
    pub fn clear_status_bits(&self, status: &HostWord, bits: u32) -> u32 {
        self.word(status.store_offset(), status.writer())
            .map_or(POISON_WORD, |w| w.fetch_and(!bits, Ordering::SeqCst))
    }
}

impl RingBacking for RingPages {
    fn load_host_word(&self, word: &HostWord) -> u32 {
        self.word(word.offset(), word.writer())
            .map_or(POISON_WORD, |w| w.load(Ordering::SeqCst))
    }

    fn load_guest_word(&self, word: &GuestWord) -> u32 {
        self.word(word.offset(), word.writer())
            .map_or(POISON_WORD, |w| w.load(Ordering::SeqCst))
    }

    fn store_head(&self, head: &HostWord, value: u32) {
        if let Some(w) = self.word(head.store_offset(), head.writer()) {
            w.store(value, Ordering::SeqCst);
        }
    }

    fn set_status_bits(&self, status: &HostWord, bits: u32) {
        let _previous = RingPages::set_status_bits(self, status, bits);
    }

    fn clear_status_bits(&self, status: &HostWord, bits: u32) {
        let _previous = RingPages::clear_status_bits(self, status, bits);
    }

    fn read_buffer(&self, buffer: &Region, offset: u64, dst: &mut [u8]) {
        let len = dst.len() as u64;
        // Three separate things, and the first two are the pump's promise
        // rather than ours to assume: the range stays inside the buffer region,
        // the buffer region stays inside these pages, and the absolute start
        // is a host index. The trait asks for a clamp rather than a panic if
        // the promise is ever broken, and nothing on a guest-steered path in
        // this crate may panic; zero-filling is the clamp, because a shadow of
        // zeroes decodes to nothing rather than to stale bytes from an earlier
        // batch.
        let start = buffer
            .contains_range(offset, len)
            .then(|| buffer.start().checked_add(offset))
            .flatten()
            .filter(|start| start.saturating_add(len) <= self.declared)
            .and_then(|start| usize::try_from(start).ok());
        let Some(start) = start else {
            self.poison();
            tracing::error!(
                buffer_start = buffer.start(),
                buffer_len = buffer.len(),
                offset,
                len,
                resource = self.declared,
                "a Venus ring buffer read falls outside the pages the layout \
                 was validated against"
            );
            dst.fill(0);
            return;
        };

        // SAFETY: `start + dst.len() <= self.declared <= self.alloc.size()`, so
        // every byte touched below is inside our own live allocation, and
        // `base.add(i)` therefore stays within one allocated object for every
        // `i` in `0..dst.len()`. `AtomicU8` needs no alignment beyond a byte.
        // The loads are `Relaxed` and atomic rather than a `copy_from_slice`
        // precisely because the guest may be storing to these bytes right now:
        // a racing relaxed atomic load yields an unspecified *value* and stays
        // defined, whereas reading them as a `&[u8]` would be a data race. The
        // ordering that makes the guest's writes below `tail` visible is the
        // `SeqCst` load of `tail` the pump already performed.
        unsafe {
            let base = self.ptr.as_ptr().add(start);
            for (i, slot) in dst.iter_mut().enumerate() {
                *slot = AtomicU8::from_ptr(base.add(i)).load(Ordering::Relaxed);
            }
        }
    }
}

/// What an accessor answers when its offset is not inside these pages.
///
/// `u32::MAX` rather than zero, because the two words it can be returned for
/// both refuse it: a `head` or `status` of `u32::MAX` fails
/// [`RingPump::new`]'s zeroed check, and a `tail` of `u32::MAX` is, for any
/// cursor more than a bufferful below the top of the range, an impossible claim
/// that marks the ring fatal. Neither is the real guard — that is
/// [`RingPages::accepts`] — and the log line and
/// [`RingPages::is_poisoned`] are what a host bug is actually found by.
const POISON_WORD: u32 = u32::MAX;

/// Proof that a set of [`RingPages`] is currently in front of a guest.
///
/// Owns an `Arc` of the pages, so they cannot be freed while this exists;
/// dropping it calls [`ShmBacking::unmap_host`] and only then releases that
/// `Arc`. There is no way to take a mapping down without dropping this, and no
/// way to build one except [`RingPages::publish`].
pub struct Publication {
    /// The keep-alive. Dropped *after* `Drop::drop` has unmapped.
    pages: Arc<RingPages>,
    window: Arc<dyn ShmBacking>,
    offset: u64,
    len: u64,
}

impl Publication {
    /// The pages this publication is keeping alive.
    #[must_use]
    pub fn pages(&self) -> &Arc<RingPages> {
        &self.pages
    }

    /// Where inside the shared-memory window the pages were placed.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// How many bytes were published: always [`RingPages::mapped_len`].
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Always `false`; present because clippy asks for it beside
    /// [`len`](Self::len). A zero-length publication cannot be built —
    /// [`RingPages::new`] refuses a zero-sized resource.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Publication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publication")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .field("pages", &self.pages)
            .finish()
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        // The order is the whole safety argument: take the pages out of the
        // guest first, and only then let `self.pages` — the last thing standing
        // between the mapping and `dealloc` — be dropped with the rest of the
        // fields. `unmap_host` is infallible by design, so there is no path
        // where this returns with the mapping still up.
        self.window.unmap_host(self.offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venus::pump::{Batch, Consumed, Pass, RingSink};
    use crate::venus::ring::RingCreateInfo;

    use std::sync::Mutex;
    use virtio_core::ShmAccessError;

    /// The resource every fixture lives in: one 4 KiB page exactly, so a
    /// rounded-up allocation and a declared size coincide on both hosts and the
    /// "validated against a different size" test has somewhere to go wrong.
    const RESOURCE: u64 = 0x1000;
    /// A deliberately tiny command buffer, so a wrap is a few bytes away.
    const BUFFER: u64 = 64;

    fn base_info() -> RingCreateInfo {
        RingCreateInfo {
            flags: 0,
            resource_id: 3,
            offset: 0,
            size: RESOURCE,
            idle_timeout_ns: 1_000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 16,
            buffer_size: BUFFER,
            extra_offset: 16 + BUFFER,
            extra_size: 4,
        }
    }

    fn layout() -> RingLayout {
        RingLayout::new(base_info(), RESOURCE).expect("fixture layout is valid")
    }

    // ------------------------------------------------- the guest, played by hand

    /// Store a `u32` the way the guest does: an aligned atomic write to a word
    /// the host only reads.
    fn guest_store(pages: &RingPages, offset: u64, value: u32) {
        let byte = usize::try_from(offset).expect("fixture offset fits a usize");
        assert!(byte + 4 <= pages.mapped_len() as usize);
        assert_eq!(offset % 4, 0);
        // SAFETY: the assertions above put the four bytes inside the live
        // allocation and on a 4-byte boundary, and `pages` outlives the borrow.
        unsafe {
            AtomicU32::from_ptr(pages.as_ptr().add(byte).cast::<u32>())
                .store(value, Ordering::SeqCst);
        }
    }

    /// Write command bytes the way the guest does.
    fn guest_write(pages: &RingPages, offset: u64, bytes: &[u8]) {
        let byte = usize::try_from(offset).expect("fixture offset fits a usize");
        assert!(byte + bytes.len() <= pages.mapped_len() as usize);
        // SAFETY: the assertion above puts every written byte inside the live
        // allocation, and `pages` outlives the borrow.
        unsafe {
            let base = pages.as_ptr().add(byte);
            for (i, value) in bytes.iter().enumerate() {
                AtomicU8::from_ptr(base.add(i)).store(*value, Ordering::Relaxed);
            }
        }
    }

    /// Read raw bytes back out, bypassing every bounds check in the module, so
    /// a test can assert what actually landed where.
    fn peek(pages: &RingPages, offset: u64, len: usize) -> Vec<u8> {
        let byte = usize::try_from(offset).expect("fixture offset fits a usize");
        assert!(byte + len <= pages.mapped_len() as usize);
        // SAFETY: the assertion above keeps the read inside the live
        // allocation; no other thread touches these pages during a test, so a
        // plain read is not racing anything.
        unsafe { std::slice::from_raw_parts(pages.as_ptr().add(byte), len).to_vec() }
    }

    // ----------------------------------------------------- a recording window

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Map { offset: u64, addr: u64, len: u64 },
        Unmap { offset: u64 },
    }

    /// A [`ShmBacking`] that records what it was told and does nothing else. No
    /// hypervisor, no guest — the whole point of `map_host` taking an address
    /// rather than a descriptor is that this is testable on any host.
    #[derive(Debug, Default)]
    struct Window {
        events: Mutex<Vec<Event>>,
        host_mapped: bool,
        refuse: bool,
    }

    impl Window {
        fn host_mapped() -> Arc<Self> {
            Arc::new(Self {
                host_mapped: true,
                ..Self::default()
            })
        }

        fn events(&self) -> Vec<Event> {
            self.events
                .lock()
                .expect("no test panics while holding it")
                .clone()
        }
    }

    impl ShmBacking for Window {
        fn len(&self) -> u64 {
            1 << 20
        }

        fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
            Err(ShmAccessError {
                offset,
                len: buf.len() as u64,
                window: self.len(),
            })
        }

        fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
            Err(ShmAccessError {
                offset,
                len: data.len() as u64,
                window: self.len(),
            })
        }

        fn fill(&self, offset: u64, len: u64, _byte: u8) -> Result<(), ShmAccessError> {
            Err(ShmAccessError {
                offset,
                len,
                window: self.len(),
            })
        }

        fn host_mapped(&self) -> bool {
            self.host_mapped
        }

        unsafe fn map_host(
            &self,
            offset: u64,
            host_addr: u64,
            len: u64,
        ) -> Result<(), ShmMapError> {
            if self.refuse {
                return Err(ShmMapError::Refused("the fixture said no".into()));
            }
            self.events.lock().expect("uncontended").push(Event::Map {
                offset,
                addr: host_addr,
                len,
            });
            Ok(())
        }

        fn unmap_host(&self, offset: u64) {
            self.events
                .lock()
                .expect("uncontended")
                .push(Event::Unmap { offset });
        }
    }

    /// A sink that takes everything and remembers what it saw.
    #[derive(Debug, Default)]
    struct Greedy {
        seen: Vec<Vec<u8>>,
    }

    impl RingSink for Greedy {
        fn consume(&mut self, batch: Batch<'_>) -> Consumed {
            self.seen.push(batch.bytes().to_vec());
            batch.all()
        }
    }

    // ------------------------------------------------------------ the allocation

    #[test]
    fn the_host_page_size_is_a_power_of_two_no_smaller_than_a_hypervisor_page() {
        let page = page_size();
        assert!(page.is_power_of_two(), "{page} is not a power of two");
        assert!(page >= MIN_PAGE_BYTES, "{page} is below a hypervisor page");
        // Queried once, cached, and stable — a page size that changed between
        // the allocation and the mapping would be a very quiet disaster.
        assert_eq!(page, page_size());
    }

    #[test]
    fn the_base_is_page_aligned_and_every_byte_starts_at_zero() {
        // Sizes that do and do not land on a page boundary, including one that
        // is smaller than a page and one that is a page plus a byte.
        for size in [1u64, 4, 0x1000, 0x1001, 0x2000, 0x3fff] {
            let pages = RingPages::new(size).expect("a modest allocation");
            let page = page_size() as u64;

            assert_eq!(
                pages.host_addr() % page,
                0,
                "a {size:#x}-byte resource is not page-aligned"
            );
            assert_eq!(pages.resource_len(), size);
            assert_eq!(pages.mapped_len() % page, 0);
            assert!(pages.mapped_len() >= size);
            // Rounded *up*, never down: the guest must never be shown a page
            // whose tail belongs to somebody else's allocation.
            assert!(pages.mapped_len() < size + page);

            let all = peek(&pages, 0, pages.mapped_len() as usize);
            assert!(
                all.iter().all(|b| *b == 0),
                "a {size:#x}-byte resource came back with non-zero bytes"
            );
            assert!(!pages.is_poisoned());
        }
    }

    #[test]
    fn a_resource_size_outside_the_limits_is_refused_rather_than_allocated() {
        let refuse = |size: u64| match RingPages::new(size) {
            Ok(_) => panic!("a {size:#x}-byte resource was allocated"),
            Err(err) => err,
        };
        assert_eq!(refuse(0), ShmemError::ZeroSized);
        for size in [
            MAX_RESOURCE_BYTES + 1,
            MAX_RESOURCE_BYTES * 2,
            1 << 40,
            u64::MAX,
        ] {
            assert_eq!(
                refuse(size),
                ShmemError::TooLarge {
                    size,
                    max: MAX_RESOURCE_BYTES
                },
                "a {size:#x}-byte resource"
            );
        }
        // Exactly the limit is allowed — the bound is a limit, not a margin.
        let at_limit = RingPages::new(MAX_RESOURCE_BYTES).expect("the limit is allocatable");
        assert_eq!(at_limit.resource_len(), MAX_RESOURCE_BYTES);
    }

    // ------------------------------------------------------------- the atomics

    #[test]
    fn every_control_word_of_a_real_layout_round_trips_at_its_own_offset() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let layout = layout();
        pages
            .accepts(&layout)
            .expect("the layout is this resource's");

        // `head`: stored through the type-stated store path, read back both
        // through the loader and out of the raw bytes, so a store to the wrong
        // offset cannot hide behind a load from the same wrong one.
        let head = layout.head();
        for value in [0x1234_5678u32, u32::MAX, 0] {
            pages.store_head(&head, value);
            assert_eq!(pages.load_host_word(&head), value);
            assert_eq!(
                peek(&pages, head.offset(), 4),
                value.to_le_bytes(),
                "head did not land where it was addressed"
            );
        }

        // `status`: never stored, only read-modify-written, bit by bit — and
        // each write returns what the word held before it.
        let status = layout.status();
        assert_eq!(pages.set_status_bits(&status, 0x3), 0);
        assert_eq!(pages.set_status_bits(&status, 0x4), 0x3);
        assert_eq!(pages.clear_status_bits(&status, 0x1), 0x7);
        assert_eq!(pages.load_host_word(&status), 0x6);
        assert_eq!(peek(&pages, status.offset(), 4), 0x6u32.to_le_bytes());
        assert_eq!(pages.clear_status_bits(&status, u32::MAX), 0x6);
        assert_eq!(pages.load_host_word(&status), 0);

        // The guest's word: written the way the guest writes it, loaded the way
        // the host loads it. There is no `store_guest_word` and there cannot
        // be — `GuestWord` has no `store_offset` — which is the property this
        // assertion is standing next to rather than testing.
        for value in [0u32, 1, 0xdead_beef, u32::MAX] {
            guest_store(&pages, layout.tail().offset(), value);
            assert_eq!(pages.load_guest_word(&layout.tail()), value);
        }

        // Three distinct words, not one aliased three ways.
        pages.store_head(&layout.head(), 0xaaaa_aaaa);
        pages.set_status_bits(&layout.status(), 0xbbbb_bbbb);
        guest_store(&pages, layout.tail().offset(), 0xcccc_cccc);
        assert_eq!(pages.load_host_word(&layout.head()), 0xaaaa_aaaa);
        assert_eq!(pages.load_guest_word(&layout.tail()), 0xcccc_cccc);
        assert_eq!(pages.load_host_word(&layout.status()), 0xbbbb_bbbb);
        assert!(!pages.is_poisoned());
    }

    /// Clear bits of a word the way the guest's watchdog does
    /// (`vn_ring_unset_status_bits`): its own `atomic_fetch_and`.
    fn guest_clear_bits(pages: &RingPages, offset: u64, bits: u32) {
        let byte = usize::try_from(offset).expect("fixture offset fits a usize");
        assert!(byte + 4 <= pages.mapped_len() as usize);
        assert_eq!(offset % 4, 0);
        // SAFETY: as `guest_store` — the assertions put the four bytes inside
        // the live allocation on a 4-byte boundary, and `pages` outlives the
        // borrow.
        unsafe {
            AtomicU32::from_ptr(pages.as_ptr().add(byte).cast::<u32>())
                .fetch_and(!bits, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_guest_clearing_alive_and_the_host_toggling_idle_lose_no_bit() {
        use crate::venus::pump::{STATUS_ALIVE, STATUS_FATAL, STATUS_IDLE};

        let pages = RingPages::new(RESOURCE).expect("a page");
        let status = layout().status();
        let at = status.offset();
        let read = |p: &RingPages| p.load_host_word(&status);

        // Deterministic first, one interleaving at a time: the monitor asserts
        // ALIVE, the ring worker publishes IDLE, the guest arms its watchdog,
        // the worker wakes. A host that stored a whole word from its own
        // picture would get one of these wrong.
        pages.set_status_bits(&status, STATUS_ALIVE);
        pages.set_status_bits(&status, STATUS_IDLE);
        assert_eq!(read(&pages), STATUS_ALIVE | STATUS_IDLE);
        guest_clear_bits(&pages, at, STATUS_ALIVE);
        assert_eq!(read(&pages), STATUS_IDLE);
        pages.clear_status_bits(&status, STATUS_IDLE);
        assert_eq!(read(&pages), 0, "clearing IDLE must not bring ALIVE back");
        pages.set_status_bits(&status, STATUS_FATAL);
        pages.set_status_bits(&status, STATUS_ALIVE);
        assert_eq!(read(&pages), STATUS_FATAL | STATUS_ALIVE);

        // Then concurrently. The "host" toggles IDLE a great many times while
        // the "guest" keeps clearing ALIVE and the "monitor" keeps setting it.
        // FATAL was published before any of them started and must survive all
        // of it, and the host's own bit must end where the host left it.
        let pages = Arc::new(pages);
        let rounds = 20_000;
        std::thread::scope(|scope| {
            let host = {
                let pages = Arc::clone(&pages);
                scope.spawn(move || {
                    for _ in 0..rounds {
                        pages.set_status_bits(&status, STATUS_IDLE);
                        pages.clear_status_bits(&status, STATUS_IDLE);
                    }
                    pages.set_status_bits(&status, STATUS_IDLE);
                })
            };
            let guest = {
                let pages = Arc::clone(&pages);
                scope.spawn(move || {
                    for _ in 0..rounds {
                        guest_clear_bits(&pages, at, STATUS_ALIVE);
                        assert_ne!(
                            pages.load_host_word(&status) & STATUS_FATAL,
                            0,
                            "FATAL was lost under the guest's clear"
                        );
                    }
                })
            };
            let monitor = {
                let pages = Arc::clone(&pages);
                scope.spawn(move || {
                    for _ in 0..rounds {
                        let before = pages.set_status_bits(&status, STATUS_ALIVE);
                        assert_ne!(before & STATUS_FATAL, 0, "FATAL was lost");
                    }
                })
            };
            host.join().expect("host thread");
            guest.join().expect("guest thread");
            monitor.join().expect("monitor thread");
        });
        let end = pages.load_host_word(&status);
        assert_eq!(end & STATUS_FATAL, STATUS_FATAL, "FATAL survived");
        assert_eq!(
            end & STATUS_IDLE,
            STATUS_IDLE,
            "the host's last word stands"
        );

        // And with everyone stopped, one more of each lands exactly.
        guest_clear_bits(&pages, at, STATUS_ALIVE);
        assert_eq!(pages.load_host_word(&status) & STATUS_ALIVE, 0);
        pages.set_status_bits(&status, STATUS_ALIVE);
        assert_eq!(
            pages.load_host_word(&status),
            STATUS_FATAL | STATUS_IDLE | STATUS_ALIVE
        );
    }

    #[test]
    fn a_buffer_read_spanning_the_region_is_exact_and_one_past_it_is_refused() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let layout = layout();
        let buffer = layout.buffer();

        let content: Vec<u8> = (0..BUFFER).map(|i| (i as u8).wrapping_mul(7)).collect();
        guest_write(&pages, buffer.start(), &content);

        // The whole region, in one read, from the first byte to the last.
        let mut dst = vec![0xffu8; BUFFER as usize];
        pages.read_buffer(&buffer, 0, &mut dst);
        assert_eq!(dst, content);

        // A read at an offset, ending exactly on the region's last byte — the
        // boundary case the pump's wrap splice produces on every lap.
        let mut tail = vec![0xffu8; 7];
        pages.read_buffer(&buffer, BUFFER - 7, &mut tail);
        assert_eq!(tail, content[BUFFER as usize - 7..]);

        // A zero-length read is legal and touches nothing.
        pages.read_buffer(&buffer, BUFFER, &mut []);
        assert!(!pages.is_poisoned());

        // And now the ones that leave the region. The pump promises it never
        // asks for these, so each is a host bug — zero-filled, logged, latched,
        // and above all not a read of the bytes past the buffer.
        let sentinel: Vec<u8> = (0..16).map(|i| 0xa0 | i).collect();
        guest_write(&pages, buffer.end(), &sentinel);
        for (offset, len) in [
            (BUFFER, 1u64),
            (BUFFER - 3, 4),
            (0, BUFFER + 1),
            (u64::MAX, 1),
        ] {
            let fresh = RingPages::new(RESOURCE).expect("a page");
            guest_write(&fresh, buffer.start(), &content);
            guest_write(&fresh, buffer.end(), &sentinel);

            let mut dst = vec![0xffu8; len as usize];
            fresh.read_buffer(&buffer, offset, &mut dst);
            assert!(
                dst.iter().all(|b| *b == 0),
                "a read of {len} at {offset:#x} was not zero-filled: {dst:?}"
            );
            assert!(
                fresh.is_poisoned(),
                "a read of {len} at {offset:#x} was not latched as a host bug"
            );
        }
    }

    // --------------------------------------------------------- the extra region

    /// A roomier scratch region than [`base_info`]'s single word: four slots,
    /// so "the last aligned one" is a different offset from "the first".
    const EXTRA: u64 = 16;

    fn roomy_extra() -> RingLayout {
        RingLayout::new(
            RingCreateInfo {
                extra_size: EXTRA,
                ..base_info()
            },
            RESOURCE,
        )
        .expect("four words of scratch fit the fixture resource")
    }

    #[test]
    fn a_word_stored_in_the_extra_region_lands_where_the_guest_polls_for_it() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let layout = roomy_extra();
        pages
            .accepts(&layout)
            .expect("the layout is this resource's");
        let extra = layout.extra().expect("the fixture declares scratch");

        // Every aligned slot, including — and this is the boundary that must be
        // *accepted* — the four bytes ending on the region's last one.
        let written = [
            (0u64, 0xfeed_faceu32),
            (4, 1),
            (8, 0),
            (EXTRA - CONTROL_WORD_LEN, u32::MAX),
        ];
        for (offset, value) in written {
            pages
                .store_extra(layout.extra(), offset, value)
                .unwrap_or_else(|err| panic!("an aligned slot at {offset:#x}: {err}"));
            assert_eq!(
                peek(&pages, extra.start() + offset, 4),
                value.to_le_bytes(),
                "the word written at {offset:#x} did not land where it was addressed"
            );
        }
        // Four distinct slots, not one aliased four ways — checked after all
        // four stores rather than during, so a store to the wrong slot cannot
        // hide behind the read that follows it.
        for (offset, value) in written {
            assert_eq!(
                peek(&pages, extra.start() + offset, 4),
                value.to_le_bytes(),
                "the word at {offset:#x} was overwritten by a later store"
            );
        }
        assert_eq!(
            peek(&pages, extra.end() - 4, 4),
            u32::MAX.to_le_bytes(),
            "the last slot is the one ending on the region's last byte"
        );

        // Nothing spilled past the region, and nothing reached the control
        // words the pump owns.
        assert_eq!(peek(&pages, extra.end(), 16), vec![0u8; 16]);
        assert_eq!(peek(&pages, 0, 12), vec![0u8; 12]);
        assert!(!pages.is_poisoned());
    }

    #[test]
    fn an_extra_write_the_guest_aimed_badly_is_refused_without_poisoning_the_pages() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let layout = roomy_extra();
        let extra = layout.extra().expect("the fixture declares scratch");

        // Past the end: ending one byte too far, starting a whole slot too far,
        // and two whose end exists only modulo 2^64.
        for offset in [EXTRA - 3, EXTRA, EXTRA + 4, u64::MAX - 3, u64::MAX] {
            assert_eq!(
                pages.store_extra(layout.extra(), offset, 0xdead_beef),
                Err(ShmemError::ExtraWriteOutOfRange {
                    offset,
                    region_len: EXTRA,
                }),
                "an extra write at {offset:#x}"
            );
        }

        // Inside the region but off the 4-byte grid an atomic store needs.
        for offset in [1u64, 2, 3, 5, EXTRA - 5] {
            assert_eq!(
                pages.store_extra(layout.extra(), offset, 0xdead_beef),
                Err(ShmemError::MisalignedExtraWrite {
                    offset,
                    at: extra.start() + offset,
                }),
                "an extra write at {offset:#x}"
            );
        }

        // Not one byte of any of that was written, and nothing was latched: a
        // guest choosing a bad offset is a refused command, not a host bug, and
        // `is_poisoned` would stop meaning anything if the guest could set it.
        assert_eq!(
            peek(&pages, 0, RESOURCE as usize),
            vec![0u8; RESOURCE as usize]
        );
        assert!(!pages.is_poisoned());
    }

    #[test]
    fn a_ring_that_declared_no_scratch_region_is_refused_rather_than_written() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let barren = RingLayout::new(
            RingCreateInfo {
                extra_offset: 0,
                extra_size: 0,
                ..base_info()
            },
            RESOURCE,
        )
        .expect("a ring may decline scratch");
        assert_eq!(barren.extra(), None, "a zero-length region is no region");

        for offset in [0u64, 4, u64::MAX] {
            assert_eq!(
                pages.store_extra(barren.extra(), offset, 0x1234),
                Err(ShmemError::NoExtraRegion),
                "an extra write at {offset:#x} on a ring with no extra region"
            );
        }
        // In particular, offset 0 of a region that does not exist is not offset
        // 0 of the resource, which is where `head` lives.
        assert_eq!(
            peek(&pages, 0, RESOURCE as usize),
            vec![0u8; RESOURCE as usize]
        );
        assert!(!pages.is_poisoned());
    }

    #[test]
    fn an_extra_region_validated_against_a_different_allocation_poisons_rather_than_storing() {
        // Scratch that is perfectly valid — in a 16 KiB resource.
        let bigger = RingCreateInfo {
            size: 0x4000,
            buffer_offset: 0x2000,
            buffer_size: BUFFER,
            extra_offset: 0x3000,
            extra_size: EXTRA,
            ..base_info()
        };
        let elsewhere = RingLayout::new(bigger, 0x4000).expect("valid in a 16 KiB resource");
        let pages = RingPages::new(RESOURCE).expect("a page");

        // The offset is impeccable; the region is not this allocation's, which
        // is ours to get wrong and therefore poisons.
        assert_eq!(
            pages.store_extra(elsewhere.extra(), 0, 0xdead_beef),
            Err(ShmemError::RegionOutsideResource {
                region: RingRegion::Extra,
                end: 0x3000 + EXTRA,
                size: RESOURCE,
            })
        );
        assert!(pages.is_poisoned());
        assert_eq!(
            peek(&pages, 0, RESOURCE as usize),
            vec![0u8; RESOURCE as usize]
        );

        // The same region against pages that are big enough is an ordinary
        // store, which is what makes this about the pair rather than about the
        // layout.
        let roomy = RingPages::new(0x4000).expect("four pages");
        roomy
            .store_extra(elsewhere.extra(), EXTRA - 4, 0x5eed)
            .expect("its own resource");
        assert_eq!(peek(&roomy, 0x3000 + EXTRA - 4, 4), 0x5eedu32.to_le_bytes());
        assert!(!roomy.is_poisoned());
    }

    // ------------------------------------------------ the size it was validated against

    #[test]
    fn a_layout_validated_against_a_different_size_is_refused_by_name() {
        // A layout that is perfectly valid — in a 16 KiB resource. These pages
        // are 4 KiB, so `ring.rs`'s proof simply does not apply to them.
        let bigger = RingCreateInfo {
            size: 0x4000,
            buffer_offset: 0x2000,
            buffer_size: BUFFER,
            extra_offset: 0x3000,
            extra_size: 4,
            ..base_info()
        };
        let elsewhere = RingLayout::new(bigger, 0x4000).expect("valid in a 16 KiB resource");
        let pages = RingPages::new(RESOURCE).expect("a page");

        assert_eq!(
            pages.accepts(&elsewhere),
            Err(ShmemError::RegionOutsideResource {
                region: RingRegion::Buffer,
                end: 0x2000 + BUFFER,
                size: RESOURCE,
            })
        );
        assert!(matches!(
            pages.adopt(elsewhere),
            Err(ShmemError::RegionOutsideResource { .. })
        ));
        // `accepts` refusing is not a use of the pages, so nothing is latched —
        // the check happened before anything was indexed.
        assert!(!pages.is_poisoned());

        // The same layout against pages that *are* big enough is fine, which is
        // what makes this about the pair rather than about the layout.
        let roomy = RingPages::new(0x4000).expect("four pages");
        roomy.accepts(&elsewhere).expect("its own resource");

        // Every region can be the one that does not fit, and each is named.
        for (region, info) in [
            (
                RingRegion::Head,
                RingCreateInfo {
                    head_offset: 0x1000,
                    ..bigger
                },
            ),
            (
                RingRegion::Tail,
                RingCreateInfo {
                    tail_offset: 0x1004,
                    ..bigger
                },
            ),
            (
                RingRegion::Status,
                RingCreateInfo {
                    status_offset: 0x1008,
                    ..bigger
                },
            ),
            (
                RingRegion::Extra,
                RingCreateInfo {
                    extra_offset: 0x3ffc,
                    ..bigger
                },
            ),
        ] {
            let layout = RingLayout::new(info, 0x4000).expect("valid in a 16 KiB resource");
            let err = pages.accepts(&layout).expect_err("not in a 4 KiB resource");
            // The head/tail/status cases report themselves; the extra case
            // reports whichever region is first past the end, which is the
            // buffer in this fixture. Only assert the one under test when it is
            // the first to fail.
            if let ShmemError::RegionOutsideResource { region: got, .. } = err {
                assert!(
                    got == region || got == RingRegion::Buffer,
                    "{region} produced {err}"
                );
            } else {
                panic!("{region}: expected an out-of-resource refusal, got {err}");
            }
        }
    }

    #[test]
    fn a_layout_that_never_passed_accepts_poisons_rather_than_reads_out_of_bounds() {
        // The failure path the infallible `RingBacking` methods exist to
        // survive: a `RingPump` built directly, skipping `adopt`.
        let bigger = RingCreateInfo {
            size: 0x4000,
            head_offset: 0x2000,
            tail_offset: 0x2004,
            status_offset: 0x2008,
            buffer_offset: 0x3000,
            ..base_info()
        };
        let stray = RingLayout::new(bigger, 0x4000).expect("valid in a 16 KiB resource");
        let pages = RingPages::new(RESOURCE).expect("a page");

        // Nothing panics, nothing is read from outside the allocation, and the
        // value handed back is one the pump refuses.
        assert_eq!(pages.load_host_word(&stray.head()), POISON_WORD);
        assert!(pages.is_poisoned());
        assert_eq!(pages.load_guest_word(&stray.tail()), POISON_WORD);
        pages.store_head(&stray.head(), 0x1234);
        assert_eq!(pages.set_status_bits(&stray.status(), 0x1234), POISON_WORD);
        assert_eq!(
            pages.clear_status_bits(&stray.status(), 0x1234),
            POISON_WORD
        );

        // And the pump does refuse it, rather than adopting a ring whose words
        // are nowhere.
        assert!(matches!(
            RingPump::new(stray, &pages),
            Err(PumpError::ControlWordsNotZeroed { .. })
        ));
    }

    // -------------------------------------------------------------- publication

    #[test]
    fn publication_and_takedown_are_recorded_in_that_order() {
        let pages = Arc::new(RingPages::new(RESOURCE).expect("a page"));
        let window = Window::host_mapped();

        let published = pages
            .publish(Arc::clone(&window) as Arc<dyn ShmBacking>, 0x8000)
            .expect("a host-mapped window takes renderer pages");
        assert_eq!(published.offset(), 0x8000);
        assert_eq!(published.len(), pages.mapped_len());
        assert!(!published.is_empty());
        assert_eq!(
            window.events(),
            vec![Event::Map {
                offset: 0x8000,
                addr: pages.host_addr(),
                len: pages.mapped_len(),
            }]
        );

        drop(published);
        assert_eq!(
            window.events(),
            vec![
                Event::Map {
                    offset: 0x8000,
                    addr: pages.host_addr(),
                    len: pages.mapped_len(),
                },
                Event::Unmap { offset: 0x8000 },
            ]
        );
    }

    #[test]
    fn a_window_that_shows_its_own_pages_has_nowhere_to_put_ours() {
        let pages = Arc::new(RingPages::new(RESOURCE).expect("a page"));
        let plain = Arc::new(Window::default());
        assert_eq!(
            pages
                .publish(Arc::clone(&plain) as Arc<dyn ShmBacking>, 0)
                .expect_err("not a host-mapped window"),
            ShmMapError::Unsupported
        );
        // Refused before the address was handed over, so there is nothing to
        // take back down.
        assert!(plain.events().is_empty());

        // A window that refuses the span itself leaves nothing behind either.
        let picky = Arc::new(Window {
            host_mapped: true,
            refuse: true,
            ..Window::default()
        });
        assert!(matches!(
            pages.publish(Arc::clone(&picky) as Arc<dyn ShmBacking>, 0),
            Err(ShmMapError::Refused(_))
        ));
        assert!(picky.events().is_empty());
    }

    #[test]
    fn the_pages_cannot_be_freed_while_a_guest_can_still_see_them() {
        let pages = Arc::new(RingPages::new(RESOURCE).expect("a page"));
        let window = Window::host_mapped();
        let addr = pages.host_addr();

        let published = pages
            .publish(Arc::clone(&window) as Arc<dyn ShmBacking>, 0)
            .expect("published");
        assert_eq!(Arc::strong_count(&pages), 2);

        // Drop every handle the caller has. This is the mistake the type is
        // built against — and it is not a mistake that can free the pages,
        // because the publication holds the same allocation. Rust will not even
        // let the attempt compile as a move out of a live `Arc`:
        //
        //     let owned = Arc::try_unwrap(pages);   // Err: the mapping holds one
        let reclaimed = Arc::try_unwrap(pages).map(|_| ());
        assert!(
            reclaimed.is_err(),
            "the allocation was reclaimable while it was mapped into a guest"
        );
        let pages = reclaimed.expect_err("still published");
        drop(pages);

        // The publication still holds live, usable pages at the same address.
        assert_eq!(Arc::strong_count(published.pages()), 1);
        assert_eq!(published.pages().host_addr(), addr);
        let layout = layout();
        published.pages().store_head(&layout.head(), 0x5eed);
        assert_eq!(published.pages().load_host_word(&layout.head()), 0x5eed);

        // Only dropping the publication unmaps — and the allocation outlives
        // that call, because `Drop::drop` runs before the `Arc` field does.
        drop(published);
        assert_eq!(window.events().last(), Some(&Event::Unmap { offset: 0 }));
    }

    // ------------------------------------------------------------ end to end

    #[test]
    fn a_pump_runs_over_real_pages_with_a_guest_played_by_hand() {
        let pages = Arc::new(RingPages::new(RESOURCE).expect("a page"));
        let window = Window::host_mapped();
        let published = pages
            .publish(Arc::clone(&window) as Arc<dyn ShmBacking>, 0)
            .expect("published");

        let layout = layout();
        let mut pump = pages.adopt(layout).expect("a zeroed ring on its own pages");
        let mut sink = Greedy::default();
        assert_eq!(pump.pump(&*pages, &mut sink), Ok(Pass::Idle));

        // The guest produces eleven bytes and rings no doorbell.
        guest_write(&pages, layout.buffer().start(), b"hello venus");
        guest_store(&pages, layout.tail().offset(), 11);
        assert_eq!(
            pump.pump(&*pages, &mut sink),
            Ok(Pass::Progress {
                offered: 11,
                consumed: 11
            })
        );
        assert_eq!(sink.seen, vec![b"hello venus".to_vec()]);
        // `head` is republished into the bytes the guest is polling, not just
        // into the pump's own mirror.
        assert_eq!(peek(&pages, layout.head().offset(), 4), 11u32.to_le_bytes());

        // A batch that wraps the end of the buffer, spliced back together out
        // of two reads of these pages.
        let buffer = layout.buffer();
        guest_write(&pages, buffer.start() + 11, &[9u8; 53]);
        guest_store(&pages, layout.tail().offset(), BUFFER as u32);
        assert!(matches!(
            pump.pump(&*pages, &mut sink),
            Ok(Pass::Progress { .. })
        ));
        let wrapped: Vec<u8> = (200u8..210).collect();
        guest_write(&pages, buffer.start() + 60, &wrapped[..4]);
        guest_write(&pages, buffer.start(), &wrapped[4..]);
        guest_store(&pages, layout.tail().offset(), BUFFER as u32 + 6);
        assert_eq!(
            pump.pump(&*pages, &mut sink),
            Ok(Pass::Progress {
                offered: 6,
                consumed: 6
            })
        );
        assert_eq!(sink.seen.last(), Some(&wrapped[4..].to_vec()));

        // A hostile tail: more bytes than the ring can hold. The ring is
        // written off and the guest is told in the bytes it polls.
        guest_store(&pages, layout.tail().offset(), 0x7fff_ffff);
        assert!(matches!(
            pump.pump(&*pages, &mut sink),
            Err(PumpError::TailOutOfRange { .. })
        ));
        let status = u32::from_le_bytes(
            peek(&pages, layout.status().offset(), 4)
                .try_into()
                .expect("four bytes"),
        );
        assert_eq!(
            status & crate::venus::pump::STATUS_FATAL,
            crate::venus::pump::STATUS_FATAL
        );

        // Nothing in any of that was out of bounds.
        assert!(!pages.is_poisoned());
        drop(published);
    }

    #[test]
    fn a_reset_zeroes_the_words_the_guest_polls() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let layout = layout();
        let mut pump = pages.adopt(layout).expect("adopted");

        guest_write(&pages, layout.buffer().start(), b"abcd");
        guest_store(&pages, layout.tail().offset(), 4);
        assert!(matches!(
            pump.pump(&pages, &mut Greedy::default()),
            Ok(Pass::Progress { .. })
        ));
        assert_ne!(peek(&pages, layout.head().offset(), 4), [0; 4]);

        // ADR-0005: a reboot that leaves a stale `head` in shared memory is
        // exactly the haunting the rule is about, and the store has to reach
        // the bytes rather than the pump's mirror of them.
        pump.reset(&pages);
        assert_eq!(peek(&pages, layout.head().offset(), 4), [0; 4]);
        assert_eq!(peek(&pages, layout.status().offset(), 4), [0; 4]);
        assert!(!pages.is_poisoned());
    }

    #[test]
    fn the_debug_rendering_does_not_leak_the_host_address() {
        let pages = RingPages::new(RESOURCE).expect("a page");
        let rendered = format!("{pages:?}");
        assert!(rendered.contains("resource_len"));
        assert!(
            !rendered.contains(&format!("{:x}", pages.host_addr())),
            "the host address appeared in a Debug rendering: {rendered}"
        );
    }
}
