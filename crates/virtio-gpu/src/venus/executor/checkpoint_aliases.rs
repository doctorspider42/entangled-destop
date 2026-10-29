//! Readback safety for resources sharing an allocation. Sharing a heap block
//! is not aliasing when native requirement ranges are disjoint. Linear versus
//! non-linear resources additionally include bufferImageGranularity pages.
use super::checkpoint::RecipeError;
use super::{HostVulkan, VulkanContext};
use crate::venus::protocol::VkMemoryRequirements2;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const MAX_BINDINGS: usize = 1 << 18;

#[derive(Clone, Copy, Debug)]
enum Resource {
    Image(u64),
    Buffer(u64),
}
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u64,
    end: u64,
    granule_start: u64,
    granule_end: u64,
    linear: bool,
    resource: Resource,
}

#[derive(Default)]
pub(super) struct BindingAliases {
    images: HashSet<u64>,
    buffers: HashSet<u64>,
}
impl BindingAliases {
    fn mark(&mut self, resource: Resource) {
        match resource {
            Resource::Image(id) => {
                self.images.insert(id);
            }
            Resource::Buffer(id) => {
                self.buffers.insert(id);
            }
        }
    }
    pub fn image(&self, id: u64) -> bool {
        self.images.contains(&id)
    }
    pub fn buffer(&self, id: u64) -> bool {
        self.buffers.contains(&id)
    }
}

fn invalid(why: &'static str) -> RecipeError {
    RecipeError::Invalid(why)
}
fn span(
    offset: u64,
    size: u64,
    allocation: u64,
    granularity: u64,
    resource: Resource,
    linear: bool,
) -> Result<Span, RecipeError> {
    let end = offset
        .checked_add(size)
        .filter(|end| size != 0 && *end <= allocation)
        .ok_or(invalid("native binding requirement exceeds its allocation"))?;
    let granularity = granularity.max(1);
    Ok(Span {
        start: offset,
        end,
        granule_start: offset / granularity * granularity,
        granule_end: end
            .checked_next_multiple_of(granularity)
            .ok_or(invalid("native binding granularity overflows"))?,
        resource,
        linear,
    })
}

fn classify(
    groups: &mut HashMap<(u64, usize), Vec<Span>>,
    count: usize,
) -> Result<BindingAliases, RecipeError> {
    let mut result = BindingAliases::default();
    result
        .images
        .try_reserve(count)
        .map_err(|_| RecipeError::Budget)?;
    result
        .buffers
        .try_reserve(count)
        .map_err(|_| RecipeError::Budget)?;
    for group in groups.values_mut() {
        group.sort_unstable_by_key(|span| (span.start, span.end));
        let mut furthest = 0;
        let mut image_end = 0;
        for (index, span) in group.iter().enumerate() {
            match span.resource {
                Resource::Image(id) => {
                    if furthest > span.start
                        || group
                            .get(index + 1)
                            .is_some_and(|next| next.start < span.end)
                    {
                        result.images.insert(id);
                    }
                    image_end = image_end.max(span.end);
                }
                Resource::Buffer(id) if image_end > span.start => {
                    result.buffers.insert(id);
                }
                Resource::Buffer(_) => {}
            }
            furthest = furthest.max(span.end);
        }
        let mut next_image = u64::MAX;
        for span in group.iter().rev() {
            match span.resource {
                Resource::Image(_) => next_image = next_image.min(span.start),
                Resource::Buffer(id) if next_image < span.end => {
                    result.buffers.insert(id);
                }
                Resource::Buffer(_) => {}
            }
        }
        // Same-linearity resources use byte ranges. Only the opposite class
        // extends to granularity pages (Vulkan Resource Memory Aliasing).
        group.sort_unstable_by_key(|span| (span.granule_start, span.granule_end));
        let mut ends = [0; 2];
        for span in group.iter() {
            let class = usize::from(span.linear);
            if ends[1 - class] > span.granule_start {
                result.mark(span.resource);
            }
            ends[class] = ends[class].max(span.granule_end);
        }
        let mut starts = [u64::MAX; 2];
        for span in group.iter().rev() {
            let class = usize::from(span.linear);
            if starts[1 - class] < span.granule_end {
                result.mark(span.resource);
            }
            starts[class] = starts[class].min(span.granule_start);
        }
    }
    Ok(result)
}

impl<H: HostVulkan> VulkanContext<H> {
    /// Query only typed live resources, after validating the allocation's
    /// generation. Native requirement padding belongs to the resource too.
    pub(super) fn checkpoint_binding_aliases(&self) -> Result<BindingAliases, RecipeError> {
        let mut groups: HashMap<(u64, usize), Vec<Span>> = HashMap::new();
        let mut count = 0;
        let mut push = |memory, identity: &Arc<()>, span| -> Result<(), RecipeError> {
            if count >= MAX_BINDINGS {
                return Err(RecipeError::Budget);
            }
            groups.try_reserve(1).map_err(|_| RecipeError::Budget)?;
            let group = groups
                .entry((memory, Arc::as_ptr(identity) as usize))
                .or_default();
            group.try_reserve(1).map_err(|_| RecipeError::Budget)?;
            group.push(span);
            count += 1;
            Ok(())
        };
        for (id, image) in self.objects.checkpoint_images() {
            if image.bindings.len() != image.binding_identities.len() {
                return Err(invalid("image binding generations are incomplete"));
            }
            for (binding, identity) in image.bindings.iter().zip(&image.binding_identities) {
                let Ok(memory) = self.objects.memory(image.device, binding.memory) else {
                    continue;
                };
                if !Arc::ptr_eq(identity, &memory.identity) {
                    continue;
                }
                let (device, guest) = self
                    .objects
                    .device_and_guest(image.device)
                    .map_err(|error| RecipeError::Replay(error.to_string()))?;
                let granularity = guest.properties.properties.limits.buffer_image_granularity;
                let (offset, size) = if image.facts.planes == 1 {
                    let mut requirements = VkMemoryRequirements2::default();
                    self.host.image_memory_requirements(
                        &device.host,
                        image.host,
                        None,
                        &mut requirements,
                    );
                    (binding.offset, requirements.memory_requirements.size)
                } else {
                    // Plane order is not stored in this old binding table.
                    // Multi-plane capture remains unsupported, and sharing
                    // with any of its planes cannot authorize buffer readback.
                    (0, memory.host_size)
                };
                push(
                    binding.memory,
                    identity,
                    span(
                        offset,
                        size,
                        memory.host_size,
                        granularity,
                        Resource::Image(id),
                        image.facts.tiling == 1,
                    )?,
                )?;
            }
        }
        for (id, buffer) in self.objects.checkpoint_buffers() {
            let (Some(binding), Some(identity)) = (buffer.bound, &buffer.bound_identity) else {
                continue;
            };
            let Ok(memory) = self.objects.memory(buffer.device, binding.memory) else {
                continue;
            };
            if !Arc::ptr_eq(identity, &memory.identity) {
                continue;
            }
            let (device, guest) = self
                .objects
                .device_and_guest(buffer.device)
                .map_err(|error| RecipeError::Replay(error.to_string()))?;
            let mut requirements = VkMemoryRequirements2::default();
            self.host
                .buffer_memory_requirements(&device.host, buffer.host, &mut requirements);
            push(
                binding.memory,
                identity,
                span(
                    binding.offset,
                    requirements.memory_requirements.size,
                    memory.host_size,
                    guest.properties.properties.limits.buffer_image_granularity,
                    Resource::Buffer(id),
                    true,
                )?,
            )?;
        }
        classify(&mut groups, count)
    }
}

#[cfg(test)]
mod tests;
