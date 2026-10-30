//! Reachability of closed object lifetimes after command/descriptor compaction.
//! Keep creation inputs and destruction order for every reachable generation;
//! never identify a reused guest handle with its previous native object.
//!
//! Allocation generations are explicit in v3 recipes. Dropping a dead
//! allocation cannot renumber surviving page/shared-payload ownership links.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::venus::executor::context::{invalid, ExecError};
use crate::venus::executor::generated::Resolve as _;
use crate::venus::protocol::*;

type Key = (Kind, u64);

#[derive(Default)]
struct References(Vec<Key>);
impl generated::Resolve for References {
    fn handle(&mut self, kind: Kind, id: u64, _: bool, _: &'static str) -> Result<u64, ExecError> {
        if id != 0 {
            self.0.push((kind, id));
        }
        Ok(id)
    }
    fn invalid(&self, what: String) -> ExecError {
        invalid("checkpoint dependencies", what)
    }
    fn link(&self, parent: &'static str, stype: i32) -> ExecError {
        self.invalid(format!("unknown link {stype} in {parent}"))
    }
    fn enabled(&self, _: &'static str) -> bool {
        true
    }
}

fn references(cmd: &mut Command<'_>) -> Option<Vec<Key>> {
    // Template data embeds handles whose layout only the executor's live
    // template knows. Do not guess them or prune any lifetime around them.
    if matches!(
        cmd.name(),
        "vkUpdateDescriptorSetWithTemplate" | "vkSetPrivateData"
    ) {
        return None;
    }
    let mut refs = References::default();
    match generated::translate(&mut refs, cmd) {
        Ok(()) => return Some(refs.0),
        Err(ExecError::NotImplemented { .. }) => {}
        Err(_) => return None,
    }
    // Hand-written executor paths, whose inputs are not walked by translate.
    match cmd {
        Command::CreateInstance(_)
        | Command::DestroyInstance(_)
        | Command::EnumeratePhysicalDevices(_)
        | Command::EnumeratePhysicalDeviceGroups(_)
        | Command::CreateDevice(_)
        | Command::DestroyDevice(_)
        | Command::GetDeviceQueue(_)
        | Command::GetDeviceQueue2(_)
        | Command::CreateCommandPool(_)
        | Command::CreateBuffer(_)
        | Command::CreateImage(_)
        | Command::FreeMemory(_) => {}
        Command::DestroyCommandPool(a) => refs.0.push((Kind::CommandPool, a.command_pool.0)),
        Command::ResetCommandPool(a) => refs.0.push((Kind::CommandPool, a.command_pool.0)),
        Command::TrimCommandPool(a) => refs.0.push((Kind::CommandPool, a.command_pool.0)),
        Command::AllocateCommandBuffers(a) => {
            if let Some(info) = &a.p_allocate_info {
                refs.0.push((Kind::CommandPool, info.command_pool.0));
            }
        }
        Command::FreeCommandBuffers(a) => {
            refs.0.push((Kind::CommandPool, a.command_pool.0));
            refs.0.extend(
                a.p_command_buffers
                    .iter()
                    .flatten()
                    .map(|id| (Kind::CommandBuffer, id.0)),
            );
        }
        Command::AllocateMemory(args) => {
            for link in args.p_allocate_info.iter().flat_map(|info| &info.p_next) {
                if let VkMemoryAllocateInfoNext::VkMemoryDedicatedAllocateInfo(info) = link {
                    refs.handle(Kind::Buffer, info.buffer.0, true, "dedicated buffer")
                        .ok()?;
                    refs.handle(Kind::Image, info.image.0, true, "dedicated image")
                        .ok()?;
                }
            }
        }
        Command::DestroyBuffer(args) => {
            refs.0.push((Kind::Buffer, args.buffer.0));
        }
        Command::DestroyImage(args) => {
            refs.0.push((Kind::Image, args.image.0));
        }
        Command::CreateBufferView(args) => {
            for info in args.p_create_info.iter() {
                refs.0.push((Kind::Buffer, info.buffer.0));
            }
        }
        Command::DestroyBufferView(args) => {
            refs.0.push((Kind::BufferView, args.buffer_view.0));
        }
        Command::CreateImageView(args) => {
            for info in args.p_create_info.iter() {
                refs.0.push((Kind::Image, info.image.0));
                for link in &info.p_next {
                    if let VkImageViewCreateInfoNext::VkSamplerYcbcrConversionInfo(info) = link {
                        refs.0
                            .push((Kind::SamplerYcbcrConversion, info.conversion.0));
                    }
                }
            }
        }
        Command::DestroyImageView(args) => {
            refs.0.push((Kind::ImageView, args.image_view.0));
        }
        Command::BindBufferMemory2(args) => {
            for info in args.p_bind_infos.iter().flatten() {
                refs.0.push((Kind::Buffer, info.buffer.0));
                refs.0.push((Kind::DeviceMemory, info.memory.0));
            }
        }
        Command::BindImageMemory2(args) => {
            for info in args.p_bind_infos.iter().flatten() {
                refs.0.push((Kind::Image, info.image.0));
                refs.0.push((Kind::DeviceMemory, info.memory.0));
            }
        }
        Command::ImportSemaphoreResourceMESA(args) => {
            for info in args.p_import_semaphore_resource_info.iter() {
                refs.0.push((Kind::Semaphore, info.semaphore.0));
            }
        }
        _ => return None,
    }
    Some(refs.0)
}

