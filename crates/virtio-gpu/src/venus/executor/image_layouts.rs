//! Executed image layouts/ownership. Recording a command is not execution:
//! changes are committed only for submits accepted by the host driver.
//! Rectangular spans keep whole-image barriers O(1), without a mip×layer array.

use std::collections::HashMap;
use std::sync::Arc;

use super::host::HostVulkan;
use super::limits::{Class, ContextLimits};
use super::objects::{Kind, Objects};
use crate::venus::protocol::*;
use crate::venus::shmem::Charge;

const IGNORED: u32 = u32::MAX;
const MAX_SPANS: usize = 1 << 16;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    Initial,
    Concurrent,
    Queue(u32),
    Released {
        from: u32,
        to: u32,
        old_layout: i32,
        new_layout: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    pub layout: i32,
    pub owner: Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    pub aspects: u32,
    pub mip: u32,
    pub mips: u32,
    pub layer: u32,
    pub layers: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub range: Range,
    pub cell: Cell,
}

#[derive(Debug, Clone)]
struct View {
    device: u64,
    image: u64,
    identity: Arc<()>,
    range: Range,
    all_aspects: u32,
    is_3d: bool,
}

#[derive(Debug, Clone)]
struct Op {
    image: View,
    layout: i32,
    old_layout: i32,
    from: u32,
    to: u32,
}

struct Retained<T> {
    device: u64,
    data: T,
    _charge: Charge,
}
struct AttachmentFinal {
    layout: i32,
    stencil: Option<i32>,
}
struct PassFinals {
    attachments: Vec<AttachmentFinal>,
    views: u32,
}
enum Framebuffer {
    Fixed { views: Vec<View>, layers: u32 },
    Imageless { layers: u32 },
}
struct Recording {
    ops: Vec<Op>,
    active_pass: Option<Arc<Retained<Vec<Op>>>>,
    charge: Charge,
}

pub(super) struct ImageState {
    pub device: u64,
    pub identity: Arc<()>,
    pub spans: Vec<Span>,
    all: Range,
    concurrent: bool,
    charge: Charge,
}

pub(super) struct Note(Option<Action>);
enum Action {
    Image {
        device: u64,
        id: u64,
        initial: i32,
        concurrent: bool,
    },
    View {
        id: u64,
        view: View,
        charge: Charge,
    },
    Pass {
        id: u64,
        retained: Arc<Retained<PassFinals>>,
    },
    Framebuffer {
        id: u64,
        retained: Arc<Retained<Framebuffer>>,
    },
    Begin {
        cb: u64,
        finals: Arc<Retained<Vec<Op>>>,
    },
    End {
        cb: u64,
    },
    Ops {
        cb: u64,
        ops: Vec<Op>,
        _charge: Charge,
    },
    Destroy {
        kind: Kind,
        id: u64,
    },
    Prune,
}

#[derive(Default)]
pub(super) struct ImageTracking {
    pub error: Option<String>,
    pub images: HashMap<u64, ImageState>,
    views: HashMap<u64, (View, Charge)>,
    passes: HashMap<u64, Arc<Retained<PassFinals>>>,
    framebuffers: HashMap<u64, Arc<Retained<Framebuffer>>>,
    recordings: HashMap<u64, Recording>,
}

fn charge(limits: &ContextLimits, count: usize, width: usize) -> Result<Charge, String> {
    let bytes = count
        .checked_mul(width)
        .and_then(|n| n.checked_add(64))
        .ok_or("image tracking size overflow")?;
    limits
        .charge(Class::RecordingBytes, bytes as u64)
        .map_err(|e| e.to_string())
}

fn aspects(format: i32) -> u32 {
    match format {
        124..=126 => 2,
        127 => 4,
        128..=130 => 6,
        _ => match super::memory::plane_count(format) {
            2 => 0x30,
            3 => 0x70,
            _ => 1,
        },
    }
}

