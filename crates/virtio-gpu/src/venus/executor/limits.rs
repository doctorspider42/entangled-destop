//! Every cap the executor holds a guest to, in one place (ADR-0004, the
//! resource-exhaustion amendment).
//!
//! A guest that is root in its VM, or any process in it that can open the
//! render node, sends whatever Venus stream it likes. What it can make the
//! *host* allocate is bounded here, twice: per venus context (a **share**) and
//! for the whole renderer (the **whole**), with the same
//! [`PageBudget::share`] rule the host-visible pages already follow — a charge
//! must fit the context's share *and* what is left of the whole, and every
//! refund goes back to both. One context that runs away is refused at its
//! share and the rest of the guest keeps what it holds; every context
//! together is refused at the whole and the host keeps the rest.
//!
//! # What is counted
//!
//! * **Objects** ([`Class::Objects`]): every entry of every context's object
//!   table, each also counted under its kind's class where it has one
//!   ([`class_of`]): devices, memory objects, pipelines, shader modules,
//!   pipeline caches, descriptor pools, query pools, command pools, command
//!   buffers, fences, semaphores and events — the host objects that cost a
//!   driver real memory or an OS object each. The rest (buffers, images,
//!   views, samplers, layouts, render passes, framebuffers, sets, …) count
//!   only as objects.
//! * **Costs an object carries** beyond its count, charged when it is created
//!   and held by its table entry: [`Class::ShaderBytes`] (SPIR-V and pipeline
//!   cache data a driver copies), [`Class::Descriptors`] (a descriptor pool's
//!   `maxSets` plus its descriptors, which a driver allocates up front),
//!   [`Class::Queries`] (a query pool's slots) and [`Class::RecordingBytes`]
//!   (the commands recorded into a command buffer, by their wire size, until
//!   it is begun again, reset or freed).
//! * **Device-local memory**, per heap of each host GPU: a plain
//!   `vkAllocateMemory` of a device-local heap, held by the memory object
//!   and by any handle blob or import of it, because the allocation lives as
//!   long as any of them ([`Limits::device_local`]).
//! * **Transient decode memory** ([`Class::DecodeBytes`]): what one command's
//!   decode allocates and the copies of the command streams a
//!   `vkExecuteCommandStreamsMESA` runs, while that command is in flight.
//! * **Held submits** ([`Class::HeldSubmits`], [`Class::HeldBytes`]): guest
//!   submits the executor keeps back until the driver could meet their
//!   waits ([`super::hold`]), by count and by wire bytes, until they are
//!   released or dropped.
//!
//! # Leaks
//!
//! Every charge is a [`Charge`] owned by what it stands for, so it is given
//! back on whatever path lets that go — `vkDestroy*`, a pool freeing its
//! children, a device taking everything with it, a context destroyed, a
//! device reset (ADR-0005) — with no bookkeeping to forget.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use crate::venus::protocol::{
    VkResult, VK_ERROR_OUT_OF_DEVICE_MEMORY, VK_ERROR_OUT_OF_HOST_MEMORY, VK_ERROR_TOO_MANY_OBJECTS,
};
use crate::venus::shmem::{Charge, PageBudget};

use super::objects::Kind;

/// What a cap counts. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// Object-table entries of every kind.
    Objects,
    /// `VkDevice`s.
    Devices,
    /// `VkDeviceMemory` objects (a WDDM allocation each on Windows).
    Memories,
    /// `VkPipeline`s.
    Pipelines,
    /// `VkShaderModule`s.
    ShaderModules,
    /// `VkPipelineCache`s.
    PipelineCaches,
    /// `VkDescriptorPool`s.
    DescriptorPools,
    /// `VkQueryPool`s.
    QueryPools,
    /// `VkCommandPool`s.
    CommandPools,
    /// `VkCommandBuffer`s.
    CommandBuffers,
    /// `VkFence`s.
    Fences,
    /// `VkSemaphore`s.
    Semaphores,
    /// `VkEvent`s.
    Events,
    /// Bytes of SPIR-V and of pipeline-cache initial data.
    ShaderBytes,
    /// Descriptor-pool capacity: `maxSets` plus every pool size's
    /// `descriptorCount` (bytes, for an inline uniform block).
    Descriptors,
    /// Query-pool slots: `queryCount` times the values one query answers.
    Queries,
    /// Wire bytes of the commands recorded into live command buffers.
    RecordingBytes,
    /// Host bytes decoded commands and copied command streams hold while
    /// they run.
    DecodeBytes,
    /// Guest submits (and the virtio-gpu fences behind them) held on the
    /// host until their waits can be met ([`super::hold`]).
    HeldSubmits,
    /// Wire bytes of those held submits.
    HeldBytes,
}

