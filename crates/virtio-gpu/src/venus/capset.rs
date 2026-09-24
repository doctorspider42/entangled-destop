//! `struct virgl_renderer_capset_venus` — the 160 bytes a guest reads before
//! it will speak Venus at all (ADR-0004, EPIC 20).
//!
//! The guest's mesa `venus` ICD issues `GET_CAPSET` for
//! [`CAPSET_VENUS`](crate::CAPSET_VENUS) during *driver load*, long before any
//! Vulkan object exists, and compares what comes back against the protocol it
//! was compiled for. If the answer does not satisfy it, it declines to
//! advertise a Vulkan device — with no error, no log line and no command on
//! any queue. That is the whole reason this module exists as testable bytes
//! rather than as a literal inside a renderer: the failure mode is *silence*,
//! so the bytes have to be asserted here, where a test can see them.
//!
//! # The wire layout
//!
//! Forty consecutive little-endian `u32`, no padding, no length prefix
//! (virglrenderer 1.1.0, `src/venus_hw.h`):
//!
//! | word | field | what it is |
//! |---|---|---|
//! | 0 | `wire_format_version` | must equal the guest's exactly |
//! | 1 | `vk_xml_version` | the `vk.xml` the protocol was generated from, in Vulkan's API-version packing |
//! | 2 | `vk_ext_command_serialization_spec_version` | `VK_EXT_command_serialization` |
//! | 3 | `vk_mesa_venus_protocol_spec_version` | `VK_MESA_venus_protocol` |
//! | 4 | `supports_blob_id_0` | blob id 0 means "plain memory, not a Vulkan object" |
//! | 5..37 | `vk_extension_mask1[32]` | 1024 bits; see [`ExtensionMask`] |
//! | 37 | `allow_vk_wait_syncs` | the guest may ask the host to block on a wait |
//! | 38 | `supports_multiple_timelines` | one fence timeline per `VkQueue` |
//! | 39 | `use_guest_vram` | the host **cannot** inject pages — see below |
//!
//! Word indices are 0-based here and named in [`word`], so a test can assert
//! each field's position on its own. A single golden blob would only ever tell
//! us *that* something moved.
//!
//! # Two fields whose names read backwards
//!
//! **`use_guest_vram` = 1 means we are the weaker host, not the stronger
//! one.** It says the hypervisor *cannot* inject host memory pages into the
//! guest's address space, so the guest must carve its Vulkan allocations out
//! of its own RAM and the host must copy. Both of our backends can inject —
//! KVM through a `KVM_SET_USER_MEMORY_REGION` slot and WHP through
//! `WHvMapGpaRange`, which `crates/vmm-core/tests/whp_shm.rs` proves against a
//! live partition — so our value is **0**. Reading the field as "do we use
//! guest VRAM? sure, the guest has VRAM" gets it exactly wrong and costs every
//! blob an extra copy.
//!
//! **`supports_multiple_timelines`'s `ring_idx` is not the Venus ring.** When
//! it is 1 the guest binds each `VkQueue` to its own virtio-gpu *fence
//! timeline*, named by the `ring_idx` byte of `struct virtio_gpu_ctrl_hdr`
//! ([`FLAG_INFO_RING_IDX`](crate::protocol::FLAG_INFO_RING_IDX)), with
//! `ring_idx == 0` reserved for CPU fences. That byte has nothing whatsoever
//! to do with the Venus *command ring* in [`super::ring`], which is a region
//! of a shared-memory blob and is never named on the control queue. The two
//! are unrelated mechanisms that collided on a word; assume they are the same
//! thing and the fence bookkeeping will look correct and signal the wrong
//! waiter.
//!
//! # Scope
//!
//! Pure bytes: no Vulkan, no renderer, no guest memory. A renderer that can
//! really execute Vulkan fills a [`VenusCapset`] in from what its host driver
//! reports and serves [`VenusCapset::to_bytes`]; everything in here builds and
//! is tested on a host with no GPU at all.

use thiserror::Error;

/// Words in `struct virgl_renderer_capset_venus`.
pub const VENUS_CAPSET_WORDS: usize = 40;

/// Length of the capset blob on the wire, in bytes. This is the `max_size` a
/// `GET_CAPSET_INFO` reply must carry for [`CAPSET_VENUS`](crate::CAPSET_VENUS):
/// the guest allocates exactly this much and reads exactly this much.
pub const VENUS_CAPSET_LEN: usize = VENUS_CAPSET_WORDS * 4;

/// `max_version` for the Venus capset. Venus versions its protocol inside the
/// blob (`wire_format_version` and the two spec versions), not through the
/// capset version, so this stays 0 — matching what
/// [`NullRenderer::with_venus`](crate::NullRenderer::with_venus) advertises.
pub const VENUS_CAPSET_MAX_VERSION: u32 = 0;

/// `u32` words in `vk_extension_mask1`.
pub const EXTENSION_MASK_WORDS: usize = 32;

/// Bits in `vk_extension_mask1`, one per Vulkan extension number — minus bit
/// 0, which is the validity sentinel rather than an extension.
pub const EXTENSION_MASK_BITS: u32 = EXTENSION_MASK_WORDS as u32 * 32;

/// Largest Vulkan extension number the mask can express. Vulkan extension
/// numbers start at 1 (`VK_KHR_surface`), so the usable range is
/// `1..=MAX_EXTENSION_NUMBER`.
pub const MAX_EXTENSION_NUMBER: u32 = EXTENSION_MASK_BITS - 1;