fn normalize(range: &VkImageSubresourceRange, all: Range) -> Result<Range, String> {
    let mut mask = range.aspect_mask;
    if all.aspects & 0x70 != 0 && mask & 1 != 0 {
        mask = (mask & !1) | all.aspects;
    }
    let mips = if range.level_count == u32::MAX {
        all.mips
            .checked_sub(range.base_mip_level)
            .ok_or("mip start outside image")?
    } else {
        range.level_count
    };
    let layers = if range.layer_count == u32::MAX {
        all.layers
            .checked_sub(range.base_array_layer)
            .ok_or("layer start outside image")?
    } else {
        range.layer_count
    };
    if mask == 0
        || mask & !all.aspects != 0
        || mips == 0
        || layers == 0
        || range
            .base_mip_level
            .checked_add(mips)
            .is_none_or(|n| n > all.mips)
        || range
            .base_array_layer
            .checked_add(layers)
            .is_none_or(|n| n > all.layers)
    {
        return Err("image subresource range is outside its image".into());
    }
    Ok(Range {
        aspects: mask,
        mip: range.base_mip_level,
        mips,
        layer: range.base_array_layer,
        layers,
    })
}

fn view<H: HostVulkan>(
    objects: &Objects<H>,
    device: u64,
    image: u64,
    range: &VkImageSubresourceRange,
) -> Result<View, String> {
    let object = objects.image(device, image).map_err(|e| e.to_string())?;
    let all = Range {
        aspects: aspects(object.facts.format),
        mip: 0,
        mips: object.facts.mip_levels,
        layer: 0,
        layers: object.facts.array_layers,
    };
    Ok(View {
        device,
        image,
        identity: object.identity.clone(),
        range: normalize(range, all)?,
        all_aspects: all.aspects,
        is_3d: object.facts.image_type == 2,
    })
}

impl ImageState {
    pub(super) fn checkpoint_cell(&self, mip: u32, layer: u32) -> Option<Cell> {
        self.checkpoint_aspect_cell(1, mip, layer)
    }

    pub(super) fn checkpoint_aspect_cell(&self, aspect: u32, mip: u32, layer: u32) -> Option<Cell> {
        self.spans
            .iter()
            .find(|s| {
                s.range.aspects & aspect == aspect
                    && mip >= s.range.mip
                    && mip - s.range.mip < s.range.mips
                    && layer >= s.range.layer
                    && layer - s.range.layer < s.range.layers
            })
            .map(|s| s.cell)
    }

    /// Private upload commands bypass guest command recording. Commit their
    /// completed layout so a later tile never discards an earlier tile.
    pub(super) fn checkpoint_uploaded(
        &mut self,
        image: u64,
        subresource: (u32, u32, u32),
        cell: Cell,
        family: u32,
    ) -> Result<(), String> {
        let (aspects, mip, layer) = subresource;
        let old_layout = self
            .checkpoint_aspect_cell(aspects & aspects.wrapping_neg(), mip, layer)
            .ok_or("missing restored image subresource")?
            .layout;
        self.apply(
            &Op {
                image: View {
                    device: self.device,
                    image,
                    identity: self.identity.clone(),
                    range: Range {
                        aspects,
                        mip,
                        mips: 1,
                        layer,
                        layers: 1,
                    },
                    all_aspects: self.all.aspects,
                    is_3d: false,
                },
                layout: cell.layout,
                old_layout,
                from: IGNORED,
                to: IGNORED,
            },
            family,
        )
    }