/// How many classes there are.
pub const CLASS_COUNT: usize = 20;

/// Every class, in [`Class::index`] order.
pub const CLASSES: [Class; CLASS_COUNT] = [
    Class::Objects,
    Class::Devices,
    Class::Memories,
    Class::Pipelines,
    Class::ShaderModules,
    Class::PipelineCaches,
    Class::DescriptorPools,
    Class::QueryPools,
    Class::CommandPools,
    Class::CommandBuffers,
    Class::Fences,
    Class::Semaphores,
    Class::Events,
    Class::ShaderBytes,
    Class::Descriptors,
    Class::Queries,
    Class::RecordingBytes,
    Class::DecodeBytes,
    Class::HeldSubmits,
    Class::HeldBytes,
];

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

impl Class {
    /// Its position in [`CLASSES`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Objects => 0,
            Self::Devices => 1,
            Self::Memories => 2,
            Self::Pipelines => 3,
            Self::ShaderModules => 4,
            Self::PipelineCaches => 5,
            Self::DescriptorPools => 6,
            Self::QueryPools => 7,
            Self::CommandPools => 8,
            Self::CommandBuffers => 9,
            Self::Fences => 10,
            Self::Semaphores => 11,
            Self::Events => 12,
            Self::ShaderBytes => 13,
            Self::Descriptors => 14,
            Self::Queries => 15,
            Self::RecordingBytes => 16,
            Self::DecodeBytes => 17,
            Self::HeldSubmits => 18,
            Self::HeldBytes => 19,
        }
    }

    /// Its name in the usage log and in refusals.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Objects => "objects",
            Self::Devices => "devices",
            Self::Memories => "memories",
            Self::Pipelines => "pipelines",
            Self::ShaderModules => "shader_modules",
            Self::PipelineCaches => "pipeline_caches",
            Self::DescriptorPools => "descriptor_pools",
            Self::QueryPools => "query_pools",
            Self::CommandPools => "command_pools",
            Self::CommandBuffers => "command_buffers",
            Self::Fences => "fences",
            Self::Semaphores => "semaphores",
            Self::Events => "events",
            Self::ShaderBytes => "shader_bytes",
            Self::Descriptors => "descriptors",
            Self::Queries => "queries",
            Self::RecordingBytes => "recording_bytes",
            Self::DecodeBytes => "decode_bytes",
            Self::HeldSubmits => "held_submits",
            Self::HeldBytes => "held_bytes",
        }
    }

    /// The default caps, `(per context, renderer-wide)`.
    ///
    /// The counts are generous fixed numbers: every per-context one at least
    /// 25 times what the heaviest context of the GPU-composited desktop
    /// (GNOME on Zink, glmark2, vkcube) was measured to hold — the
    /// amendment's table has the peaks — except devices, of which a context
    /// makes one. They bound a hostile guest, not a busy one. The
    /// renderer-wide ones are four times the per-context ones (sixteen for
    /// devices), so four contexts at their shares, or a desktop of ordinary
    /// clients beside one at its share, fit.
    #[must_use]
    pub const fn defaults(self) -> (u64, u64) {
        match self {
            // The table's own bound since stage 5a.3.
            Self::Objects => (1 << 16, 1 << 18),
            // A context makes one; a VkDevice is tens of MiB of driver state.
            Self::Devices => (4, 64),
            // NVIDIA's own maxMemoryAllocationCount is 4096 per device.
            Self::Memories => (4096, 16384),
            Self::Pipelines => (16384, 65536),
            Self::ShaderModules => (16384, 65536),
            Self::PipelineCaches => (256, 2048),
            Self::DescriptorPools => (4096, 16384),
            Self::QueryPools => (1024, 4096),
            Self::CommandPools => (1024, 4096),
            Self::CommandBuffers => (16384, 65536),
            Self::Fences => (16384, 65536),
            Self::Semaphores => (16384, 65536),
            Self::Events => (16384, 65536),
            Self::ShaderBytes => (256 * MIB, GIB),
            Self::Descriptors => (8 << 20, 32 << 20),
            Self::Queries => (1 << 20, 4 << 20),
            Self::RecordingBytes => (256 * MIB, GIB),
            Self::DecodeBytes => (512 * MIB, 2 * GIB),
            // A wait-before-signal holds a frame or two of submits (the
            // desktop held none at all); a guest past these ends its context
            // (`super::hold`'s module docs have the numbers).
            Self::HeldSubmits => (1024, 4096),
            Self::HeldBytes => (16 * MIB, 64 * MIB),
        }
    }

    /// What a command that creates something of this class answers when it
    /// is refused: what a driver out of that room says.
    #[must_use]
    pub const fn refusal(self) -> VkResult {
        match self {
            // `maxMemoryAllocationCount`'s answer.
            Self::Memories => VK_ERROR_TOO_MANY_OBJECTS,
            Self::Descriptors | Self::Queries => VK_ERROR_OUT_OF_DEVICE_MEMORY,
            _ => VK_ERROR_OUT_OF_HOST_MEMORY,
        }
    }
}