/// Bit 0 of `vk_extension_mask1[0]`: the *validity sentinel*, not extension 0.
const SENTINEL_BIT: u32 = 1 << 0;

/// Word index of every field, 0-based, as laid out by
/// `struct virgl_renderer_capset_venus`.
///
/// These are the numbers the tests assert one at a time. Nothing else in this
/// module hard-codes an offset.
pub mod word {
    /// `wire_format_version`.
    pub const WIRE_FORMAT_VERSION: usize = 0;
    /// `vk_xml_version`.
    pub const VK_XML_VERSION: usize = 1;
    /// `vk_ext_command_serialization_spec_version`.
    pub const VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION: usize = 2;
    /// `vk_mesa_venus_protocol_spec_version`.
    pub const VK_MESA_VENUS_PROTOCOL_SPEC_VERSION: usize = 3;
    /// `supports_blob_id_0`.
    pub const SUPPORTS_BLOB_ID_0: usize = 4;
    /// First word of `vk_extension_mask1[32]`; the array runs to
    /// `EXTENSION_MASK1 + 31` inclusive.
    pub const EXTENSION_MASK1: usize = 5;
    /// `allow_vk_wait_syncs`.
    pub const ALLOW_VK_WAIT_SYNCS: usize = 37;
    /// `supports_multiple_timelines`.
    pub const SUPPORTS_MULTIPLE_TIMELINES: usize = 38;
    /// `use_guest_vram`.
    pub const USE_GUEST_VRAM: usize = 39;
}

// ------------------------------------------------------- Vulkan API versions

/// Vulkan's `VK_MAKE_API_VERSION`: variant in bits 31..29, major in 28..22,
/// minor in 21..12, patch in 11..0.
///
/// Faithful to the macro, which means it does **not** mask its arguments: a
/// part that does not fit its field bleeds into the next one, exactly as the C
/// macro does. Call it with real version numbers.
pub const fn vk_make_api_version(variant: u32, major: u32, minor: u32, patch: u32) -> u32 {
    (variant << 29) | (major << 22) | (minor << 12) | patch
}

/// Inverse of [`vk_make_api_version`]: `(variant, major, minor, patch)`.
pub const fn vk_api_version_parts(packed: u32) -> (u32, u32, u32, u32) {
    (
        packed >> 29,
        (packed >> 22) & 0x7f,
        (packed >> 12) & 0x3ff,
        packed & 0xfff,
    )
}

// ----------------------------------------------------------- extension mask

/// `vk_extension_mask1`: 1024 bits in which extension number `n` is bit
/// `n % 32` of word `n / 32` — **except** bit 0 of word 0, which is a validity
/// sentinel.
///
/// # The sentinel, and why the all-zero mask is the dangerous one
///
/// `venus_hw.h` states it outright: when bit 0 of word 0 is *set*, the masks
/// are meaningful and the guest uses only the extensions listed. When it is
/// *clear*, the guest assumes the renderer supports **every** extension it
/// knows about. So the default a careless `[0u32; 32]` produces is not "we
/// support nothing", it is "ask us for anything" — a promise no renderer in
/// this project can keep, and one whose breach surfaces as a guest-side crash
/// in an extension entry point rather than as a refusal.
///
/// This type makes that state reachable only by name:
///
/// * [`ExtensionMask::ENUMERATED`] (and [`Default`]) has the sentinel set and
///   nothing enumerated — "we support no optional extension", which is honest
///   and inert;
/// * [`ExtensionMask::enable`] sets the sentinel as well as the extension bit,
///   so a mask with bits set and the sentinel clear — a list the guest would
///   silently ignore — cannot be built;
/// * [`ExtensionMask::GUEST_ASSUMES_EVERYTHING`] is the all-zero mask, and is
///   spelled out at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionMask {
    words: [u32; EXTENSION_MASK_WORDS],
}

impl ExtensionMask {
    /// Sentinel set, no extensions enumerated: the guest takes the mask at its
    /// word and uses no optional extension. The safe starting point, and
    /// [`Default`].
    pub const ENUMERATED: Self = {
        let mut words = [0u32; EXTENSION_MASK_WORDS];
        words[0] = SENTINEL_BIT;
        Self { words }
    };

    /// All zero, sentinel included — which tells the guest to assume the
    /// renderer supports **every** Vulkan extension (see the type docs).
    ///
    /// Correct only for a renderer that genuinely passes everything through to
    /// a real host driver and is willing to be asked for anything. Enabling a
    /// single extension with [`ExtensionMask::enable`] converts it back into
    /// an enumerated mask, because the two states cannot be held at once.
    pub const GUEST_ASSUMES_EVERYTHING: Self = Self {
        words: [0u32; EXTENSION_MASK_WORDS],
    };

    /// An enumerated mask holding exactly `numbers`.
    ///
    /// Fails on the first number outside `1..=`[`MAX_EXTENSION_NUMBER`]
    /// rather than dropping it, so a typo in a renderer's extension list is
    /// loud instead of being a quietly missing feature.
    pub fn enumerating(numbers: impl IntoIterator<Item = u32>) -> Result<Self, CapsetError> {
        let mut mask = Self::ENUMERATED;
        for number in numbers {
            mask.enable(number)?;
        }
        Ok(mask)
    }