    fn apply(&mut self, op: &Op, family: u32) -> Result<(), String> {
        let owner = if self.concurrent {
            Owner::Concurrent
        } else if op.from != op.to && op.from != IGNORED && op.to != IGNORED {
            if family == op.from {
                Owner::Released {
                    from: op.from,
                    to: op.to,
                    old_layout: op.old_layout,
                    new_layout: op.layout,
                }
            } else if family == op.to {
                Owner::Queue(family)
            } else {
                return Err("an ownership transfer ran on neither of its families".into());
            }
        } else {
            Owner::Queue(family)
        };
        let transfer = op.from != op.to && op.from != IGNORED && op.to != IGNORED;
        // Equal-layout memory dependencies do not transition a layout. In
        // synchronization2 their layout values may be placeholders for a
        // range whose subresources actually have different layouts.
        let transition = op.old_layout != op.layout || transfer;
        let cell = Cell {
            layout: op.layout,
            owner,
        };
        let target = op.image.range;
        if target == self.all && transition {
            self.spans.clear();
            self.spans.push(Span {
                range: target,
                cell,
            });
            self.charge
                .shrink_to((std::mem::size_of::<Span>() + 64) as u64);
            return Ok(());
        }
        let mut result = Vec::new();
        result
            .try_reserve(self.spans.len().saturating_mul(6).min(MAX_SPANS + 6))
            .map_err(|_| "image tracking allocation failed")?;
        for span in &self.spans {
            let old = span.range;
            let mask = old.aspects & target.aspects;
            let m0 = old.mip.max(target.mip);
            let m1 = (old.mip + old.mips).min(target.mip + target.mips);
            let l0 = old.layer.max(target.layer);
            let l1 = (old.layer + old.layers).min(target.layer + target.layers);
            if mask == 0 || m0 >= m1 || l0 >= l1 {
                result.push(*span);
                if result.len() > MAX_SPANS {
                    return Err("too many tracked image layout spans".into());
                }
                continue;
            }
            if old.aspects & !mask != 0 {
                result.push(Span {
                    range: Range {
                        aspects: old.aspects & !mask,
                        ..old
                    },
                    cell: span.cell,
                });
            }
            let mut add = |mip, mips, layer, layers, value| {
                if mips != 0 && layers != 0 {
                    result.push(Span {
                        range: Range {
                            aspects: mask,
                            mip,
                            mips,
                            layer,
                            layers,
                        },
                        cell: value,
                    });
                }
            };
            add(old.mip, m0 - old.mip, old.layer, old.layers, span.cell);
            add(
                m1,
                old.mip + old.mips - m1,
                old.layer,
                old.layers,
                span.cell,
            );
            add(m0, m1 - m0, old.layer, l0 - old.layer, span.cell);
            add(m0, m1 - m0, l1, old.layer + old.layers - l1, span.cell);
            add(
                m0,
                m1 - m0,
                l0,
                l1 - l0,
                Cell {
                    layout: if transition {
                        op.layout
                    } else {
                        span.cell.layout
                    },
                    owner,
                },
            );
            if result.len() > MAX_SPANS {
                return Err("too many tracked image layout spans".into());
            }
        }
        let bytes = (result.len() * std::mem::size_of::<Span>() + 64) as u64;
        if bytes > self.charge.amount() {
            self.charge
                .grow(bytes - self.charge.amount())
                .map_err(|e| format!("image layout budget exceeded: {e:?}"))?;
        } else {
            self.charge.shrink_to(bytes);
        }
        self.spans = result;
        Ok(())
    }
}

impl ImageTracking {
    fn disable(&mut self, reason: String) {
        if self.error.is_none() {
            *self = Self {
                error: Some(reason),
                ..Self::default()
            };
        }
    }

    pub fn forget_recording(&mut self, cb: u64) {
        self.recordings.remove(&cb);
    }

    pub fn prune<H: HostVulkan>(&mut self, objects: &Objects<H>) {
        self.recordings
            .retain(|cb, _| objects.raw_any(Kind::CommandBuffer, *cb).is_ok());
        self.images.retain(|id, state| {
            objects
                .image(state.device, *id)
                .is_ok_and(|i| Arc::ptr_eq(&i.identity, &state.identity))
        });
        self.views
            .retain(|id, (v, _)| objects.image_view(v.device, *id).is_ok());
        self.passes
            .retain(|id, p| objects.raw(Kind::RenderPass, p.device, *id).is_ok());
        self.framebuffers
            .retain(|id, f| objects.raw(Kind::Framebuffer, f.device, *id).is_ok());
    }

    fn framebuffer_views(&self, ids: &[VkImageView]) -> Result<Vec<View>, String> {
        let mut views = Vec::new();
        views
            .try_reserve_exact(ids.len())
            .map_err(|_| "framebuffer tracking allocation failed")?;
        for id in ids {
            views.push(
                self.views
                    .get(&id.0)
                    .map(|(v, _)| v.clone())
                    .ok_or("framebuffer image view is not tracked")?,
            );
        }
        Ok(views)
    }