/// The class a table entry of `kind` counts under besides
/// [`Class::Objects`], if it has one.
#[must_use]
pub const fn class_of(kind: Kind) -> Option<Class> {
    match kind {
        Kind::Device => Some(Class::Devices),
        Kind::DeviceMemory => Some(Class::Memories),
        Kind::Pipeline => Some(Class::Pipelines),
        Kind::ShaderModule => Some(Class::ShaderModules),
        Kind::PipelineCache => Some(Class::PipelineCaches),
        Kind::DescriptorPool => Some(Class::DescriptorPools),
        Kind::QueryPool => Some(Class::QueryPools),
        Kind::CommandPool => Some(Class::CommandPools),
        Kind::CommandBuffer => Some(Class::CommandBuffers),
        Kind::Fence => Some(Class::Fences),
        Kind::Semaphore => Some(Class::Semaphores),
        Kind::Event => Some(Class::Events),
        _ => None,
    }
}

/// The part of one device-local heap every guest context together may
/// allocate, by default: three quarters. On the RTX 2070 that is 6 GiB of its
/// 8 GiB, leaving 2 GiB to the host, whose desktop (Windows, a browser, the
/// VMM's own window) was measured at 1.7 GiB in use.
pub const DEVICE_LOCAL_WHOLE: (u64, u64) = (3, 4);

/// The part of that one context may hold: three quarters again (4.5 GiB of
/// the RTX 2070's 6), so a game-sized client fits and a runaway one still
/// leaves the rest of the desktop a quarter. It is what the guest is told the
/// heap's size is ([`super::policy::guest_heaps`]).
pub const DEVICE_LOCAL_SHARE: (u64, u64) = (3, 4);

/// Most bytes one command's decode may allocate (the protocol's own maximum,
/// [`crate::venus::wire::MAX_TEMP_ALLOC_BYTES`], is the reference's 1 GiB):
/// 256 MiB, four times the largest command a stream can carry
/// ([`super::MAX_STREAM_BYTES`]) — a shader module's SPIR-V decodes one to
/// one — and far past anything else a command decodes to.
pub const MAX_COMMAND_DECODE_BYTES: usize = 256 << 20;

/// `a * num / den` without overflow for any heap a GPU reports.
const fn part(a: u64, (num, den): (u64, u64)) -> u64 {
    a / den * num + a % den * num / den
}

/// Which heap of which host GPU a device-local budget is for:
/// `(deviceUUID, heap index)`.
pub type HeapKey = ([u8; 16], u32);

/// The caps a renderer is built with: [`Class::defaults`], with any a test or
/// a profile overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    per_context: [u64; CLASS_COUNT],
    renderer: [u64; CLASS_COUNT],
    /// Device-local bytes every context together may allocate per heap, when
    /// the profile says (`[display] gpu_memory_mib`); otherwise
    /// [`DEVICE_LOCAL_WHOLE`] of the heap. Never more than the heap.
    device_local: Option<u64>,
}

impl Default for Caps {
    fn default() -> Self {
        let mut per_context = [0; CLASS_COUNT];
        let mut renderer = [0; CLASS_COUNT];
        for class in CLASSES {
            let (c, r) = class.defaults();
            per_context[class.index()] = c;
            renderer[class.index()] = r;
        }
        Self {
            per_context,
            renderer,
            device_local: None,
        }
    }
}

impl Caps {
    /// `class` capped at `per_context` per context and `renderer` in all.
    #[must_use]
    pub fn with(mut self, class: Class, per_context: u64, renderer: u64) -> Self {
        self.per_context[class.index()] = per_context;
        self.renderer[class.index()] = renderer;
        self
    }