fn created(cmd: &mut Command<'_>) -> Vec<Key> {
    if let Command::CreateCommandPool(a) = cmd {
        return a
            .p_command_pool
            .iter()
            .map(|id| (Kind::CommandPool, id.0))
            .collect();
    }
    if let Command::AllocateMemory(args) = cmd {
        return args
            .p_memory
            .iter()
            .map(|id| (Kind::DeviceMemory, id.0))
            .collect();
    }
    if let Command::CreateBuffer(args) = cmd {
        return args
            .p_buffer
            .iter()
            .map(|id| (Kind::Buffer, id.0))
            .collect();
    }
    if let Command::CreateBufferView(args) = cmd {
        return args
            .p_view
            .iter()
            .map(|id| (Kind::BufferView, id.0))
            .collect();
    }
    if let Command::CreateImage(args) = cmd {
        return args.p_image.iter().map(|id| (Kind::Image, id.0)).collect();
    }
    if let Command::CreateImageView(args) = cmd {
        return args
            .p_view
            .iter()
            .map(|id| (Kind::ImageView, id.0))
            .collect();
    }
    if !cmd.name().starts_with("vkCreate")
        && !matches!(
            cmd,
            Command::AllocateCommandBuffers(_) | Command::AllocateDescriptorSets(_)
        )
    {
        return Vec::new();
    }
    let Some((kind, ids)) = generated::output_handles(cmd) else {
        return Vec::new();
    };
    if !matches!(
        kind,
        Kind::BufferView
            | Kind::CommandBuffer
            | Kind::DescriptorSet
            | Kind::DescriptorPool
            | Kind::ShaderModule
            | Kind::Pipeline
            | Kind::PipelineCache
            | Kind::Sampler
            | Kind::SamplerYcbcrConversion
            | Kind::PipelineLayout
            | Kind::DescriptorSetLayout
            | Kind::DescriptorUpdateTemplate
            | Kind::Fence
            | Kind::Semaphore
            | Kind::Event
            | Kind::QueryPool
    ) {
        return Vec::new();
    }
    ids.into_iter()
        .filter(|id| **id != 0)
        .map(|id| (kind, *id))
        .collect()
}

