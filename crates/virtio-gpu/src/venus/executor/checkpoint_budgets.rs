//! Stable budget-owner identities. Context ids can be reused while an old
//! export still holds its old share; equal limits do not mean equal owners.
//! Counters are reconstructed from actual live allocations, never trusted from
//! the file. Roots resolve against the new factory's configured caps first.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::checkpoint::RecipeError;
use super::host::HostVulkan;
use super::limits::{HeapKey, CLASS_COUNT};
use super::{ExecutorFactory, VulkanContext};
use crate::venus::shmem::PageBudget;
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_ROOTS: usize = 4096;
const MAX_SHARES: usize = 1 << 16;
const MAX_BYTES: usize = 12 + MAX_ROOTS * 40 + MAX_SHARES * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BudgetRootKey {
    HostPages,
    Class(u32),
    Heap(HeapKey),
}

/// Trusted runtime policy, not file-provided limits. The share cap is clamped
/// to the whole exactly as PageBudget::share clamps it during normal execution.
#[derive(Debug, Clone)]
pub struct BudgetRoot {
    pub key: BudgetRootKey,
    pub whole: Arc<PageBudget>,
    pub share_limit: u64,
}

#[derive(Default)]
pub struct BudgetTable {
    roots: Vec<BudgetRoot>,
    shares: Vec<(u32, Arc<PageBudget>)>,
    by_identity: HashMap<usize, u32>,
}

/// A not-yet-adopted cold context owns native resources too. Ensure an error
/// anywhere in reconstruction destroys them through the normal deferred path.
pub struct CheckpointContext<H: HostVulkan> {
    context: Option<VulkanContext<H>>,
}

impl<H: HostVulkan> std::ops::Deref for CheckpointContext<H> {
    type Target = VulkanContext<H>;
    fn deref(&self) -> &Self::Target {
        self.context.as_ref().expect("context not yet adopted")
    }
}

impl<H: HostVulkan> std::ops::DerefMut for CheckpointContext<H> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.context.as_mut().expect("context not yet adopted")
    }
}

impl<H: HostVulkan> Drop for CheckpointContext<H> {
    fn drop(&mut self) {
        if let Some(context) = self.context.as_mut() {
            context.destroy_all();
        }
    }
}

struct RootRecord {
    key: BudgetRootKey,
    whole_limit: u64,
    share_limit: u64,
}

/// Live context's exact share identities. Orphan allocations can retain other
/// owners in BudgetTable without attaching them to a new use of this ctx id.
#[derive(Debug, Clone)]
pub struct ContextBudgetLinks {
    pub(super) host_pages: u32,
    pub(super) classes: [u32; CLASS_COUNT],
    pub(super) heaps: Vec<(HeapKey, u32)>,
}

impl ContextBudgetLinks {
    pub(super) fn validate(&self, table: &BudgetTable) -> Result<(), RecipeError> {
        let valid = |id, key| -> Result<(), RecipeError> {
            if table.root_key(id)? != key || table.budget(id)?.whole().is_none() {
                return Err(invalid(
                    "context budget has the wrong role or is not a share",
                ));
            }
            Ok(())
        };
        valid(self.host_pages, BudgetRootKey::HostPages)?;
        for (index, id) in self.classes.iter().enumerate() {
            valid(*id, BudgetRootKey::Class(index as u32))?;
        }
        if self.heaps.len() > MAX_ROOTS {
            return Err(RecipeError::Budget);
        }
        let mut heaps = HashSet::new();
        heaps
            .try_reserve(self.heaps.len())
            .map_err(|_| RecipeError::Budget)?;
        for (key, id) in &self.heaps {
            if !heaps.insert(*key) {
                return Err(invalid("duplicate context heap budget"));
            }
            valid(*id, BudgetRootKey::Heap(*key))?;
        }
        Ok(())
    }

