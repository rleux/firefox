/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::internal_types::FastHashMap;
use super::resources::Owned;
use super::{hal, wgt, Buffer, Device, Recording};
use api::units::{DeviceIntPoint, DeviceIntRect};
use std::{convert::TryInto, rc::Rc};

#[derive(Default)]
pub(super) struct ClearCache {
    pipelines: FastHashMap<(*const Device, wgt::TextureFormat, bool, bool, bool), Rc<ClearPipeline>>,
    bindings: FastHashMap<(*const ClearPipeline, *const Buffer), Rc<ClearBindings>>,
}

struct ClearPipeline {
    raw: Owned<dyn hal::DynRenderPipeline>,
    layout: Owned<dyn hal::DynPipelineLayout>,
    bindings: Owned<dyn hal::DynBindGroupLayout>,
}

struct ClearBindings {
    raw: Owned<dyn hal::DynBindGroup>,
    uniform: Rc<Buffer>,
    pipeline: Rc<ClearPipeline>,
}

pub(super) struct Clear {
    bindings: Rc<ClearBindings>,
    offset: u32,
    pub rect: DeviceIntRect,
}

impl Recording<'_> {
    pub(super) fn clear(
        &mut self,
        owner: &Rc<Device>,
        format: wgt::TextureFormat,
        has_depth: bool,
        rect: DeviceIntRect,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
    ) -> Result<Rc<Clear>, String> {
        self.with_binding_cache(|commands, cache| {
            let mut values = [0f32; 16];
            values[..4].copy_from_slice(&color.unwrap_or([0.; 4]));
            values[4] = depth.unwrap_or(0.);
            let (uniform, offset) = cache.uniform(commands, owner, values.map(f32::to_bits),
                |commands, capacity| commands.upload_uniform(capacity))?;
            let clear = cache.clears.prepare(owner, format, has_depth, rect,
                color.is_some(), depth.is_some(), uniform, offset)?;
            clear.prepare(commands)?;
            Ok(clear)
        })
    }
}

impl ClearCache {
    pub fn clear(&mut self) {
        self.bindings.clear();
    }

    fn prepare(
        &mut self,
        owner: &Rc<Device>,
        format: wgt::TextureFormat,
        has_depth: bool,
        rect: DeviceIntRect,
        color: bool,
        depth: bool,
        uniform: Rc<Buffer>,
        offset: u32,
    ) -> Result<Rc<Clear>, String> {
        let key = (Rc::as_ptr(owner), format, has_depth, color, depth);
        let pipeline = match self.pipelines.get(&key) {
            Some(pipeline) => pipeline.clone(),
            None => {
                let pipeline = Rc::new(ClearPipeline::new(owner, (format, has_depth, color, depth))?);
                self.pipelines.insert(key, pipeline.clone());
                pipeline
            }
        };
        let key = (Rc::as_ptr(&pipeline), Rc::as_ptr(&uniform));
        let bindings = match self.bindings.get(&key) {
            Some(bindings) => bindings.clone(),
            None => {
                let raw = unsafe {
                    owner.open.device.create_bind_group(&hal::BindGroupDescriptor {
                        label: Some("WR clear parameters"),
                        layout: &*pipeline.bindings,
                        buffers: &[hal::BufferBinding::new_unchecked(&*uniform.raw, 0, std::num::NonZeroU64::new(32).unwrap())],
                        textures: &[],
                        samplers: &[],
                        entries: &[hal::BindGroupEntry { binding: 0, resource_index: 0, count: 1 }],
                        acceleration_structures: &[],
                        external_textures: &[],
                    })
                }.map_err(|error| format!("Creating clear bindings: {error:?}"))?;
                #[cfg(test)]
                owner.trace.borrow_mut().push(super::tests::Command::ClearBindGroup);
                let bindings = Rc::new(ClearBindings {
                    raw: Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_bind_group),
                    uniform,
                    pipeline,
                });
                self.bindings.insert(key, bindings.clone());
                bindings
            }
        };
        Ok(Rc::new(Clear { bindings, offset, rect }))
    }
}