fn destroyed(cmd: &Command<'_>) -> Vec<Key> {
    match cmd {
        Command::FreeCommandBuffers(a) => {
            return a
                .p_command_buffers
                .iter()
                .flatten()
                .map(|id| (Kind::CommandBuffer, id.0))
                .collect()
        }
        Command::FreeDescriptorSets(a) => {
            return a
                .p_descriptor_sets
                .iter()
                .flatten()
                .map(|id| (Kind::DescriptorSet, id.0))
                .collect()
        }
        _ => {}
    }
    vec![match cmd {
        Command::DestroyCommandPool(a) => (Kind::CommandPool, a.command_pool.0),
        Command::DestroyDescriptorPool(a) => (Kind::DescriptorPool, a.descriptor_pool.0),
        Command::DestroyBuffer(a) => (Kind::Buffer, a.buffer.0),
        Command::FreeMemory(a) => (Kind::DeviceMemory, a.memory.0),
        Command::DestroyImage(a) => (Kind::Image, a.image.0),
        Command::DestroyImageView(a) => (Kind::ImageView, a.image_view.0),
        Command::DestroyBufferView(a) => (Kind::BufferView, a.buffer_view.0),
        Command::DestroyShaderModule(a) => (Kind::ShaderModule, a.shader_module.0),
        Command::DestroyPipeline(a) => (Kind::Pipeline, a.pipeline.0),
        Command::DestroyPipelineCache(a) => (Kind::PipelineCache, a.pipeline_cache.0),
        Command::DestroySampler(a) => (Kind::Sampler, a.sampler.0),
        Command::DestroySamplerYcbcrConversion(a) => {
            (Kind::SamplerYcbcrConversion, a.ycbcr_conversion.0)
        }
        Command::DestroyPipelineLayout(a) => (Kind::PipelineLayout, a.pipeline_layout.0),
        Command::DestroyDescriptorSetLayout(a) => {
            (Kind::DescriptorSetLayout, a.descriptor_set_layout.0)
        }
        Command::DestroyDescriptorUpdateTemplate(a) => (
            Kind::DescriptorUpdateTemplate,
            a.descriptor_update_template.0,
        ),
        Command::DestroyFence(a) => (Kind::Fence, a.fence.0),
        Command::DestroySemaphore(a) => (Kind::Semaphore, a.semaphore.0),
        Command::DestroyEvent(a) => (Kind::Event, a.event.0),
        Command::DestroyQueryPool(a) => (Kind::QueryPool, a.query_pool.0),
        _ => return Vec::new(),
    }]
}

fn allocation_pool(cmd: &Command<'_>) -> Option<Key> {
    match cmd {
        Command::AllocateCommandBuffers(a) => a
            .p_allocate_info
            .as_ref()
            .map(|i| (Kind::CommandPool, i.command_pool.0)),
        Command::AllocateDescriptorSets(a) => a
            .p_allocate_info
            .as_ref()
            .map(|i| (Kind::DescriptorPool, i.descriptor_pool.0)),
        _ => None,
    }
}

fn retired_pool(cmd: &Command<'_>) -> Option<Key> {
    match cmd {
        Command::DestroyCommandPool(a) => Some((Kind::CommandPool, a.command_pool.0)),
        Command::DestroyDescriptorPool(a) => Some((Kind::DescriptorPool, a.descriptor_pool.0)),
        Command::ResetDescriptorPool(a) => Some((Kind::DescriptorPool, a.descriptor_pool.0)),
        _ => None,
    }
}

fn targets(cmd: &Command<'_>) -> Vec<Key> {
    match cmd {
        Command::BindBufferMemory2(a) => a
            .p_bind_infos
            .iter()
            .flatten()
            .map(|i| (Kind::Buffer, i.buffer.0))
            .collect(),
        Command::BindImageMemory2(a) => a
            .p_bind_infos
            .iter()
            .flatten()
            .map(|i| (Kind::Image, i.image.0))
            .collect(),
        Command::ResetFences(a) => a
            .p_fences
            .iter()
            .flatten()
            .map(|id| (Kind::Fence, id.0))
            .collect(),
        Command::ImportSemaphoreResourceMESA(a) => a
            .p_import_semaphore_resource_info
            .iter()
            .map(|i| (Kind::Semaphore, i.semaphore.0))
            .collect(),
        Command::SetEvent(a) => vec![(Kind::Event, a.event.0)],
        Command::ResetEvent(a) => vec![(Kind::Event, a.event.0)],
        Command::SignalSemaphore(a) => a
            .p_signal_info
            .iter()
            .map(|i| (Kind::Semaphore, i.semaphore.0))
            .collect(),
        Command::MergePipelineCaches(a) => vec![(Kind::PipelineCache, a.dst_cache.0)],
        _ => Vec::new(),
    }
}