    pub fn encode(&self, table: &BudgetTable) -> Result<Vec<u8>, RecipeError> {
        self.validate(table)?;
        let mut encoder = Encoder::with_limit(16 + CLASS_COUNT * 4 + MAX_ROOTS * 24);
        encoder.u32(VERSION)?;
        encoder.u32(self.host_pages)?;
        encoder.u32(CLASS_COUNT as u32)?;
        for id in self.classes {
            encoder.u32(id)?;
        }
        encoder.u32(self.heaps.len() as u32)?;
        for ((uuid, heap), id) in &self.heaps {
            for word in uuid.chunks_exact(4) {
                encoder.u32(u32::from_le_bytes(
                    word.try_into().expect("four UUID bytes"),
                ))?;
            }
            encoder.u32(*heap)?;
            encoder.u32(*id)?;
        }
        Ok(encoder.finish()?)
    }

    pub fn decode(bytes: &[u8], table: &BudgetTable) -> Result<Self, RecipeError> {
        if bytes.len() > 16 + CLASS_COUNT * 4 + MAX_ROOTS * 24 {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let host_pages = decoder.u32()?;
        if decoder.u32()? as usize != CLASS_COUNT {
            return Err(invalid("context budget class schema differs"));
        }
        let mut classes = [0; CLASS_COUNT];
        for id in &mut classes {
            *id = decoder.u32()?;
        }
        let count = decoder.u32()? as usize;
        if count > MAX_ROOTS || count > decoder.remaining() / 24 {
            return Err(RecipeError::Budget);
        }
        let mut heaps = Vec::new();
        reserve(&mut heaps, count)?;
        for _ in 0..count {
            let mut uuid = [0; 16];
            for word in uuid.chunks_exact_mut(4) {
                word.copy_from_slice(&decoder.u32()?.to_le_bytes());
            }
            heaps.push(((uuid, decoder.u32()?), decoder.u32()?));
        }
        if decoder.position() != bytes.len() {
            return Err(invalid("trailing context budget links"));
        }
        let links = Self {
            host_pages,
            classes,
            heaps,
        };
        links.validate(table)?;
        Ok(links)
    }
}

fn invalid(reason: &'static str) -> RecipeError {
    RecipeError::Invalid(reason)
}
fn identity(budget: &Arc<PageBudget>) -> usize {
    Arc::as_ptr(budget) as usize
}
fn reserve<T>(vec: &mut Vec<T>, n: usize) -> Result<(), RecipeError> {
    vec.try_reserve(n).map_err(|_| RecipeError::Budget)
}

impl BudgetRootKey {
    fn validate(self) -> Result<(), RecipeError> {
        if matches!(self, Self::Class(index) if index as usize >= CLASS_COUNT) {
            return Err(invalid("unknown budget class"));
        }
        Ok(())
    }

    fn encode(self, encoder: &mut Encoder) -> Result<(), RecipeError> {
        let (kind, index, uuid) = match self {
            Self::HostPages => (0, 0, [0; 16]),
            Self::Class(index) => (1, index, [0; 16]),
            Self::Heap((uuid, index)) => (2, index, uuid),
        };
        encoder.u32(kind)?;
        encoder.u32(index)?;
        for word in uuid.chunks_exact(4) {
            encoder.u32(u32::from_le_bytes(
                word.try_into().expect("four UUID bytes"),
            ))?;
        }
        Ok(())
    }