    /// Word index and bit position of Vulkan extension number `n`.
    ///
    /// Rejects 0 separately from the out-of-range numbers: 0 is not merely
    /// invalid, it is the slot the sentinel occupies, and a caller that thinks
    /// it is enabling "extension 0" is really flipping the meaning of the
    /// whole mask.
    pub fn locate(extension_number: u32) -> Result<(usize, u32), CapsetError> {
        if extension_number == 0 {
            return Err(CapsetError::SentinelIsNotAnExtension);
        }
        if extension_number > MAX_EXTENSION_NUMBER {
            return Err(CapsetError::ExtensionOutOfRange(extension_number));
        }
        Ok(((extension_number / 32) as usize, extension_number % 32))
    }

    /// Advertises Vulkan extension `extension_number`, setting the validity
    /// sentinel with it.
    pub fn enable(&mut self, extension_number: u32) -> Result<(), CapsetError> {
        let (index, bit) = Self::locate(extension_number)?;
        if let Some(slot) = self.words.get_mut(index) {
            *slot |= 1u32 << bit;
        }
        // An enumerated bit is meaningless unless the mask is authoritative,
        // so enabling anything at all promotes `GUEST_ASSUMES_EVERYTHING`.
        if let Some(first) = self.words.first_mut() {
            *first |= SENTINEL_BIT;
        }
        Ok(())
    }

    /// True when the sentinel is set, i.e. the guest will read the bits as an
    /// exhaustive list.
    pub fn is_enumerated(&self) -> bool {
        self.words.first().is_some_and(|w| w & SENTINEL_BIT != 0)
    }

    /// The raw bit for `extension_number`, ignoring the sentinel. `false` for
    /// any number outside the representable range.
    ///
    /// This is what was *written*; [`ExtensionMask::guest_may_use`] is what
    /// the guest will *conclude*, and on an all-zero mask the two disagree for
    /// every extension in Vulkan.
    pub fn is_enabled(&self, extension_number: u32) -> bool {
        match Self::locate(extension_number) {
            Ok((index, bit)) => self
                .words
                .get(index)
                .is_some_and(|w| w & (1u32 << bit) != 0),
            Err(_) => false,
        }
    }

    /// What the guest concludes about `extension_number` from this mask: the
    /// enumerated bit when the sentinel is set, and unconditionally `true`
    /// when it is clear.
    pub fn guest_may_use(&self, extension_number: u32) -> bool {
        !self.is_enumerated() || self.is_enabled(extension_number)
    }

    /// The 32 words as they appear on the wire.
    pub fn words(&self) -> &[u32; EXTENSION_MASK_WORDS] {
        &self.words
    }

    /// Wraps 32 words read off the wire. Used by [`VenusCapset::parse`]; the
    /// sentinel is preserved exactly as it was found, because the point of
    /// decoding is to see what a renderer really said.
    pub fn from_words(words: [u32; EXTENSION_MASK_WORDS]) -> Self {
        Self { words }
    }
}

impl Default for ExtensionMask {
    fn default() -> Self {
        Self::ENUMERATED
    }
}

// ------------------------------------------------------------------ capset

/// The Venus capset this device advertises.
///
/// [`VenusCapset::new`] is the one this project should serve today; the fields
/// are public so a renderer that can do more (a real extension list, per-queue
/// fence timelines) can say so, and every one of them is typed so that the
/// only values reaching the wire are values the protocol defines — the five
/// flag words are `bool`, and the extension mask is an [`ExtensionMask`]
/// rather than 32 loose words.
///
/// `#[non_exhaustive]`: `struct virgl_renderer_capset_venus` has grown fields
/// at its tail across virglrenderer releases and will again, so downstream
/// code starts from [`VenusCapset::new`] and adjusts rather than writing a
/// struct literal that a later version would break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VenusCapset {
    /// Must equal the guest ICD's own constant exactly. Mesa compares it
    /// first and gives up on any mismatch, in either direction.
    pub wire_format_version: u32,

    /// The `vk.xml` the serialized protocol was generated against, packed by
    /// [`vk_make_api_version`]. Ours is
    /// [`VK_XML_VERSION`](VenusCapset::VK_XML_VERSION) — Vulkan-Headers
    /// `v1.3.269`, the version ADR-0004 pins for the virglrenderer build.
    pub vk_xml_version: u32,

    /// `VK_EXT_command_serialization` spec version.
    pub vk_ext_command_serialization_spec_version: u32,

    /// `VK_MESA_venus_protocol` spec version.
    pub vk_mesa_venus_protocol_spec_version: u32,

    /// `blob_id` 0 is legal and means "plain memory, not a Vulkan object" —
    /// which is how the guest allocates its command ring and reply shmem
    /// before it owns any `VkDeviceMemory` to name.
    ///
    /// True for us: [`BlobTable`](crate::BlobTable) treats `blob_id` as an
    /// opaque `u64` and reserves no value of it.
    pub supports_blob_id_0: bool,

    /// Which Vulkan extensions the guest may use. See [`ExtensionMask`] — the
    /// all-zero mask means the opposite of what it looks like.
    pub extensions: ExtensionMask,

    /// The guest may hand the host blocking waits (`vkWaitForFences`,
    /// `vkWaitSemaphores`) instead of polling their status from the guest.
    ///
    /// True for us, matching virglrenderer: the wait blocks a thread inside
    /// virglrenderer's *render server process* (ADR-0004 — Venus is decoded
    /// out of process), never a queue-serving thread of this device, and
    /// Mesa's polling fallback is the less-travelled path.
    pub allow_vk_wait_syncs: bool,

    /// Each `VkQueue` gets its own virtio-gpu fence timeline, named by the
    /// header's `ring_idx` (`ring_idx == 0` is reserved for CPU fences).
    ///
    /// It takes two things, and both exist since EPIC 20 stage 5b.3: a device
    /// whose fence table keeps one FIFO per `(context, ring_idx)`
    /// ([`FenceQueue`](crate::FenceQueue) and
    /// [`FenceTimeline`](crate::FenceTimeline)), so a later queue's fence
    /// never waits behind an unrelated earlier queue's; and a renderer that
    /// retires a fence on a queue's timeline when that queue's work is done.
    /// [`Self::new`] says **false**, because a renderer that executes nothing
    /// (a capture) has no queue to retire one on; the Venus renderer sets it
    /// from its sink factory (`SinkFactory::retires_ring_fences`), which is
    /// **true** for the executor. Mesa only asserts the bit and binds every
    /// queue to a timeline regardless.
    ///
    /// Note again that this `ring_idx` is *not* the Venus command ring of
    /// [`super::ring`]; see the module docs.
    pub supports_multiple_timelines: bool,

    /// The hypervisor **cannot** inject host pages into the guest, so the
    /// guest must allocate blob memory from its own RAM.
    ///
    /// **False for us** — both backends can inject, so the guest should let
    /// the host own the memory and map it through the shared-memory window.
    /// The name reads like a question about VRAM; it is really a statement
    /// about the hypervisor's weakness. See the module docs.
    pub use_guest_vram: bool,
}