fn edge(edges: &mut [Vec<usize>], from: usize, to: usize) -> Result<(), RecipeError> {
    edges[from]
        .try_reserve(1)
        .map_err(|_| RecipeError::Budget)?;
    edges[from].push(to);
    Ok(())
}

pub(super) fn prune(records: &[Vec<u8>], keep: &mut [bool]) -> Result<(), RecipeError> {
    let mut edges = Vec::new();
    edges
        .try_reserve_exact(records.len())
        .map_err(|_| RecipeError::Budget)?;
    edges.resize_with(records.len(), Vec::new);
    let mut roots = Vec::new();
    roots
        .try_reserve_exact(records.len())
        .map_err(|_| RecipeError::Budget)?;
    let mut live: HashMap<Key, usize> = HashMap::new();
    let mut members: HashMap<Key, HashSet<Key>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if !keep[index] {
            continue;
        }
        let mut cmd = command(record)?;
        let Some(refs) = references(&mut cmd) else {
            tracing::debug!(
                command = cmd.name(),
                "retaining object lifetimes around an opaque reconstruction command"
            );
            return Ok(());
        };
        for id in refs {
            if let Some(producer) = live.get(&id) {
                edge(&mut edges, index, *producer)?;
            }
        }
        let outputs = created(&mut cmd);
        if !outputs.is_empty() {
            if let Some(pool) = allocation_pool(&cmd) {
                members.try_reserve(1).map_err(|_| RecipeError::Budget)?;
                let children = members.entry(pool).or_default();
                children
                    .try_reserve(outputs.len())
                    .map_err(|_| RecipeError::Budget)?;
                children.extend(outputs.iter().copied());
            }
            // Pipeline construction can modify the supplied cache. Its live
            // lifetime must keep those writes even when a pipeline was freed.
            let cache = match &cmd {
                Command::CreateGraphicsPipelines(a) => a.pipeline_cache.0,
                Command::CreateComputePipelines(a) => a.pipeline_cache.0,
                _ => 0,
            };
            if let Some(producer) = live.get(&(Kind::PipelineCache, cache)) {
                edge(&mut edges, *producer, index)?;
            }
            live.try_reserve(outputs.len())
                .map_err(|_| RecipeError::Budget)?;
            for id in outputs {
                live.insert(id, index);
            }
        } else {
            let mut retired = destroyed(&cmd);
            let pool = retired_pool(&cmd);
            if let Some(pool) = pool {
                if let Some(children) = members.remove(&pool) {
                    retired.extend(children);
                }
            }
            if !retired.is_empty() || pool.is_some() {
                for id in retired {
                    if let Some(producer) = live.remove(&id) {
                        // Keep explicit and implicit frees of selected batch
                        // siblings, before a guest handle/pool is reused.
                        edge(&mut edges, producer, index)?;
                    } else {
                        roots.push(index);
                    }
                    for children in members.values_mut() {
                        children.remove(&id);
                    }
                }
                // An empty descriptor-pool reset is unnecessary. A reset
                // needed by a retained older set is reached via its producer.
                continue;
            }
            let targets = targets(&cmd);
            if !targets.is_empty() && targets.iter().all(|id| live.contains_key(id)) {
                for id in targets {
                    if let Some(producer) = live.get(&id) {
                        edge(&mut edges, *producer, index)?;
                    }
                }
            } else {
                roots.push(index);
            }
        }
    }
    roots
        .try_reserve(live.len())
        .map_err(|_| RecipeError::Budget)?;
    roots.extend(live.values().copied());
    let mut marked = Vec::new();
    marked
        .try_reserve_exact(keep.len())
        .map_err(|_| RecipeError::Budget)?;
    marked.resize(keep.len(), false);
    while let Some(index) = roots.pop() {
        if marked[index] {
            continue;
        }
        marked[index] = true;
        roots
            .try_reserve(edges[index].len())
            .map_err(|_| RecipeError::Budget)?;
        roots.extend(edges[index].iter().copied());
    }
    let mut dropped = 0;
    for (retain, reachable) in keep.iter_mut().zip(marked) {
        if *retain && !reachable {
            *retain = false;
            dropped += 1;
        }
    }
    tracing::debug!(dropped, "discarded unreachable Venus object lifetimes");
    Ok(())
}