    fn decode(decoder: &mut Decoder<'_>) -> Result<Self, RecipeError> {
        let kind = decoder.u32()?;
        let index = decoder.u32()?;
        let mut uuid = [0; 16];
        for word in uuid.chunks_exact_mut(4) {
            word.copy_from_slice(&decoder.u32()?.to_le_bytes());
        }
        let key = match kind {
            0 if index == 0 && uuid == [0; 16] => Self::HostPages,
            1 if uuid == [0; 16] => Self::Class(index),
            2 => Self::Heap((uuid, index)),
            _ => return Err(invalid("invalid budget root kind or reserved fields")),
        };
        key.validate()?;
        Ok(key)
    }
}

impl BudgetTable {
    /// Freeze the root set before registering owners: roots get the first dense
    /// ids, then distinct shares get distinct ids even when their caps match.
    pub fn new(roots: Vec<BudgetRoot>) -> Result<Self, RecipeError> {
        if roots.len() > MAX_ROOTS {
            return Err(RecipeError::Budget);
        }
        let mut keys = HashSet::new();
        keys.try_reserve(roots.len())
            .map_err(|_| RecipeError::Budget)?;
        let mut table = Self::default();
        reserve(&mut table.roots, roots.len())?;
        table
            .by_identity
            .try_reserve(roots.len())
            .map_err(|_| RecipeError::Budget)?;
        for mut root in roots {
            root.key.validate()?;
            if root.whole.whole().is_some()
                || !keys.insert(root.key)
                || table.by_identity.contains_key(&identity(&root.whole))
            {
                return Err(invalid("duplicate or non-root budget policy"));
            }
            root.share_limit = root.share_limit.min(root.whole.limit());
            let id = table.roots.len() as u32 + 1;
            table.by_identity.insert(identity(&root.whole), id);
            table.roots.push(root);
        }
        Ok(table)
    }

    pub fn register(&mut self, budget: &Arc<PageBudget>) -> Result<u32, RecipeError> {
        if let Some(id) = self.by_identity.get(&identity(budget)) {
            return Ok(*id);
        }
        let parent = budget
            .whole()
            .ok_or(invalid("budget has no registered factory root"))?;
        let root_id = *self
            .by_identity
            .get(&identity(parent))
            .ok_or(invalid("budget belongs to an unknown factory root"))?;
        let root = root_id
            .checked_sub(1)
            .and_then(|id| self.roots.get(id as usize))
            .ok_or(invalid("nested budget shares are unsupported"))?;
        if budget.limit() != root.share_limit {
            return Err(invalid(
                "budget share cap differs from the configured policy",
            ));
        }
        if self.shares.len() >= MAX_SHARES {
            return Err(RecipeError::Budget);
        }
        reserve(&mut self.shares, 1)?;
        self.by_identity
            .try_reserve(1)
            .map_err(|_| RecipeError::Budget)?;
        let id = (self.roots.len() + self.shares.len()) as u32 + 1;
        self.by_identity.insert(identity(budget), id);
        self.shares.push((root_id, budget.clone()));
        Ok(id)
    }

    pub fn budget(&self, id: u32) -> Result<&Arc<PageBudget>, RecipeError> {
        let index = id.checked_sub(1).ok_or(invalid("zero budget owner id"))? as usize;
        if let Some(root) = self.roots.get(index) {
            return Ok(&root.whole);
        }
        index
            .checked_sub(self.roots.len())
            .and_then(|index| self.shares.get(index))
            .map(|(_, budget)| budget)
            .ok_or(invalid("unknown budget owner id"))
    }

    pub fn root_key(&self, id: u32) -> Result<BudgetRootKey, RecipeError> {
        let index = id.checked_sub(1).ok_or(invalid("zero budget owner id"))? as usize;
        if let Some(root) = self.roots.get(index) {
            return Ok(root.key);
        }
        let (root, _) = index
            .checked_sub(self.roots.len())
            .and_then(|index| self.shares.get(index))
            .ok_or(invalid("unknown budget owner id"))?;
        Ok(self.roots[*root as usize - 1].key)
    }

    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        encoder.u32(self.roots.len() as u32)?;
        encoder.u32(self.shares.len() as u32)?;
        for root in &self.roots {
            root.key.encode(&mut encoder)?;
            encoder.u64(root.whole.limit())?;
            encoder.u64(root.share_limit)?;
        }
        for (root, _) in &self.shares {
            encoder.u32(*root)?;
        }
        Ok(encoder.finish()?)
    }