    fn begin<H: HostVulkan>(
        &self,
        cb: u64,
        begin: &VkRenderPassBeginInfo,
        objects: &Objects<H>,
    ) -> Result<Action, String> {
        let pass = self
            .passes
            .get(&begin.render_pass.0)
            .ok_or("render pass is not tracked")?;
        let fb = self
            .framebuffers
            .get(&begin.framebuffer.0)
            .ok_or("framebuffer is not tracked")?;
        let (views, layers) = match &fb.data {
            Framebuffer::Fixed { views, layers } => (views.clone(), *layers),
            Framebuffer::Imageless { layers } => {
                let info = begin
                    .p_next
                    .iter()
                    .find_map(|link| match link {
                        VkRenderPassBeginInfoNext::VkRenderPassAttachmentBeginInfo(info) => {
                            Some(info)
                        }
                        _ => None,
                    })
                    .ok_or("imageless framebuffer has no attachment views")?;
                (
                    self.framebuffer_views(info.p_attachments.as_deref().unwrap_or_default())?,
                    *layers,
                )
            }
        };
        if pass.data.attachments.len() != views.len() {
            return Err("render pass attachment count differs".into());
        }
        let charged = charge(
            objects.limits(),
            views
                .len()
                .saturating_mul(2)
                .saturating_mul(pass.data.views.count_ones().max(1) as usize),
            std::mem::size_of::<Op>(),
        )?;
        let mut finals = Vec::new();
        for (mut base, attachment) in views.into_iter().zip(&pass.data.attachments) {
            // Framebuffer depth/stencil attachments ignore the view's aspect
            // mask. A 2D view of a 3D image transitions the entire mip in 1.3.
            if base.all_aspects & 6 != 0 {
                base.range.aspects = base.all_aspects;
            }
            let mut ranges = Vec::new();
            if base.is_3d {
                ranges.push(base.range);
            } else if pass.data.views == 0 {
                if layers == 0 || layers > base.range.layers {
                    return Err("framebuffer layers exceed its attachment view".into());
                }
                ranges.push(Range {
                    layers,
                    ..base.range
                });
            } else {
                for bit in 0..32 {
                    if pass.data.views & (1 << bit) == 0 {
                        continue;
                    }
                    if bit >= base.range.layers {
                        return Err("multiview layer is outside its attachment view".into());
                    }
                    ranges.push(Range {
                        layer: base.range.layer + bit,
                        layers: 1,
                        ..base.range
                    });
                }
            }
            for range in ranges {
                let image = View {
                    range,
                    ..base.clone()
                };
                if let Some(stencil) = attachment.stencil {
                    let mut depth = image.clone();
                    depth.range.aspects &= !4;
                    if depth.range.aspects != 0 {
                        finals.push(Op {
                            image: depth,
                            layout: attachment.layout,
                            old_layout: 0,
                            from: IGNORED,
                            to: IGNORED,
                        });
                    }
                    let mut image = image;
                    image.range.aspects &= 4;
                    if image.range.aspects != 0 {
                        finals.push(Op {
                            image,
                            layout: stencil,
                            old_layout: 0,
                            from: IGNORED,
                            to: IGNORED,
                        });
                    }
                } else {
                    finals.push(Op {
                        image,
                        layout: attachment.layout,
                        old_layout: 0,
                        from: IGNORED,
                        to: IGNORED,
                    });
                }
            }
        }
        Ok(Action::Begin {
            cb,
            finals: Arc::new(Retained {
                device: pass.device,
                data: finals,
                _charge: charged,
            }),
        })
    }