impl VenusCapset {
    /// `wire_format_version` this device speaks.
    pub const WIRE_FORMAT_VERSION: u32 = 1;

    /// Vulkan-Headers `v1.3.269` in Vulkan's API-version packing — the
    /// version ADR-0004 pins for the virglrenderer build, because the bundled
    /// `venus-protocol` headers are generated against `VK_HEADER_VERSION 269`.
    pub const VK_XML_VERSION: u32 = vk_make_api_version(0, 1, 3, 269);

    /// `VK_EXT_command_serialization` spec version.
    pub const VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION: u32 = 1;

    /// `VK_MESA_venus_protocol` spec version.
    pub const VK_MESA_VENUS_PROTOCOL_SPEC_VERSION: u32 = 2;

    /// The capset this project advertises today.
    ///
    /// The protocol versions are fixed by the ICD we have to satisfy; the
    /// three capability flags are this VMM's honest answers (see each field).
    /// The extension mask is enumerated — sentinel set — and holds exactly the
    /// extensions whose chained structures the executor **admits**
    /// ([`admitted_extension_mask`](super::executor::policy::admitted_extension_mask)):
    /// the two venus protocol extensions and every extension promoted into
    /// core 1.1–1.3 that brought a structure with it, derived from the same
    /// rule the executor judges each chain by.
    ///
    /// This is narrower than virglrenderer, which advertises everything its
    /// protocol decodes (`vkr_renderer.c:40-48`) because it hands every
    /// decoded structure to the driver. The mask gates only which structures
    /// the guest's *encoder* may send (`vn_cs_renderer_protocol_has_extension`),
    /// dropping the rest silently — so a bit here obliges the executor to
    /// accept that extension's structures (a decoded one it does not admit is
    /// fatal), and a missing bit drops even a core structure the guest gates on
    /// its original extension (`VkPhysicalDeviceSynchronization2Features` on
    /// bit 315). Which extensions a device *offers* is the executor's
    /// `vkEnumerateDeviceExtensionProperties` answer, a separate question.
    pub fn new() -> Self {
        Self {
            wire_format_version: Self::WIRE_FORMAT_VERSION,
            vk_xml_version: Self::VK_XML_VERSION,
            vk_ext_command_serialization_spec_version:
                Self::VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION,
            vk_mesa_venus_protocol_spec_version: Self::VK_MESA_VENUS_PROTOCOL_SPEC_VERSION,
            supports_blob_id_0: true,
            extensions: super::executor::policy::admitted_extension_mask(),
            allow_vk_wait_syncs: true,
            supports_multiple_timelines: false,
            use_guest_vram: false,
        }
    }

    /// Advertises one more Vulkan extension by number; see
    /// [`ExtensionMask::enable`].
    pub fn enable_extension(&mut self, extension_number: u32) -> Result<(), CapsetError> {
        self.extensions.enable(extension_number)
    }

    /// The capset as its 40 words, in wire order.
    pub fn to_words(&self) -> [u32; VENUS_CAPSET_WORDS] {
        let mut out = [0u32; VENUS_CAPSET_WORDS];
        let mut put = |index: usize, value: u32| {
            if let Some(slot) = out.get_mut(index) {
                *slot = value;
            }
        };
        put(word::WIRE_FORMAT_VERSION, self.wire_format_version);
        put(word::VK_XML_VERSION, self.vk_xml_version);
        put(
            word::VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION,
            self.vk_ext_command_serialization_spec_version,
        );
        put(
            word::VK_MESA_VENUS_PROTOCOL_SPEC_VERSION,
            self.vk_mesa_venus_protocol_spec_version,
        );
        put(word::SUPPORTS_BLOB_ID_0, u32::from(self.supports_blob_id_0));
        for (index, mask_word) in self.extensions.words().iter().enumerate() {
            put(word::EXTENSION_MASK1 + index, *mask_word);
        }
        put(
            word::ALLOW_VK_WAIT_SYNCS,
            u32::from(self.allow_vk_wait_syncs),
        );
        put(
            word::SUPPORTS_MULTIPLE_TIMELINES,
            u32::from(self.supports_multiple_timelines),
        );
        put(word::USE_GUEST_VRAM, u32::from(self.use_guest_vram));
        out
    }