    /// Resolve roots by role only. The callback never sees file-provided caps;
    /// it must return the new factory's trusted, currently unused root policy.
    /// Validate all framing/links before invoking it or making any new shares.
    pub fn restore(
        bytes: &[u8],
        mut resolve: impl FnMut(BudgetRootKey) -> Result<BudgetRoot, RecipeError>,
    ) -> Result<Self, RecipeError> {
        if bytes.len() > MAX_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let root_count = decoder.u32()? as usize;
        let share_count = decoder.u32()? as usize;
        if root_count > MAX_ROOTS || share_count > MAX_SHARES {
            return Err(RecipeError::Budget);
        }
        let mut roots = Vec::new();
        let mut shares = Vec::new();
        let mut keys = HashSet::new();
        reserve(&mut roots, root_count)?;
        reserve(&mut shares, share_count)?;
        keys.try_reserve(root_count)
            .map_err(|_| RecipeError::Budget)?;
        for _ in 0..root_count {
            let key = BudgetRootKey::decode(&mut decoder)?;
            let whole_limit = decoder.u64()?;
            let share_limit = decoder.u64()?;
            if !keys.insert(key) || share_limit > whole_limit {
                return Err(invalid("duplicate budget root or invalid share cap"));
            }
            roots.push(RootRecord {
                key,
                whole_limit,
                share_limit,
            });
        }
        for _ in 0..share_count {
            let root = decoder.u32()?;
            if root == 0 || root as usize > root_count {
                return Err(invalid("budget share has an unknown or nested parent"));
            }
            shares.push(root);
        }
        if decoder.position() != bytes.len() {
            return Err(invalid("trailing budget owner data"));
        }
        let mut policies = Vec::new();
        reserve(&mut policies, roots.len())?;
        for saved in roots {
            let mut actual = resolve(saved.key)?;
            actual.share_limit = actual.share_limit.min(actual.whole.limit());
            if actual.key != saved.key
                || actual.whole.limit() != saved.whole_limit
                || actual.share_limit != saved.share_limit
                || actual.whole.used() != 0
            {
                return Err(invalid("budget root policy changed or is already in use"));
            }
            policies.push(actual);
        }
        let mut restored = Self::new(policies)?;
        for root in shares {
            let policy = &restored.roots[root as usize - 1];
            let share = PageBudget::share(&policy.whole, policy.share_limit);
            restored.register(&share)?;
        }
        Ok(restored)
    }
}

impl<H: HostVulkan> VulkanContext<H> {
    pub fn checkpoint_budget_links(
        &self,
        table: &mut BudgetTable,
    ) -> Result<ContextBudgetLinks, RecipeError> {
        self.objects
            .limits()
            .checkpoint_budget_links(&self.budget, table)
    }
}

impl<H: HostVulkan> ExecutorFactory<H> {
    pub fn checkpoint_budget_table(&self) -> Result<BudgetTable, RecipeError> {
        let mut roots = self.limits.checkpoint_roots()?;
        roots.push(BudgetRoot {
            key: BudgetRootKey::HostPages,
            whole: self.budget.clone(),
            share_limit: self.context_share,
        });
        BudgetTable::new(roots)
    }

    pub fn restore_budget_table(&self, bytes: &[u8]) -> Result<BudgetTable, RecipeError> {
        let mut devices = None;
        BudgetTable::restore(bytes, |key| match key {
            BudgetRootKey::HostPages => Ok(BudgetRoot {
                key,
                whole: self.budget.clone(),
                share_limit: self.context_share,
            }),
            BudgetRootKey::Class(index) => self.limits.checkpoint_class_root(index),
            BudgetRootKey::Heap((uuid, index)) => {
                if devices.is_none() {
                    devices = Some(super::probe(self.host.as_ref()).map_err(RecipeError::Replay)?);
                }
                let guest = devices
                    .as_ref()
                    .ok_or(invalid("budget GPU probe is absent"))?
                    .iter()
                    .find(|guest| super::memory::heap_gpu(guest) == uuid)
                    .ok_or(invalid("checkpoint budget GPU is unavailable"))?;
                let heap = guest
                    .host_memory
                    .memory_heaps
                    .get(index as usize)
                    .filter(|_| index < guest.host_memory.memory_heap_count)
                    .filter(|heap| heap.flags & super::policy::MEMORY_HEAP_DEVICE_LOCAL != 0)
                    .ok_or(invalid("checkpoint device-local heap is unavailable"))?;
                Ok(self.limits.checkpoint_heap_root((uuid, index), heap.size))
            }
        })
    }