    pub fn prepare<H: HostVulkan>(
        &self,
        command: &Command<'_>,
        objects: &Objects<H>,
    ) -> Result<Note, String> {
        if self.error.is_some() {
            return Ok(Note(None));
        }
        let action = match command {
            Command::CreateImage(a) => a.p_create_info.as_ref().and_then(|info| {
                a.p_image.map(|id| Action::Image {
                    device: a.device.0,
                    id: id.0,
                    initial: info.initial_layout,
                    concurrent: info.sharing_mode == 1,
                })
            }),
            Command::CreateImageView(a) => {
                let (Some(info), Some(id)) = (&a.p_create_info, a.p_view) else {
                    return Ok(Note(None));
                };
                let mut range = info.subresource_range.clone();
                if objects
                    .image(a.device.0, info.image.0)
                    .map_err(|e| e.to_string())?
                    .facts
                    .image_type
                    == 2
                {
                    range.base_array_layer = 0;
                    range.layer_count = 1;
                }
                Some(Action::View {
                    id: id.0,
                    view: view(objects, a.device.0, info.image.0, &range)?,
                    charge: charge(objects.limits(), 1, std::mem::size_of::<View>())?,
                })
            }
            Command::CreateRenderPass(a) => {
                let (Some(info), Some(id)) = (&a.p_create_info, a.p_render_pass) else {
                    return Ok(Note(None));
                };
                let inputs = info.p_attachments.as_deref().unwrap_or_default();
                let charged = charge(
                    objects.limits(),
                    inputs.len(),
                    std::mem::size_of::<AttachmentFinal>(),
                )?;
                Some(Action::Pass {
                    id: id.0,
                    retained: Arc::new(Retained {
                        device: a.device.0,
                        data: PassFinals {
                            attachments: inputs
                                .iter()
                                .map(|d| AttachmentFinal {
                                    layout: d.final_layout,
                                    stencil: None,
                                })
                                .collect(),
                            views: info
                                .p_next
                                .iter()
                                .find_map(|link| match link {
                                    VkRenderPassCreateInfoNext::VkRenderPassMultiviewCreateInfo(
                                        m,
                                    ) => Some(m),
                                    _ => None,
                                })
                                .map_or(0, |m| {
                                    m.p_view_masks
                                        .iter()
                                        .flatten()
                                        .fold(0, |mask, value| mask | value)
                                }),
                        },
                        _charge: charged,
                    }),
                })
            }
            Command::CreateRenderPass2(a) => {
                let (Some(info), Some(id)) = (&a.p_create_info, a.p_render_pass) else {
                    return Ok(Note(None));
                };
                let inputs = info.p_attachments.as_deref().unwrap_or_default();
                let charged = charge(
                    objects.limits(),
                    inputs.len(),
                    std::mem::size_of::<AttachmentFinal>(),
                )?;
                Some(Action::Pass { id: id.0, retained: Arc::new(Retained { device: a.device.0, data: PassFinals {
                    attachments: inputs.iter().map(|d| AttachmentFinal { layout: d.final_layout, stencil: d.p_next.first().map(|link| match link { VkAttachmentDescription2Next::VkAttachmentDescriptionStencilLayout(s) => s.stencil_final_layout }) }).collect(),
                    views: info.p_subpasses.iter().flatten().fold(0, |mask, s| mask | s.view_mask),
                }, _charge: charged }) })
            }
            Command::CreateFramebuffer(a) => {
                let (Some(info), Some(id)) = (&a.p_create_info, a.p_framebuffer) else {
                    return Ok(Note(None));
                };
                let count = if info.flags & 1 != 0 {
                    0
                } else {
                    info.p_attachments.as_ref().map_or(0, Vec::len)
                };
                let _charge = charge(objects.limits(), count, std::mem::size_of::<View>())?;
                let data = if info.flags & 1 != 0 {
                    Framebuffer::Imageless {
                        layers: info.layers,
                    }
                } else {
                    Framebuffer::Fixed {
                        views: self
                            .framebuffer_views(info.p_attachments.as_deref().unwrap_or_default())?,
                        layers: info.layers,
                    }
                };
                Some(Action::Framebuffer {
                    id: id.0,
                    retained: Arc::new(Retained {
                        device: a.device.0,
                        data,
                        _charge,
                    }),
                })
            }
            Command::CmdBeginRenderPass(a) => match &a.p_render_pass_begin {
                Some(info) => Some(self.begin(a.command_buffer.0, info, objects)?),
                None => None,
            },
            Command::CmdBeginRenderPass2(a) => match &a.p_render_pass_begin {
                Some(info) => Some(self.begin(a.command_buffer.0, info, objects)?),
                None => None,
            },
            Command::CmdEndRenderPass(a) => Some(Action::End {
                cb: a.command_buffer.0,
            }),
            Command::CmdEndRenderPass2(a) => Some(Action::End {
                cb: a.command_buffer.0,
            }),
            Command::CmdExecuteCommands(a) => {
                let count =
                    a.p_command_buffers
                        .iter()
                        .flatten()
                        .try_fold(0usize, |count, cb| {
                            count
                                .checked_add(self.recordings.get(&cb.0).map_or(0, |r| r.ops.len()))
                                .ok_or("inherited image recording size overflow")
                        })?;
                let _charge = charge(objects.limits(), count, std::mem::size_of::<Op>())?;
                let mut ops = Vec::new();
                ops.try_reserve_exact(count)
                    .map_err(|_| "inherited image recording allocation failed")?;
                for cb in a.p_command_buffers.iter().flatten() {
                    if let Some(recording) = self.recordings.get(&cb.0) {
                        ops.extend(recording.ops.iter().cloned());
                    }
                }
                Some(Action::Ops {
                    cb: a.command_buffer.0,
                    ops,
                    _charge,
                })
            }
            Command::DestroyImage(a) => Some(Action::Destroy {
                kind: Kind::Image,
                id: a.image.0,
            }),
            Command::DestroyImageView(a) => Some(Action::Destroy {
                kind: Kind::ImageView,
                id: a.image_view.0,
            }),
            Command::DestroyRenderPass(a) => Some(Action::Destroy {
                kind: Kind::RenderPass,
                id: a.render_pass.0,
            }),
            Command::DestroyFramebuffer(a) => Some(Action::Destroy {
                kind: Kind::Framebuffer,
                id: a.framebuffer.0,
            }),
            Command::DestroyDevice(_) => Some(Action::Prune),
            _ => self.barriers(command, objects)?,
        };
        Ok(Note(action))
    }