    /// Device-local memory capped at `bytes` per heap for every context
    /// together (a profile's `gpu_memory_mib`), or the default fraction.
    #[must_use]
    pub fn with_device_local(mut self, bytes: Option<u64>) -> Self {
        self.device_local = bytes;
        self
    }

    /// `(per context, renderer-wide)` for `class`.
    #[must_use]
    pub fn of(&self, class: Class) -> (u64, u64) {
        (
            self.per_context[class.index()],
            self.renderer[class.index()],
        )
    }

    /// What every context together may allocate of a device-local heap of
    /// `heap` bytes.
    #[must_use]
    pub fn device_local_whole(&self, heap: u64) -> u64 {
        self.device_local
            .map_or_else(|| part(heap, DEVICE_LOCAL_WHOLE), |bytes| bytes.min(heap))
    }

    /// What one context may allocate of it — the heap size its guest is shown.
    #[must_use]
    pub fn device_local_share(&self, heap: u64) -> u64 {
        part(self.device_local_whole(heap), DEVICE_LOCAL_SHARE)
    }
}

/// Which level refused a charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The context's share.
    Context,
    /// The renderer-wide whole.
    Renderer,
}

/// A charge refused, and by what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// What was being charged.
    pub class: Class,
    /// How much.
    pub wanted: u64,
    /// What the refusing level held.
    pub used: u64,
    /// Its cap.
    pub limit: u64,
    /// Which level it was.
    pub level: Level,
}

impl Refusal {
    /// What the refused create answers ([`Class::refusal`]).
    #[must_use]
    pub fn result(&self) -> VkResult {
        self.class.refusal()
    }

    fn of(class: Class, wanted: u64, share: &PageBudget, (used, limit): (u64, u64)) -> Self {
        // `PageBudget::refuser`'s rule: the share if it cannot take it, the
        // whole behind it otherwise.
        let level =
            if share.whole().is_none() || share.used().saturating_add(wanted) > share.limit() {
                Level::Context
            } else {
                Level::Renderer
            };
        Self {
            class,
            wanted,
            used,
            limit,
            level,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} past the {} cap ({} held of {})",
            self.wanted,
            self.class.name(),
            match self.level {
                Level::Context => "per-context",
                Level::Renderer => "renderer-wide",
            },
            self.used,
            self.limit
        )
    }
}

/// The renderer-wide half: one budget per class, and one per device-local
/// heap of every host GPU a context has created a device on.
#[derive(Debug)]
pub struct Limits {
    caps: Caps,
    wholes: [Arc<PageBudget>; CLASS_COUNT],
    device_local: Mutex<HashMap<HeapKey, Arc<PageBudget>>>,
}

impl Limits {
    /// Budgets for `caps`, none of them used.
    #[must_use]
    pub fn new(caps: Caps) -> Arc<Self> {
        let wholes = std::array::from_fn(|i| PageBudget::new(caps.renderer[i]));
        Arc::new(Self {
            caps,
            wholes,
            device_local: Mutex::new(HashMap::new()),
        })
    }

    /// The caps.
    #[must_use]
    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// A new context's shares of every budget.
    #[must_use]
    pub fn context(self: &Arc<Self>) -> ContextLimits {
        let shares =
            std::array::from_fn(|i| PageBudget::share(&self.wholes[i], self.caps.per_context[i]));
        ContextLimits {
            renderer: Arc::clone(self),
            shares,
            device_local: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// The renderer-wide budget of device-local heap `key`, of `heap` bytes,
    /// made the first time a context asks.
    #[must_use]
    pub fn device_local(&self, key: HeapKey, heap: u64) -> Arc<PageBudget> {
        let mut map = self
            .device_local
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            map.entry(key)
                .or_insert_with(|| PageBudget::new(self.caps.device_local_whole(heap))),
        )
    }

    /// What every class and every device-local heap holds right now.
    #[must_use]
    pub fn usage(&self) -> LimitUsage {
        let mut usage = LimitUsage::default();
        for class in CLASSES {
            usage.used[class.index()] = self.wholes[class.index()].used();
            usage.high[class.index()] = self.wholes[class.index()].peak();
        }
        usage.device_local_bytes = self
            .device_local
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|b| b.used())
            .fold(0, u64::saturating_add);
        usage
    }
}