    /// Construct an empty native context with the recorded owner shares. The
    /// caller replays its object state before adopting it and starting rings.
    pub fn new_checkpoint_context(
        &self,
        ctx_id: u32,
        links: &ContextBudgetLinks,
        table: &BudgetTable,
    ) -> Result<CheckpointContext<H>, RecipeError> {
        if ctx_id == 0 || self.contexts.contains_key(&ctx_id) {
            return Err(invalid("checkpoint context identity is not fresh"));
        }
        links.validate(table)?;
        let host = table.budget(links.host_pages)?;
        if host
            .whole()
            .is_none_or(|whole| !Arc::ptr_eq(whole, &self.budget))
            || host.limit() != self.context_share.min(self.budget.limit())
        {
            return Err(invalid("host-page share belongs to another factory or cap"));
        }
        let limits = self.limits.restore_context_budget_links(links, table)?;
        let mut context = VulkanContext::with_caps(
            ctx_id,
            self.host.clone(),
            host.clone(),
            self.fence_threads.clone(),
            limits,
        );
        context.payloads = self.payloads.clone();
        context.graveyard = self.graveyard.clone();
        context.hold_stats = self.hold_stats.clone();
        context.strict_checkpoint_budgets = true;
        Ok(CheckpointContext {
            context: Some(context),
        })
    }

    /// Commit a reconstructed context only to the factory whose roots and
    /// shared scheduling services it was built with. Failure cleans it up.
    pub fn adopt_checkpoint_context(
        &mut self,
        mut owner: CheckpointContext<H>,
    ) -> Result<(), RecipeError> {
        let context = &*owner;
        let id = context.ctx_id;
        if id == 0
            || self.contexts.contains_key(&id)
            || self.contexts.len() >= crate::venus::renderer::MAX_VENUS_CONTEXTS
            || !context.strict_checkpoint_budgets
            || !Arc::ptr_eq(&context.host, &self.host)
            || !Arc::ptr_eq(context.objects.limits().renderer(), &self.limits)
            || context
                .budget
                .whole()
                .is_none_or(|root| !Arc::ptr_eq(root, &self.budget))
            || !Arc::ptr_eq(&context.payloads, &self.payloads)
            || !Arc::ptr_eq(&context.graveyard, &self.graveyard)
            || !Arc::ptr_eq(&context.hold_stats, &self.hold_stats)
            || !Arc::ptr_eq(&context.fence_threads, &self.fence_threads)
        {
            return Err(invalid(
                "context belongs to another factory or is not a fresh checkpoint",
            ));
        }
        self.contexts
            .try_reserve(1)
            .map_err(|_| RecipeError::Budget)?;
        let context = owner
            .context
            .take()
            .ok_or(invalid("context was already adopted"))?;
        self.contexts
            .insert(id, Arc::new(std::sync::Mutex::new(context)));
        self.checkpoint_seen_contexts = true;
        Ok(())
    }

    pub fn checkpoint_payload_limits(
        &self,
        owner: u32,
        table: &BudgetTable,
    ) -> Result<super::limits::ContextLimits, RecipeError> {
        self.limits.checkpoint_payload_limits(owner, table)
    }
}

#[cfg(test)]
mod tests;