    fn barriers<H: HostVulkan>(
        &self,
        command: &Command<'_>,
        objects: &Objects<H>,
    ) -> Result<Option<Action>, String> {
        let (cb, legacy, deps): (u64, &[VkImageMemoryBarrier], Vec<&VkDependencyInfo>) =
            match command {
                Command::CmdPipelineBarrier(a) => (
                    a.command_buffer.0,
                    a.p_image_memory_barriers.as_deref().unwrap_or_default(),
                    Vec::new(),
                ),
                Command::CmdWaitEvents(a) => (
                    a.command_buffer.0,
                    a.p_image_memory_barriers.as_deref().unwrap_or_default(),
                    Vec::new(),
                ),
                Command::CmdPipelineBarrier2(a) => (
                    a.command_buffer.0,
                    &[],
                    a.p_dependency_info.iter().collect(),
                ),
                Command::CmdWaitEvents2(a) => (
                    a.command_buffer.0,
                    &[],
                    a.p_dependency_infos.iter().flatten().collect(),
                ),
                _ => return Ok(None),
            };
        let device = objects
            .raw_any(Kind::CommandBuffer, cb)
            .map_err(|e| e.to_string())?
            .device;
        let count = deps.iter().try_fold(legacy.len(), |n, d| {
            n.checked_add(d.p_image_memory_barriers.as_ref().map_or(0, Vec::len))
                .ok_or("image barrier tracking size overflow")
        })?;
        let _charge = charge(objects.limits(), count, std::mem::size_of::<Op>())?;
        let mut ops = Vec::new();
        ops.try_reserve_exact(count)
            .map_err(|_| "image barrier tracking allocation failed")?;
        for b in legacy {
            ops.push(Op {
                image: view(objects, device, b.image.0, &b.subresource_range)?,
                layout: b.new_layout,
                old_layout: b.old_layout,
                from: b.src_queue_family_index,
                to: b.dst_queue_family_index,
            });
        }
        for b in deps
            .into_iter()
            .flat_map(|d| d.p_image_memory_barriers.iter().flatten())
        {
            ops.push(Op {
                image: view(objects, device, b.image.0, &b.subresource_range)?,
                layout: b.new_layout,
                old_layout: b.old_layout,
                from: b.src_queue_family_index,
                to: b.dst_queue_family_index,
            });
        }
        Ok(Some(Action::Ops { cb, ops, _charge }))
    }