impl ClearPipeline {
    fn new(owner: &Rc<Device>, (format, has_depth, color, depth): (wgt::TextureFormat, bool, bool, bool)) -> Result<Self, String> {
        let device = owner.open.device.as_ref();
        let module = |bytes: &[u8]| -> Result<Owned<dyn hal::DynShaderModule>, String> {
            let words: Vec<_> = bytes.chunks_exact(4)
                .map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect();
            let raw = owner.create_shader_module("WR clear", &words)?;
            Ok(Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_shader_module))
        };
        let vertex = module(include_bytes!("clear.vert.spv"))?;
        let fragment = module(include_bytes!("clear.frag.spv"))?;
        unsafe {
            let bindings = device.create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                label: Some("WR clear parameters"),
                flags: hal::BindGroupLayoutFlags::empty(),
                entries: &[wgt::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgt::ShaderStages::VERTEX | wgt::ShaderStages::FRAGMENT,
                    ty: wgt::BindingType::Buffer {
                        ty: wgt::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: std::num::NonZeroU64::new(32),
                    },
                    count: None,
                }],
            }).map_err(|error| format!("Creating clear layout: {error:?}"))?;
            let bindings = Owned::new(owner, bindings, <dyn hal::DynDevice>::destroy_bind_group_layout);
            let layout = device.create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                label: Some("WR clear"),
                flags: hal::PipelineLayoutFlags::empty(),
                bind_group_layouts: &[Some(&*bindings)],
                immediate_size: 0,
            }).map_err(|error| format!("Creating clear pipeline layout: {error:?}"))?;
            let layout = Owned::new(owner, layout, <dyn hal::DynDevice>::destroy_pipeline_layout);
            let constants = Default::default();
            let stage = |module| hal::ProgrammableStage {
                module, entry_point: "main", constants: &constants,
                zero_initialize_workgroup_memory: false,
            };
            let pipeline = device.create_render_pipeline(&hal::RenderPipelineDescriptor {
                label: Some("WR clear"),
                layout: &*layout,
                vertex_processor: hal::VertexProcessor::Standard {
                    vertex_buffers: &[],
                    vertex_stage: stage(&*vertex),
                },
                fragment_stage: Some(stage(&*fragment)),
                primitive: Default::default(),
                depth_stencil: has_depth.then(|| wgt::DepthStencilState {
                    format: wgt::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(depth),
                    depth_compare: Some(wgt::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                color_targets: &[Some(wgt::ColorTargetState {
                    format, blend: None,
                    write_mask: if color { wgt::ColorWrites::ALL } else { wgt::ColorWrites::empty() },
                })],
                multiview_mask: None,
                cache: None,
            }).map_err(|error| format!("Creating clear pipeline: {error:?}"))?;
            Ok(Self {
                raw: Owned::new(owner, pipeline, <dyn hal::DynDevice>::destroy_render_pipeline),
                layout,
                bindings,
            })
        }
    }
}

impl Clear {
    pub fn prepare(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.bindings.uniform.transition(commands, wgt::BufferUses::UNIFORM)?;
        commands.keep(&self.bindings);
        Ok(())
    }

    pub unsafe fn record(&self, encoder: &mut dyn hal::DynCommandEncoder, size: wgt::Extent3d, origin: DeviceIntPoint) {
        unsafe {
            encoder.set_viewport(&hal::Rect { x: 0., y: 0., w: size.width as f32, h: size.height as f32 }, 0.0..1.0);
            encoder.set_scissor_rect(&hal::Rect {
                x: (self.rect.min.x - origin.x) as u32,
                y: (self.rect.min.y - origin.y) as u32,
                w: self.rect.width() as u32,
                h: self.rect.height() as u32,
            });
            encoder.set_render_pipeline(&*self.bindings.pipeline.raw);
            encoder.set_bind_group(&*self.bindings.pipeline.layout, 0, &*self.bindings.raw, &[self.offset]);
            encoder.draw(0, 3, 0, 1);
        }
    }
}