/// One context's shares. A clone is the same shares, not new ones.
#[derive(Debug, Clone)]
pub struct ContextLimits {
    renderer: Arc<Limits>,
    shares: [Arc<PageBudget>; CLASS_COUNT],
    /// A cell, so an allocation can find its heap's share while it holds
    /// references into the object table.
    device_local: std::cell::RefCell<HashMap<HeapKey, Arc<PageBudget>>>,
}

impl Default for ContextLimits {
    /// A context of a renderer of its own with the default caps: for a test
    /// that builds a context by hand.
    fn default() -> Self {
        Limits::new(Caps::default()).context()
    }
}

impl ContextLimits {
    /// The renderer these shares belong to.
    #[must_use]
    pub fn renderer(&self) -> &Arc<Limits> {
        &self.renderer
    }

    /// The context's share of `class`.
    #[must_use]
    pub fn share(&self, class: Class) -> &Arc<PageBudget> {
        &self.shares[class.index()]
    }

    /// Take `amount` of `class` from the share and the whole.
    ///
    /// # Errors
    /// Which level refused.
    pub fn charge(&self, class: Class, amount: u64) -> Result<Charge, Refusal> {
        let share = self.share(class);
        share
            .charge(amount)
            .map_err(|refused| Refusal::of(class, amount, share, refused))
    }

    /// Take `amount` of `class` whatever the caps say (see
    /// [`PageBudget::charge_anyway`]).
    #[must_use]
    pub fn charge_anyway(&self, class: Class, amount: u64) -> Charge {
        self.share(class).charge_anyway(amount)
    }

    /// Grow `charge` (of `class`) by `more`.
    ///
    /// # Errors
    /// Which level refused; the charge is unchanged.
    pub fn grow(&self, class: Class, charge: &mut Charge, more: u64) -> Result<(), Refusal> {
        let share = self.share(class);
        charge
            .grow(more)
            .map_err(|refused| Refusal::of(class, more, share, refused))
    }

    /// The context's share of device-local heap `key` of `heap` bytes.
    #[must_use]
    pub fn device_local(&self, key: HeapKey, heap: u64) -> Arc<PageBudget> {
        let renderer = &self.renderer;
        Arc::clone(
            self.device_local
                .borrow_mut()
                .entry(key)
                .or_insert_with(|| {
                    let whole = renderer.device_local(key, heap);
                    PageBudget::share(&whole, renderer.caps.device_local_share(heap))
                }),
        )
    }

    /// What the context holds of every class, and of device-local memory.
    #[must_use]
    pub fn usage(&self) -> LimitUsage {
        let mut usage = LimitUsage::default();
        for class in CLASSES {
            usage.used[class.index()] = self.shares[class.index()].used();
            usage.high[class.index()] = self.shares[class.index()].peak();
        }
        usage.device_local_bytes = self
            .device_local
            .borrow()
            .values()
            .map(|b| b.used())
            .fold(0, u64::saturating_add);
        usage
    }
}

/// What the caps of [`Limits`] bound, in use: renderer-wide, or — as
/// [`LimitUsage::max_context`] — the most one context holds. Part of the
/// usage log (`virtio_gpu::venus::usage`), which is how the peaks in the
/// amendment were measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitUsage {
    /// Per class, in [`CLASSES`] order.
    pub used: [u64; CLASS_COUNT],
    /// The high-water mark of each: the most ever held at once, however
    /// briefly (`PageBudget::peak`).
    pub high: [u64; CLASS_COUNT],
    /// Device-local bytes of every heap.
    pub device_local_bytes: u64,
    /// The most any one context holds of each, in the same order.
    pub max_context: [u64; CLASS_COUNT],
    /// The most device-local bytes one context holds.
    pub max_context_device_local_bytes: u64,
}

impl LimitUsage {
    /// Field by field, the larger of the two.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            used: std::array::from_fn(|i| self.used[i].max(other.used[i])),
            high: std::array::from_fn(|i| self.high[i].max(other.high[i])),
            device_local_bytes: self.device_local_bytes.max(other.device_local_bytes),
            max_context: std::array::from_fn(|i| self.max_context[i].max(other.max_context[i])),
            max_context_device_local_bytes: self
                .max_context_device_local_bytes
                .max(other.max_context_device_local_bytes),
        }
    }

    /// Fold one context's usage into the per-context maxima: the most it
    /// has ever held of each class.
    pub fn note_context(&mut self, context: &Self) {
        for i in 0..CLASS_COUNT {
            self.max_context[i] = self.max_context[i]
                .max(context.used[i])
                .max(context.high[i]);
        }
        self.max_context_device_local_bytes = self
            .max_context_device_local_bytes
            .max(context.device_local_bytes);
    }

    /// Whether nothing is held right now (high-water marks aside).
    #[must_use]
    pub fn holds_nothing(&self) -> bool {
        self.used.iter().all(|u| *u == 0) && self.device_local_bytes == 0
    }

    /// What `class` holds renderer-wide.
    #[must_use]
    pub fn of(&self, class: Class) -> u64 {
        self.used[class.index()]
    }

    /// The most one context holds of `class`.
    #[must_use]
    pub fn context_max(&self, class: Class) -> u64 {
        self.max_context[class.index()]
    }
}