    /// The capset as the [`VENUS_CAPSET_LEN`] little-endian bytes a
    /// `GET_CAPSET` reply carries.
    pub fn to_bytes(&self) -> [u8; VENUS_CAPSET_LEN] {
        let mut out = [0u8; VENUS_CAPSET_LEN];
        for (index, value) in self.to_words().iter().enumerate() {
            let at = index * 4;
            if let Some(slot) = out.get_mut(at..at + 4) {
                slot.copy_from_slice(&value.to_le_bytes());
            }
        }
        out
    }

    /// Reads a capset blob back — what a host renderer handed us, or what this
    /// module wrote.
    ///
    /// Total, in the style of [`crate::protocol`]: `None` for a buffer shorter
    /// than [`VENUS_CAPSET_LEN`], never a panic and never an out-of-bounds
    /// index. Trailing bytes are ignored, because a newer virglrenderer's
    /// struct is this one plus fields at the end.
    ///
    /// The five flag words are normalised the way C reads them — any non-zero
    /// value is `true` — so a blob containing `2` decodes to `true` and
    /// re-encodes as `1`. That is a deliberate asymmetry: the wire is being
    /// interpreted, not preserved.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < VENUS_CAPSET_LEN {
            return None;
        }
        let at = |index: usize| -> u32 {
            let start = index * 4;
            match bytes
                .get(start..start + 4)
                .and_then(|s| <[u8; 4]>::try_from(s).ok())
            {
                Some(raw) => u32::from_le_bytes(raw),
                None => 0,
            }
        };
        let mut mask = [0u32; EXTENSION_MASK_WORDS];
        for (index, slot) in mask.iter_mut().enumerate() {
            *slot = at(word::EXTENSION_MASK1 + index);
        }
        Some(Self {
            wire_format_version: at(word::WIRE_FORMAT_VERSION),
            vk_xml_version: at(word::VK_XML_VERSION),
            vk_ext_command_serialization_spec_version: at(
                word::VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION,
            ),
            vk_mesa_venus_protocol_spec_version: at(word::VK_MESA_VENUS_PROTOCOL_SPEC_VERSION),
            supports_blob_id_0: at(word::SUPPORTS_BLOB_ID_0) != 0,
            extensions: ExtensionMask::from_words(mask),
            allow_vk_wait_syncs: at(word::ALLOW_VK_WAIT_SYNCS) != 0,
            supports_multiple_timelines: at(word::SUPPORTS_MULTIPLE_TIMELINES) != 0,
            use_guest_vram: at(word::USE_GUEST_VRAM) != 0,
        })
    }
}