    fn recording(&mut self, cb: u64, limits: &ContextLimits) -> Result<&mut Recording, String> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.recordings.entry(cb) {
            entry.insert(Recording {
                ops: Vec::new(),
                active_pass: None,
                charge: charge(limits, 0, 1)?,
            });
        }
        self.recordings
            .get_mut(&cb)
            .ok_or_else(|| "missing image recording".into())
    }

    fn append(&mut self, cb: u64, ops: Vec<Op>, limits: &ContextLimits) -> Result<(), String> {
        if ops.is_empty() {
            return Ok(());
        }
        let recording = self.recording(cb, limits)?;
        recording
            .charge
            .grow((ops.len() * std::mem::size_of::<Op>()) as u64)
            .map_err(|e| format!("image recording budget exceeded: {e:?}"))?;
        recording
            .ops
            .try_reserve(ops.len())
            .map_err(|_| "image recording allocation failed")?;
        recording.ops.extend(ops);
        Ok(())
    }

    pub fn finish<H: HostVulkan>(&mut self, note: Result<Note, String>, objects: &Objects<H>) {
        let result = (|| {
            let Note(action) = note?;
            match action {
                None => {}
                Some(Action::Image {
                    device,
                    id,
                    initial,
                    concurrent,
                }) => {
                    let image = objects.image(device, id).map_err(|e| e.to_string())?;
                    let all = Range {
                        aspects: aspects(image.facts.format),
                        mip: 0,
                        mips: image.facts.mip_levels,
                        layer: 0,
                        layers: image.facts.array_layers,
                    };
                    self.images.insert(
                        id,
                        ImageState {
                            device,
                            identity: image.identity.clone(),
                            all,
                            concurrent,
                            spans: vec![Span {
                                range: all,
                                cell: Cell {
                                    layout: initial,
                                    owner: if concurrent {
                                        Owner::Concurrent
                                    } else {
                                        Owner::Initial
                                    },
                                },
                            }],
                            charge: charge(objects.limits(), 1, std::mem::size_of::<Span>())?,
                        },
                    );
                }
                Some(Action::View { id, view, charge }) => {
                    self.views.insert(id, (view, charge));
                }
                Some(Action::Pass { id, retained }) => {
                    self.passes.insert(id, retained);
                }
                Some(Action::Framebuffer { id, retained }) => {
                    self.framebuffers.insert(id, retained);
                }
                Some(Action::Begin { cb, finals }) => {
                    let recording = self.recording(cb, objects.limits())?;
                    if recording.active_pass.is_some() {
                        return Err("nested tracked render pass".into());
                    }
                    recording.active_pass = Some(finals);
                }
                Some(Action::End { cb }) => {
                    let finals = self
                        .recording(cb, objects.limits())?
                        .active_pass
                        .take()
                        .ok_or("render pass ended without tracked attachments")?;
                    self.append(cb, finals.data.clone(), objects.limits())?;
                }
                Some(Action::Ops { cb, ops, _charge }) => self.append(cb, ops, objects.limits())?,
                Some(Action::Destroy { kind, id }) => match kind {
                    Kind::Image => {
                        self.images.remove(&id);
                    }
                    Kind::ImageView => {
                        self.views.remove(&id);
                    }
                    Kind::RenderPass => {
                        self.passes.remove(&id);
                    }
                    Kind::Framebuffer => {
                        self.framebuffers.remove(&id);
                    }
                    _ => {}
                },
                Some(Action::Prune) => self.prune(objects),
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.disable(error);
        }
    }

    pub fn submitted<H: HostVulkan>(
        &mut self,
        cb: u64,
        family: u32,
        objects: &Objects<H>,
        epoch: u64,
        ownership: Option<(
            &super::checkpoint_ownership::SharedOwnership,
            &[crate::venus::renderer::SharedRef],
        )>,
    ) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            let Some(recording) = self.recordings.get(&cb) else {
                return Ok(());
            };
            if recording.active_pass.is_some() {
                return Err("submitted command buffer has an unfinished render pass".into());
            }
            for op in &recording.ops {
                let Some(state) = self.images.get_mut(&op.image.image) else {
                    continue;
                };
                if state.device != op.image.device
                    || !Arc::ptr_eq(&state.identity, &op.image.identity)
                {
                    continue;
                }
                if objects.image(state.device, op.image.image).is_err() {
                    continue;
                }
                state.apply(op, family)?;
                if let (Some((ownership, claimed)), Ok(image)) =
                    (ownership, objects.image(state.device, op.image.image))
                {
                    ownership.observe(
                        epoch,
                        Some(claimed),
                        image,
                        state.checkpoint_cell(0, 0),
                        objects,
                    );
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.disable(error);
        }
    }
}

#[cfg(test)]
impl<H: HostVulkan> super::VulkanContext<H> {
    pub(crate) fn checkpoint_image_layouts(&self, image: u64) -> Result<Vec<Span>, String> {
        if let Some(error) = &self.image_tracking.error {
            return Err(error.clone());
        }
        self.image_tracking
            .images
            .get(&image)
            .map(|state| state.spans.clone())
            .ok_or_else(|| "image layout is not tracked".into())
    }
}