impl fmt::Display for LimitUsage {
    /// `name=now/high-water/most-of-one-context …`, every class, then
    /// device-local bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for class in CLASSES {
            write!(
                f,
                "{}={}/{}/{} ",
                class.name(),
                self.used[class.index()],
                self.high[class.index()],
                self.max_context[class.index()]
            )?;
        }
        write!(
            f,
            "device_local_bytes={}/{}",
            self.device_local_bytes, self.max_context_device_local_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_is_at_its_index_and_every_default_share_fits_its_whole() {
        for (i, class) in CLASSES.iter().enumerate() {
            assert_eq!(class.index(), i, "{class:?}");
            let (c, r) = class.defaults();
            assert!(c > 0 && c <= r, "{class:?}: {c} per context, {r} in all");
        }
        // The table's bound, which the arena's output bound also names.
        assert_eq!(
            Class::Objects.defaults().0,
            super::super::objects::MAX_OBJECTS_PER_CONTEXT as u64
        );
    }

    #[test]
    fn device_local_budgets_are_parts_of_the_heap_and_a_profile_may_set_the_whole() {
        let caps = Caps::default();
        let heap = 8 << 30;
        assert_eq!(caps.device_local_whole(heap), 6 << 30);
        assert_eq!(caps.device_local_share(heap), (6 << 30) / 4 * 3);
        // The BAR heap of the RTX 2070.
        assert_eq!(caps.device_local_whole(224_395_264), 168_296_448);
        let caps = Caps::default().with_device_local(Some(2 << 30));
        assert_eq!(caps.device_local_whole(heap), 2 << 30);
        assert_eq!(
            caps.device_local_whole(1 << 30),
            1 << 30,
            "never past the heap"
        );
        assert_eq!(part(u64::MAX, (3, 4)), u64::MAX / 4 * 3 + 2);
    }

    #[test]
    fn a_share_is_refused_at_itself_and_the_whole_at_itself_and_both_come_back() {
        let limits = Limits::new(Caps::default().with(Class::Devices, 2, 3));
        let a = limits.context();
        let b = limits.context();
        let one = a.charge(Class::Devices, 1).expect("one");
        let two = a.charge(Class::Devices, 1).expect("two");
        let refused = a.charge(Class::Devices, 1).expect_err("past the share");
        assert_eq!(refused.level, Level::Context);
        assert_eq!(refused.result(), VK_ERROR_OUT_OF_HOST_MEMORY);
        let three = b.charge(Class::Devices, 1).expect("the whole's last");
        let refused = b.charge(Class::Devices, 1).expect_err("past the whole");
        assert_eq!(refused.level, Level::Renderer, "{refused}");
        assert_eq!(limits.usage().of(Class::Devices), 3);
        drop((one, two, three));
        assert_eq!(limits.usage().of(Class::Devices), 0);
        assert_eq!(a.usage().of(Class::Devices), 0);
    }

    #[test]
    fn device_local_shares_are_per_heap_and_per_gpu() {
        let limits = Limits::new(Caps::default());
        let ctx = limits.context();
        let heap0 = ctx.device_local(([1; 16], 0), 8 << 30);
        let again = ctx.device_local(([1; 16], 0), 8 << 30);
        assert!(Arc::ptr_eq(&heap0, &again));
        let other_gpu = ctx.device_local(([2; 16], 0), 8 << 30);
        assert!(!Arc::ptr_eq(&heap0, &other_gpu));
        let held = heap0.charge(1 << 30).expect("1 GiB");
        assert_eq!(limits.usage().device_local_bytes, 1 << 30);
        assert_eq!(ctx.usage().device_local_bytes, 1 << 30);
        drop(held);
        assert_eq!(limits.usage().device_local_bytes, 0);
    }
}