impl Default for VenusCapset {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------ errors

/// Why a capset could not be built. Host-side configuration mistakes, not
/// guest input: nothing a guest sends reaches this module, and the capset is
/// written by the host before any guest command exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CapsetError {
    #[error(
        "Vulkan extension number {0} is outside the capset's mask \
         (1..={max})",
        max = MAX_EXTENSION_NUMBER
    )]
    ExtensionOutOfRange(u32),

    #[error(
        "there is no Vulkan extension number 0: bit 0 of vk_extension_mask1[0] \
         is the mask's validity sentinel"
    )]
    SentinelIsNotAnExtension,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words_of(capset: &VenusCapset) -> [u32; VENUS_CAPSET_WORDS] {
        let bytes = capset.to_bytes();
        let mut out = [0u32; VENUS_CAPSET_WORDS];
        for (index, slot) in out.iter_mut().enumerate() {
            let at = index * 4;
            let raw: [u8; 4] = bytes[at..at + 4].try_into().expect("four bytes");
            *slot = u32::from_le_bytes(raw);
        }
        out
    }

    // --------------------------------------------------------------- layout

    #[test]
    fn the_blob_is_exactly_one_hundred_and_sixty_bytes() {
        // `struct virgl_renderer_capset_venus` is 40 u32 with no padding. The
        // guest sizes its GET_CAPSET buffer from `capset_max_size`, so a blob
        // of any other length is not "slightly wrong", it is unreadable.
        assert_eq!(VENUS_CAPSET_WORDS, 40);
        assert_eq!(VENUS_CAPSET_LEN, 160);
        assert_eq!(VenusCapset::new().to_bytes().len(), 160);
    }

    #[test]
    fn word_offsets_match_venus_hw_h() {
        assert_eq!(word::WIRE_FORMAT_VERSION, 0);
        assert_eq!(word::VK_XML_VERSION, 1);
        assert_eq!(word::VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION, 2);
        assert_eq!(word::VK_MESA_VENUS_PROTOCOL_SPEC_VERSION, 3);
        assert_eq!(word::SUPPORTS_BLOB_ID_0, 4);
        assert_eq!(word::EXTENSION_MASK1, 5);
        // The mask is 32 words, so it ends at 36 and the tail starts at 37.
        assert_eq!(EXTENSION_MASK_WORDS, 32);
        assert_eq!(word::EXTENSION_MASK1 + EXTENSION_MASK_WORDS, 37);
        assert_eq!(word::ALLOW_VK_WAIT_SYNCS, 37);
        assert_eq!(word::SUPPORTS_MULTIPLE_TIMELINES, 38);
        assert_eq!(word::USE_GUEST_VRAM, 39);
        assert_eq!(word::USE_GUEST_VRAM + 1, VENUS_CAPSET_WORDS);
    }

    #[test]
    fn each_version_field_lands_in_its_own_word() {
        // Distinct probe values, so a swapped pair cannot pass.
        let mut capset = VenusCapset::new();
        capset.wire_format_version = 0x1111_1111;
        capset.vk_xml_version = 0x2222_2222;
        capset.vk_ext_command_serialization_spec_version = 0x3333_3333;
        capset.vk_mesa_venus_protocol_spec_version = 0x4444_4444;

        let words = words_of(&capset);
        assert_eq!(words[word::WIRE_FORMAT_VERSION], 0x1111_1111);
        assert_eq!(words[word::VK_XML_VERSION], 0x2222_2222);
        assert_eq!(
            words[word::VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION],
            0x3333_3333
        );
        assert_eq!(
            words[word::VK_MESA_VENUS_PROTOCOL_SPEC_VERSION],
            0x4444_4444
        );
    }

    #[test]
    fn each_flag_field_moves_exactly_its_own_word() {
        // The four boolean words cannot be told apart by value (all are 1), so
        // assert them by isolation: flipping one changes one word, at the
        // offset venus_hw.h gives it.
        type SetFlag = fn(&mut VenusCapset);
        let flags: [(usize, SetFlag); 4] = [
            (word::SUPPORTS_BLOB_ID_0, |c| c.supports_blob_id_0 = true),
            (word::ALLOW_VK_WAIT_SYNCS, |c| c.allow_vk_wait_syncs = true),
            (word::SUPPORTS_MULTIPLE_TIMELINES, |c| {
                c.supports_multiple_timelines = true
            }),
            (word::USE_GUEST_VRAM, |c| c.use_guest_vram = true),
        ];

        let mut all_false = VenusCapset::new();
        all_false.supports_blob_id_0 = false;
        all_false.allow_vk_wait_syncs = false;
        all_false.supports_multiple_timelines = false;
        all_false.use_guest_vram = false;
        let base = words_of(&all_false);

        for (index, set) in flags {
            let mut capset = all_false;
            set(&mut capset);
            let words = words_of(&capset);
            assert_eq!(words[index], 1, "word {index} should carry the flag");
            for (other, (before, after)) in base.iter().zip(words.iter()).enumerate() {
                if other != index {
                    assert_eq!(before, after, "flag at word {index} disturbed word {other}");
                }
            }
        }
    }

    #[test]
    fn the_extension_mask_occupies_words_five_to_thirty_six() {
        // One bit in the first mask word and one in the last, so both ends of
        // the array are pinned.
        // From an empty enumerated mask: the default one carries the
        // decodable transport extensions (word 12), which is not what this
        // test is about.
        let mut capset = VenusCapset::new();
        capset.extensions = ExtensionMask::ENUMERATED;
        capset.enable_extension(1).expect("extension 1");
        capset.enable_extension(MAX_EXTENSION_NUMBER).expect("1023");

        let words = words_of(&capset);
        assert_eq!(words[word::EXTENSION_MASK1], SENTINEL_BIT | (1 << 1));
        assert_eq!(words[word::EXTENSION_MASK1 + 31], 1 << 31);
        for index in 1..31 {
            assert_eq!(words[word::EXTENSION_MASK1 + index], 0, "word {index}");
        }
    }

    #[test]
    fn words_are_encoded_little_endian() {
        let mut capset = VenusCapset::new();
        capset.wire_format_version = 0x0403_0201;
        let bytes = capset.to_bytes();
        assert_eq!(&bytes[..4], &[0x01, 0x02, 0x03, 0x04]);
        // And the second word really starts at byte 4, not byte 8 or 2.
        assert_eq!(&bytes[4..8], &VenusCapset::VK_XML_VERSION.to_le_bytes()[..],);
    }

    // ------------------------------------------------------------ decoding

    #[test]
    fn a_capset_round_trips_through_its_bytes() {
        let mut capset = VenusCapset::new();
        capset.supports_multiple_timelines = true;
        capset.use_guest_vram = true;
        capset.enable_extension(7).expect("extension 7");
        capset.enable_extension(600).expect("extension 600");

        let decoded = VenusCapset::parse(&capset.to_bytes()).expect("160 bytes parse");
        assert_eq!(decoded, capset);
    }

    #[test]
    fn parse_is_total_on_short_and_long_buffers() {
        let bytes = VenusCapset::new().to_bytes();
        for len in 0..VENUS_CAPSET_LEN {
            assert!(VenusCapset::parse(&bytes[..len]).is_none(), "len {len}");
        }
        // A newer virglrenderer's struct is this one plus a tail; reading the
        // prefix we understand is right, and must not panic.
        let mut longer = bytes.to_vec();
        longer.extend_from_slice(&[0xff; 64]);
        assert_eq!(VenusCapset::parse(&longer), Some(VenusCapset::new()));
    }

    #[test]
    fn parse_reads_flags_the_way_c_does() {
        // Any non-zero value is true; re-encoding normalises it to 1.
        let mut bytes = VenusCapset::new().to_bytes();
        let at = word::USE_GUEST_VRAM * 4;
        bytes[at..at + 4].copy_from_slice(&7u32.to_le_bytes());
        let decoded = VenusCapset::parse(&bytes).expect("parse");
        assert!(decoded.use_guest_vram);
        assert_eq!(words_of(&decoded)[word::USE_GUEST_VRAM], 1);
    }

    // ------------------------------------------------- extension numbering

    #[test]
    fn extension_number_maps_to_word_and_bit() {
        // n -> (word n/32, bit n%32). The boundaries are what a hand-rolled
        // version gets wrong.
        assert_eq!(ExtensionMask::locate(1), Ok((0, 1)));
        assert_eq!(ExtensionMask::locate(31), Ok((0, 31)));
        assert_eq!(ExtensionMask::locate(32), Ok((1, 0)));
        assert_eq!(ExtensionMask::locate(33), Ok((1, 1)));
        assert_eq!(ExtensionMask::locate(63), Ok((1, 31)));
        assert_eq!(ExtensionMask::locate(64), Ok((2, 0)));
        assert_eq!(ExtensionMask::locate(MAX_EXTENSION_NUMBER), Ok((31, 31)));
        assert_eq!(MAX_EXTENSION_NUMBER, 1023);
        assert_eq!(EXTENSION_MASK_BITS, 1024);
    }

    #[test]
    fn enabling_an_extension_sets_that_bit_and_no_other() {
        for number in [1u32, 31, 32, 33, 63, 64, 512, MAX_EXTENSION_NUMBER] {
            let mask = ExtensionMask::enumerating([number]).expect("in range");
            let (index, bit) = ExtensionMask::locate(number).expect("in range");
            // Word 0 also carries the sentinel; every other word carries the
            // one bit and nothing else. (Extension 32 is bit 0 of word 1, so
            // stripping bit 0 unconditionally would hide a real regression.)
            let sentinel = if index == 0 { SENTINEL_BIT } else { 0 };
            assert_eq!(
                mask.words()[index],
                (1u32 << bit) | sentinel,
                "extension {number}"
            );
            assert!(mask.is_enabled(number));
            let set_bits: u32 = mask.words().iter().map(|w| w.count_ones()).sum();
            // The extension plus the sentinel; when the extension *is* bit 0
            // of word 0 that would collide, which is why 0 is refused.
            assert_eq!(set_bits, 2, "extension {number}");
        }
    }

    #[test]
    fn extension_zero_is_the_sentinel_and_is_refused() {
        // Vulkan has no extension number 0, and bit 0 of word 0 means
        // something else entirely. Accepting it would silently turn "we
        // support extension 0" into "this mask is authoritative".
        assert_eq!(
            ExtensionMask::locate(0),
            Err(CapsetError::SentinelIsNotAnExtension)
        );
        let mut mask = ExtensionMask::GUEST_ASSUMES_EVERYTHING;
        assert_eq!(mask.enable(0), Err(CapsetError::SentinelIsNotAnExtension));
        assert!(!mask.is_enumerated(), "a refused enable changed nothing");
    }

    #[test]
    fn extension_numbers_past_the_mask_are_refused() {
        for number in [
            EXTENSION_MASK_BITS,
            EXTENSION_MASK_BITS + 1,
            100_000,
            u32::MAX,
        ] {
            assert_eq!(
                ExtensionMask::locate(number),
                Err(CapsetError::ExtensionOutOfRange(number)),
                "{number}"
            );
            assert!(!ExtensionMask::ENUMERATED.is_enabled(number));
        }
        assert!(ExtensionMask::enumerating([1, u32::MAX]).is_err());
    }

    // ------------------------------------------------------------ sentinel

    #[test]
    fn a_capset_built_the_normal_way_sets_the_validity_sentinel() {
        let capset = VenusCapset::new();
        assert!(capset.extensions.is_enumerated());
        assert_eq!(words_of(&capset)[word::EXTENSION_MASK1] & SENTINEL_BIT, 1);
        assert_eq!(ExtensionMask::default(), ExtensionMask::ENUMERATED);
    }

    #[test]
    fn clearing_the_sentinel_promises_the_guest_every_extension_in_vulkan() {
        // This is the consequence the name is for: an all-zero mask does NOT
        // say "no extensions". venus_hw.h says the guest then assumes the
        // renderer supports every extension it knows about, so it will call
        // into entry points this host may not implement — and the failure
        // lands in the guest, at the call, not here at negotiation.
        let mask = ExtensionMask::GUEST_ASSUMES_EVERYTHING;
        assert_eq!(mask.words(), &[0u32; EXTENSION_MASK_WORDS]);
        assert!(!mask.is_enumerated());
        for number in [1u32, 32, 269, MAX_EXTENSION_NUMBER] {
            assert!(!mask.is_enabled(number), "nothing is written");
            assert!(mask.guest_may_use(number), "yet the guest will use it");
        }

        // The enumerated mask promises nothing and is believed.
        let enumerated = ExtensionMask::ENUMERATED;
        for number in [1u32, 32, 269, MAX_EXTENSION_NUMBER] {
            assert!(!enumerated.guest_may_use(number));
        }
    }

    #[test]
    fn enumerated_bits_cannot_exist_without_the_sentinel() {
        // The state that would be ignored on the wire — bits set, sentinel
        // clear — is unreachable through the API: enabling anything promotes
        // the mask.
        let mut mask = ExtensionMask::GUEST_ASSUMES_EVERYTHING;
        mask.enable(269).expect("in range");
        assert!(mask.is_enumerated());
        assert!(mask.is_enabled(269));
        assert!(!mask.guest_may_use(270), "the list is now exhaustive");
    }

    #[test]
    fn from_words_preserves_what_a_renderer_actually_said() {
        // Decoding must not "fix" a mask: a renderer that really did serve an
        // all-zero mask has to be visible as such.
        let mask = ExtensionMask::from_words([0u32; EXTENSION_MASK_WORDS]);
        assert!(!mask.is_enumerated());
        assert_eq!(mask, ExtensionMask::GUEST_ASSUMES_EVERYTHING);
    }

    // ------------------------------------------------------------ versions

    #[test]
    fn vk_api_version_packing_matches_vulkans_macro() {
        // VK_MAKE_API_VERSION(variant, major, minor, patch):
        // variant << 29 | major << 22 | minor << 12 | patch.
        assert_eq!(vk_make_api_version(0, 1, 0, 0), 0x0040_0000);
        assert_eq!(vk_make_api_version(0, 1, 1, 0), 0x0040_1000);
        assert_eq!(vk_make_api_version(0, 1, 3, 0), 0x0040_3000);
        assert_eq!(vk_make_api_version(1, 0, 0, 0), 0x2000_0000);
        assert_eq!(vk_make_api_version(0, 0, 0, 1), 1);
        for parts in [(0, 1, 3, 269), (0, 1, 0, 0), (1, 127, 1023, 4095)] {
            let (variant, major, minor, patch) = parts;
            let packed = vk_make_api_version(variant, major, minor, patch);
            assert_eq!(vk_api_version_parts(packed), parts, "{parts:?}");
        }
    }

    #[test]
    fn vk_xml_version_decodes_to_one_three_two_six_nine() {
        // Vulkan-Headers v1.3.269, the tag ADR-0004 pins for the
        // virglrenderer build whose venus-protocol headers this matches.
        assert_eq!(
            vk_api_version_parts(VenusCapset::VK_XML_VERSION),
            (0, 1, 3, 269)
        );
        assert_eq!(VenusCapset::VK_XML_VERSION, 4_206_861);
        assert_eq!(
            words_of(&VenusCapset::new())[word::VK_XML_VERSION],
            4_206_861
        );
    }

    // ------------------------------------------------- what we advertise

    #[test]
    fn the_default_capset_is_the_one_this_project_should_serve() {
        let capset = VenusCapset::new();
        assert_eq!(capset, VenusCapset::default());

        let words = words_of(&capset);
        assert_eq!(words[word::WIRE_FORMAT_VERSION], 1);
        assert_eq!(words[word::VK_EXT_COMMAND_SERIALIZATION_SPEC_VERSION], 1);
        assert_eq!(words[word::VK_MESA_VENUS_PROTOCOL_SPEC_VERSION], 2);
        // blob_id 0 is legal here: BlobTable keeps `blob_id` opaque and
        // reserves no value of it.
        assert_eq!(words[word::SUPPORTS_BLOB_ID_0], 1);
        // A blocking wait blocks virglrenderer's render-server process, not a
        // queue-serving thread of this device (ADR-0004).
        assert_eq!(words[word::ALLOW_VK_WAIT_SYNCS], 1);
        // No queue timelines by default: a renderer that executes nothing has
        // no queue to retire a fence on. The executing one turns this on
        // (`VenusRenderer::new`, from its factory); see the field docs.
        assert_eq!(words[word::SUPPORTS_MULTIPLE_TIMELINES], 0);
        // Exactly what the executor admits, sentinel set — never the all-zero
        // "assume everything" mask, and no longer the whole decodable table.
        assert_eq!(
            words[word::EXTENSION_MASK1..word::EXTENSION_MASK1 + EXTENSION_MASK_WORDS],
            *super::super::executor::policy::admitted_extension_mask().words()
        );
        assert!(capset.extensions.is_enumerated());
        assert_ne!(
            capset.extensions.words(),
            &super::super::protocol::info::DECODABLE_EXTENSION_MASK
        );
    }

    #[test]
    fn use_guest_vram_is_zero_because_this_vmm_can_inject_host_pages() {
        // 1 would mean "the hypervisor cannot put host pages into the guest,
        // so allocate from guest RAM and copy". KVM can (memory slots) and WHP
        // can (WhvMapGpaRange, proven by vmm-core's whp_shm test), so saying 1
        // would cost every Vulkan allocation a host copy for nothing.
        assert!(!VenusCapset::new().use_guest_vram);
        assert_eq!(words_of(&VenusCapset::new())[word::USE_GUEST_VRAM], 0);
    }

    #[test]
    fn the_capset_length_is_what_a_capset_info_reply_must_advertise() {
        // GET_CAPSET_INFO carries `capset_max_size`; the guest allocates that
        // much and reads that much, so the two must agree by construction.
        assert_eq!(VenusCapset::new().to_bytes().len(), VENUS_CAPSET_LEN);
        assert_eq!(VENUS_CAPSET_MAX_VERSION, 0);
    }
}
