/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::{Buffer, Owned, Texture, texture_format};
use std::{collections::HashMap, mem, rc::Rc};
use api::{ColorF, ImageBufferKind, PremultipliedColorF, units::*};
use crate::batch::{AlphaBatchContainer, BatchKind};
use crate::composite::{CompositeTileSurface, ResolvedExternalSurfaceColorData};
use crate::device::{BlendMode, TextureFilter, VertexAttributeKind, VertexDescriptor};
use crate::frame_builder::Frame;
use crate::gpu_types::{ClearInstance, CompositeInstance, PrimitiveInstanceData};
use crate::internal_types::{
    CacheTextureId, ResourceUpdateList, Swizzle, TextureCacheAllocationKind, TextureSource,
    TextureUpdateSource,
};
use crate::pattern::PatternKind;
use crate::picture::ResolvedSurfaceTexture;
use crate::render_target::{PictureCacheTargetKind, RenderTarget};
use crate::renderer::{vertex_descriptors as desc, MAX_VERTEX_TEXTURE_WIDTH};
use webrender_build::hal::{ScalarType, ShaderArtifact};

mod shaders {
    include!(concat!(env!("OUT_DIR"), "/hal_shaders.rs"));
}

// Only audited, fully initialized numeric GPU layouts may expose their bytes.
unsafe trait GpuData {
    const SIZE: usize;
}
unsafe impl GpuData for PrimitiveInstanceData {
    const SIZE: usize = 16;
}
unsafe impl GpuData for CompositeInstance {
    const SIZE: usize = 152;
}
unsafe impl GpuData for ClearInstance {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::transform::TransformData {
    const SIZE: usize = 128;
}
unsafe impl GpuData for crate::render_task::RenderTaskData {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::renderer::GpuBufferBlockF {
    const SIZE: usize = 16;
}
unsafe impl GpuData for crate::renderer::GpuBufferBlockI {
    const SIZE: usize = 16;
}

fn bytes<T: GpuData>(values: &[T]) -> &[u8] {
    assert_eq!(mem::size_of::<T>(), T::SIZE);
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), mem::size_of_val(values)) }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Shader {
    Quad,
    Composite,
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PipelineKey {
    shader: Shader,
    blend: u8,
    depth: u8,
    format: wgt::TextureFormat,
}

#[derive(Default, Debug)]
pub struct DrawStats {
    pub draw_calls: usize,
    pub primitive_instances: usize,
    pub composite_tiles: usize,
    pub color_targets: usize,
}

pub struct FrameOutput {
    pub size: [u32; 2],
    pub pixels: Vec<u8>,
    pub stats: DrawStats,
}

struct Draw<A: hal::Api> {
    shader: Shader,
    blend: u8,
    depth: u8,
    count: u32,
    instances: Vec<u8>,
    source: Rc<Texture<A>>,
    scissor: DeviceIntRect,
}

pub(crate) struct FrameRenderer<A: hal::Api> {
    owner: Rc<Device<A>>,
    textures: HashMap<CacheTextureId, Rc<Texture<A>>>,
    pipelines: HashMap<PipelineKey, Owned<A, A::RenderPipeline>>,
    layout: Owned<A, A::PipelineLayout>,
    bindings: Owned<A, A::BindGroupLayout>,
    samplers: [Owned<A, A::Sampler>; 2],
    quad: Buffer<A>,
    dummy: Rc<Texture<A>>,
    failed: bool,
}

impl<A: hal::Api> FrameRenderer<A> {
    pub fn new(device: Device<A>) -> Result<Self> {
        let owner = Rc::new(device);
        let native = &owner.open.device;
        let mut entries = vec![wgt::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgt::ShaderStages::VERTEX,
            ty: wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: std::num::NonZeroU64::new(64),
            },
            count: None,
        }];
        for binding in shaders::TEXTURE_BINDINGS {
            let filtering = binding.name.starts_with("sColor");
            let sample_type = match binding.scalar {
                ScalarType::Float => wgt::TextureSampleType::Float {
                    filterable: filtering,
                },
                ScalarType::Sint => wgt::TextureSampleType::Sint,
                ScalarType::Uint => wgt::TextureSampleType::Uint,
            };
            entries.push(wgt::BindGroupLayoutEntry {
                binding: binding.binding,
                visibility: wgt::ShaderStages::VERTEX_FRAGMENT,
                ty: wgt::BindingType::Texture {
                    sample_type,
                    view_dimension: wgt::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
            entries.push(wgt::BindGroupLayoutEntry {
                binding: binding.binding + 1,
                visibility: wgt::ShaderStages::VERTEX_FRAGMENT,
                ty: wgt::BindingType::Sampler(if filtering {
                    wgt::SamplerBindingType::Filtering
                } else {
                    wgt::SamplerBindingType::NonFiltering
                }),
                count: None,
            });
        }
        let bindings = Owned::new(
            &owner,
            unsafe {
                native.create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                    label: Some("WR bindings"),
                    flags: hal::BindGroupLayoutFlags::empty(),
                    entries: &entries,
                })
            }
            .map_err(|e| format!("Creating binding layout: {e:?}"))?,
            A::Device::destroy_bind_group_layout,
        );
        let layout = Owned::new(
            &owner,
            unsafe {
                native.create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                    label: Some("WR pipelines"),
                    flags: hal::PipelineLayoutFlags::empty(),
                    bind_group_layouts: &[Some(&*bindings)],
                    immediate_size: 0,
                })
            }
            .map_err(|e| format!("Creating pipeline layout: {e:?}"))?,
            A::Device::destroy_pipeline_layout,
        );
        let sampler = |filter| -> Result<_> {
            Ok(Owned::new(
                &owner,
                unsafe {
                    native.create_sampler(&hal::SamplerDescriptor {
                        label: Some("WR sampler"),
                        address_modes: [wgt::AddressMode::ClampToEdge; 3],
                        mag_filter: filter,
                        min_filter: filter,
                        mipmap_filter: wgt::MipmapFilterMode::Nearest,
                        lod_clamp: 0.0..0.0,
                        compare: None,
                        anisotropy_clamp: 1,
                        border_color: None,
                    })
                }
                .map_err(|e| format!("Creating sampler: {e:?}"))?,
                A::Device::destroy_sampler,
            ))
        };
        let samplers = [
            sampler(wgt::FilterMode::Nearest)?,
            sampler(wgt::FilterMode::Linear)?,
        ];
        let quad = Buffer::new(
            &owner,
            &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
            wgt::BufferUses::VERTEX,
        )?;
        let dummy = Texture::new(
            &owner,
            1,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )?;
        dummy.upload(
            &owner,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[255; 4],
            None,
            0,
            None,
        )?;
        Ok(Self {
            owner,
            textures: HashMap::new(),
            pipelines: HashMap::new(),
            layout,
            bindings,
            samplers,
            quad,
            dummy,
            failed: false,
        })
    }

    pub fn info(&self) -> &wgt::AdapterInfo {
        self.owner.info()
    }

    fn source(&self, source: TextureSource) -> Result<Rc<Texture<A>>> {
        match source {
            TextureSource::Invalid | TextureSource::Dummy => Ok(self.dummy.clone()),
            TextureSource::TextureCache(id, Swizzle::Rgba) => self
                .textures
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("Missing HAL texture {id:?}")),
            _ => Err(format!("Unsupported HAL texture source {source:?}")),
        }
    }

    fn surface(&self, surface: &ResolvedSurfaceTexture) -> Result<Rc<Texture<A>>> {
        match *surface {
            ResolvedSurfaceTexture::TextureCache { texture } => self.source(texture),
            _ => Err("HAL native render targets are not implemented".into()),
        }
    }

    fn update(&mut self, updates: ResourceUpdateList) -> Result<()> {
        if !updates.native_surface_updates.is_empty() {
            return Err("HAL native surface updates are not implemented".into());
        }
        let updates = updates.texture_updates;
        for ((src, dst), copies) in updates.copies {
            let source = self.textures.get(&src).ok_or("Missing HAL copy source")?;
            let destination = self
                .textures
                .get(&dst)
                .ok_or("Missing HAL copy destination")?;
            for copy in copies {
                self.copy(source, destination, copy.src_rect, copy.dst_rect)?;
            }
        }
        for allocation in updates.allocations {
            match allocation.kind {
                TextureCacheAllocationKind::Alloc(info)
                | TextureCacheAllocationKind::Reset(info) => {
                    if info.target != ImageBufferKind::Texture2D {
                        return Err("Unsupported HAL texture target".into());
                    }
                    let texture = Texture::new(
                        &self.owner,
                        info.width as u32,
                        info.height as u32,
                        texture_format(info.format)?,
                        info.filter,
                        true,
                    )?;
                    self.textures.insert(allocation.id, texture);
                }
                TextureCacheAllocationKind::Free => {
                    self.textures.remove(&allocation.id);
                }
            }
        }
        for (id, updates) in updates.updates {
            let texture = self
                .textures
                .get(&id)
                .ok_or("Updating unknown HAL texture")?;
            for update in updates {
                match update.source {
                    TextureUpdateSource::Bytes { data } => texture.upload(
                        &self.owner,
                        update.rect,
                        &data,
                        update.stride,
                        update.offset,
                        update.format_override,
                    )?,
                    _ => return Err("Unsupported HAL external/debug texture update".into()),
                }
            }
        }
        Ok(())
    }

    fn copy(
        &self,
        src: &Texture<A>,
        dst: &Texture<A>,
        src_rect: DeviceIntRect,
        dst_rect: DeviceIntRect,
    ) -> Result<()> {
        if src.format != dst.format || src_rect.size() != dst_rect.size() || std::ptr::eq(src, dst)
        {
            return Err("Unsupported HAL texture copy".into());
        }
        for (texture, rect) in [(src, src_rect), (dst, dst_rect)] {
            if rect.min.x < 0
                || rect.min.y < 0
                || rect.max.x as u32 > texture.size.width
                || rect.max.y as u32 > texture.size.height
                || rect.is_empty()
            {
                return Err("Invalid HAL texture copy bounds".into());
            }
        }
        let base = |rect: DeviceIntRect| hal::TextureCopyBase {
            mip_level: 0,
            array_layer: 0,
            origin: wgt::Origin3d {
                x: rect.min.x as u32,
                y: rect.min.y as u32,
                z: 0,
            },
            aspect: hal::FormatAspects::COLOR,
        };
        let mut commands = Commands::<A>::new(&self.owner.open)?;
        src.transition(commands.encoder(), wgt::TextureUses::COPY_SRC);
        dst.transition(commands.encoder(), wgt::TextureUses::COPY_DST);
        unsafe {
            commands.encoder().copy_texture_to_texture(
                &src.raw,
                wgt::TextureUses::COPY_SRC,
                &dst.raw,
                std::iter::once(hal::TextureCopy {
                    src_base: base(src_rect),
                    dst_base: base(dst_rect),
                    size: wgt::Extent3d {
                        width: src_rect.width() as u32,
                        height: src_rect.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
        }
        src.transition(commands.encoder(), wgt::TextureUses::RESOURCE);
        dst.transition(commands.encoder(), wgt::TextureUses::RESOURCE);
        commands.submit_and_wait()
    }

    fn data_texture<T: GpuData>(
        &self,
        values: &[T],
        format: wgt::TextureFormat,
    ) -> Result<Rc<Texture<A>>> {
        let source = bytes(values);
        let width = MAX_VERTEX_TEXTURE_WIDTH;
        let height = source.len().div_ceil(width * 16).max(1);
        let height_u32 = u32::try_from(height).map_err(|_| "HAL data texture height overflow")?;
        self.owner.layout(width as u32, height_u32)?;
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(16))
            .ok_or("HAL data texture size overflow")?;
        if size > isize::MAX as usize
            || size as u64 > self.owner.capabilities.limits.max_buffer_size
        {
            return Err("HAL data texture exceeds buffer limits".into());
        }
        let mut data = vec![0; size];
        data[..source.len()].copy_from_slice(source);
        let texture = Texture::new(
            &self.owner,
            width as u32,
            height as u32,
            format,
            TextureFilter::Nearest,
            false,
        )?;
        texture.upload(
            &self.owner,
            DeviceIntRect::from_size(DeviceIntSize::new(width as i32, height as i32)),
            &data,
            None,
            0,
            None,
        )?;
        Ok(texture)
    }

    fn artifact(shader: Shader) -> &'static ShaderArtifact {
        let (name, features) = match shader {
            Shader::Quad => ("ps_quad_textured", "TEXTURE_2D"),
            Shader::Composite => ("composite", "TEXTURE_2D"),
            Shader::Clear => ("ps_clear", ""),
        };
        shaders::SHADERS
            .iter()
            .find(|entry| entry.name == name && entry.features == features)
            .unwrap()
    }

    fn pipeline(&mut self, key: PipelineKey) -> Result<()> {
        if self.pipelines.contains_key(&key) {
            return Ok(());
        }
        let artifact = Self::artifact(key.shader);
        let descriptor = match key.shader {
            Shader::Quad => &desc::PRIM_INSTANCES,
            Shader::Composite => &desc::COMPOSITE,
            Shader::Clear => &desc::CLEAR,
        };
        let (vertex, instances, stride) = vertex_layouts(descriptor, artifact)?;
        let vertex_buffers = [
            Some(hal::VertexBufferLayout {
                array_stride: 4,
                step_mode: wgt::VertexStepMode::Vertex,
                attributes: &vertex,
            }),
            Some(hal::VertexBufferLayout {
                array_stride: stride,
                step_mode: wgt::VertexStepMode::Instance,
                attributes: &instances,
            }),
        ];
        let native = &self.owner.open.device;
        let module = |data: &[u8]| -> Result<_> {
            let words: Vec<_> = data
                .chunks_exact(4)
                .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
                .collect();
            let raw = unsafe {
                native.create_shader_module(
                    &hal::ShaderModuleDescriptor {
                        label: Some(artifact.name),
                        runtime_checks: wgt::ShaderRuntimeChecks::default(),
                    },
                    hal::ShaderInput::SpirV(&words),
                )
            }
            .map_err(|e| format!("Creating shader {}: {e:?}", artifact.name))?;
            Ok(Owned::new(
                &self.owner,
                raw,
                A::Device::destroy_shader_module,
            ))
        };
        let vs = module(artifact.vertex)?;
        let fs = module(artifact.fragment)?;
        let constants = Default::default();
        let stage = |module| hal::ProgrammableStage {
            module,
            entry_point: "main",
            constants: &constants,
            zero_initialize_workgroup_memory: false,
        };
        let blend = match key.blend {
            0 => None,
            1 => Some(wgt::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            2 => Some(wgt::BlendState::ALPHA_BLENDING),
            _ => unreachable!(),
        };
        let pipeline = unsafe {
            native.create_render_pipeline(&hal::RenderPipelineDescriptor {
                label: Some(artifact.name),
                layout: &self.layout,
                vertex_processor: hal::VertexProcessor::Standard {
                    vertex_buffers: &vertex_buffers,
                    vertex_stage: stage(&*vs),
                },
                fragment_stage: Some(stage(&*fs)),
                primitive: wgt::PrimitiveState {
                    topology: wgt::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: if key.depth == 0 {
                    None
                } else {
                    Some(wgt::DepthStencilState {
                        format: wgt::TextureFormat::Depth32Float,
                        depth_write_enabled: Some(key.depth == 1),
                        depth_compare: Some(if key.depth == 3 {
                            wgt::CompareFunction::Always
                        } else {
                            wgt::CompareFunction::LessEqual
                        }),
                        stencil: Default::default(),
                        bias: Default::default(),
                    })
                },
                multisample: Default::default(),
                color_targets: &[Some(wgt::ColorTargetState {
                    format: key.format,
                    blend,
                    write_mask: wgt::ColorWrites::ALL,
                })],
                multiview_mask: None,
                cache: None,
            })
        }
        .map_err(|e| format!("Creating pipeline {key:?}: {e:?}"))?;
        println!("HAL pipeline {:?} shader={:016x}", key, artifact.digest);
        self.pipelines.insert(
            key,
            Owned::new(&self.owner, pipeline, A::Device::destroy_render_pipeline),
        );
        Ok(())
    }

    fn draw_pass(
        &mut self,
        target: &Rc<Texture<A>>,
        draws: &[Draw<A>],
        data: &HashMap<&str, Rc<Texture<A>>>,
        stats: &mut DrawStats,
    ) -> Result<()> {
        let size = target.size;
        let has_depth = draws.iter().any(|draw| draw.depth != 0);
        let depth = if has_depth {
            Some(Texture::new(
                &self.owner,
                size.width,
                size.height,
                wgt::TextureFormat::Depth32Float,
                TextureFilter::Nearest,
                true,
            )?)
        } else {
            None
        };
        let matrix: [f32; 16] = [
            2.0 / size.width as f32,
            0.0,
            0.0,
            0.0,
            0.0,
            -2.0 / size.height as f32,
            0.0,
            0.0,
            0.0,
            0.0,
            -1.0 / crate::renderer::hal::MAX_DEPTH_IDS as f32,
            0.0,
            -1.0,
            1.0,
            1.0,
            1.0,
        ];
        let matrix_bytes: Vec<_> = matrix.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let uniform = Buffer::new(&self.owner, &matrix_bytes, wgt::BufferUses::UNIFORM)?;
        let mut resources = Vec::new();
        for draw in draws {
            let depth_mode = if has_depth && draw.depth == 0 {
                3
            } else {
                draw.depth
            };
            let key = PipelineKey {
                shader: draw.shader,
                blend: draw.blend,
                depth: depth_mode,
                format: target.format,
            };
            self.pipeline(key)?;
            let buffer = Buffer::new(&self.owner, &draw.instances, wgt::BufferUses::VERTEX)?;
            let mut entries = vec![hal::BindGroupEntry {
                binding: 0,
                resource_index: 0,
                count: 1,
            }];
            let mut textures = Vec::new();
            let mut samplers = Vec::new();
            for binding in shaders::TEXTURE_BINDINGS {
                let texture = if binding.name == "sColor0" {
                    &draw.source
                } else if binding.name.starts_with("sColor") {
                    &self.dummy
                } else {
                    data.get(binding.name)
                        .ok_or_else(|| format!("Missing HAL binding {}", binding.name))?
                };
                if Rc::ptr_eq(texture, target) {
                    return Err("HAL attachment feedback is unsupported".into());
                }
                entries.push(hal::BindGroupEntry {
                    binding: binding.binding,
                    resource_index: textures.len() as u32,
                    count: 1,
                });
                textures.push(hal::TextureBinding {
                    view: &*texture.view,
                    usage: wgt::TextureUses::RESOURCE,
                });
                entries.push(hal::BindGroupEntry {
                    binding: binding.binding + 1,
                    resource_index: samplers.len() as u32,
                    count: 1,
                });
                samplers
                    .push(&*self.samplers[usize::from(texture.filter == TextureFilter::Linear)]);
            }
            let group = unsafe {
                self.owner
                    .open
                    .device
                    .create_bind_group(&hal::BindGroupDescriptor {
                        label: Some("WR draw"),
                        layout: &self.bindings,
                        buffers: &[uniform.binding()],
                        samplers: &samplers,
                        textures: &textures,
                        entries: &entries,
                        acceleration_structures: &[],
                        external_textures: &[],
                    })
            }
            .map_err(|e| format!("Creating draw bindings: {e:?}"))?;
            resources.push((
                key,
                buffer,
                Owned::new(&self.owner, group, A::Device::destroy_bind_group),
            ));
        }
        let initialized = target.state.get() != wgt::TextureUses::UNINITIALIZED;
        let mut commands = Commands::<A>::new(&self.owner.open)?;
        self.quad
            .transition(commands.encoder(), wgt::BufferUses::VERTEX);
        uniform.transition(commands.encoder(), wgt::BufferUses::UNIFORM);
        for (_, buffer, _) in &resources {
            buffer.transition(commands.encoder(), wgt::BufferUses::VERTEX);
        }
        target.transition(commands.encoder(), wgt::TextureUses::COLOR_TARGET);
        if let Some(depth) = &depth {
            depth.transition(commands.encoder(), wgt::TextureUses::DEPTH_STENCIL_WRITE);
        }
        unsafe {
            commands
                .encoder()
                .begin_render_pass(&hal::RenderPassDescriptor {
                    label: Some("WR target"),
                    extent: size,
                    sample_count: 1,
                    color_attachments: &[Some(hal::ColorAttachment {
                        target: hal::Attachment {
                            view: target
                                .target
                                .as_ref()
                                .ok_or("HAL texture is not renderable")?,
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        clear_value: wgt::Color::TRANSPARENT,
                        ops: (if initialized {
                            hal::AttachmentOps::LOAD
                        } else {
                            hal::AttachmentOps::LOAD_CLEAR
                        }) | hal::AttachmentOps::STORE,
                    })],
                    depth_stencil_attachment: depth.as_ref().map(|texture| {
                        hal::DepthStencilAttachment {
                            target: hal::Attachment {
                                view: &*texture.view,
                                usage: wgt::TextureUses::DEPTH_STENCIL_WRITE,
                            },
                            depth_ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                            stencil_ops: hal::AttachmentOps::LOAD_DONT_CARE
                                | hal::AttachmentOps::STORE_DISCARD,
                            clear_value: (1.0, 0),
                        }
                    }),
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .map_err(|e| format!("Beginning target: {e:?}"))?;
            commands.encoder().set_viewport(
                &hal::Rect {
                    x: 0.0,
                    y: 0.0,
                    w: size.width as f32,
                    h: size.height as f32,
                },
                0.0..1.0,
            );
            commands.encoder().set_vertex_buffer(0, self.quad.binding());
            for (draw, (key, buffer, group)) in draws.iter().zip(&resources) {
                let full = DeviceIntRect::from_size(DeviceIntSize::new(
                    size.width as i32,
                    size.height as i32,
                ));
                let Some(rect) = draw.scissor.intersection(&full) else {
                    continue;
                };
                commands.encoder().set_scissor_rect(&hal::Rect {
                    x: rect.min.x as u32,
                    y: rect.min.y as u32,
                    w: rect.width() as u32,
                    h: rect.height() as u32,
                });
                commands.encoder().set_render_pipeline(&self.pipelines[key]);
                commands
                    .encoder()
                    .set_bind_group(&self.layout, 0, group, &[]);
                commands.encoder().set_vertex_buffer(1, buffer.binding());
                commands.encoder().draw(0, 4, 0, draw.count);
                stats.draw_calls += 1;
                match draw.shader {
                    Shader::Quad => stats.primitive_instances += draw.count as usize,
                    Shader::Composite => stats.composite_tiles += draw.count as usize,
                    Shader::Clear => {}
                }
            }
            commands.encoder().end_render_pass();
        }
        target.transition(commands.encoder(), wgt::TextureUses::RESOURCE);
        commands.submit_and_wait()?;
        stats.color_targets += 1;
        Ok(())
    }

    fn clear(&self, rect: DeviceIntRect, color: ColorF) -> Draw<A> {
        let instance = ClearInstance {
            rect: [
                rect.min.x as f32,
                rect.min.y as f32,
                rect.max.x as f32,
                rect.max.y as f32,
            ],
            color: [color.r, color.g, color.b, color.a],
        };
        Draw {
            shader: Shader::Clear,
            blend: 0,
            depth: 0,
            count: 1,
            instances: bytes(&[instance]).to_vec(),
            source: self.dummy.clone(),
            scissor: rect,
        }
    }

    fn batches(
        &self,
        container: &AlphaBatchContainer,
        rect: DeviceIntRect,
        draws: &mut Vec<Draw<A>>,
    ) -> Result<()> {
        let has_depth = !container.opaque_batches.is_empty();
        let Some(scissor) = container
            .task_scissor_rect
            .unwrap_or(rect)
            .intersection(&rect)
        else {
            return Ok(());
        };
        for (opaque, batches) in [
            (true, &container.opaque_batches),
            (false, &container.alpha_batches),
        ] {
            for batch in batches {
                if batch.instances.is_empty() {
                    continue;
                }
                if batch.key.kind != BatchKind::Quad(PatternKind::ColorOrTexture)
                    || batch.readback.is_some()
                    || !matches!(
                        batch.key.textures.clip_mask,
                        TextureSource::Invalid | TextureSource::Dummy
                    )
                {
                    return Err(format!("Unsupported HAL batch {:?}", batch.key));
                }
                let blend = match batch.key.blend_mode {
                    BlendMode::None => 0,
                    BlendMode::PremultipliedAlpha => 1,
                    BlendMode::Alpha => 2,
                    mode => return Err(format!("Unsupported HAL blend {mode:?}")),
                };
                draws.push(Draw {
                    shader: Shader::Quad,
                    blend,
                    depth: if !has_depth {
                        0
                    } else if opaque {
                        1
                    } else {
                        2
                    },
                    count: batch.instances.len() as u32,
                    instances: bytes(&batch.instances).to_vec(),
                    source: self.source(batch.key.textures.input.colors[0])?,
                    scissor,
                });
            }
        }
        Ok(())
    }

    fn target(
        &mut self,
        target: &RenderTarget,
        data: &HashMap<&str, Rc<Texture<A>>>,
        stats: &mut DrawStats,
    ) -> Result<()> {
        if !target.clip_masks.is_empty()
            || !target.vertical_blurs.is_empty()
            || !target.horizontal_blurs.is_empty()
            || !target.scalings.is_empty()
            || !target.svg_nodes.is_empty()
            || !target.blits.is_empty()
            || !target.resolve_ops.is_empty()
            || target.prim_instances.iter().any(|batch| !batch.is_empty())
            || !target.prim_instances_with_scissor.is_empty()
            || !target.border_segments_complex.is_empty()
            || !target.border_segments_solid.is_empty()
            || !target.border_segments_complex_superellipse.is_empty()
            || !target.border_segments_solid_superellipse.is_empty()
            || !target.line_decorations.is_empty()
        {
            return Err("Unsupported HAL render-target operations".into());
        }
        let texture = self
            .textures
            .get(&target.texture_id)
            .cloned()
            .ok_or("Missing HAL render target")?;
        let rect = target
            .used_rect
            .unwrap_or(DeviceIntRect::from_size(DeviceIntSize::new(
                texture.size.width as i32,
                texture.size.height as i32,
            )));
        let mut draws = Vec::new();
        if let Some(color) = target.clear_color {
            draws.push(self.clear(rect, color));
        }
        for &(rect, color) in &target.clears {
            draws.push(self.clear(rect, color));
        }
        for container in &target.alpha_batch_containers {
            self.batches(container, rect, &mut draws)?;
        }
        self.draw_pass(&texture, &draws, data, stats)
    }

    pub fn render(
        &mut self,
        frame: &Frame,
        updates: Vec<ResourceUpdateList>,
        clear: ColorF,
    ) -> Result<FrameOutput> {
        if self.failed {
            return Err("HAL renderer must be recreated after an execution failure".into());
        }
        self.failed = true;
        let result = self.render_inner(frame, updates, clear);
        if result.is_ok() {
            self.failed = false;
        }
        result
    }

    fn render_inner(
        &mut self,
        frame: &Frame,
        updates: Vec<ResourceUpdateList>,
        clear: ColorF,
    ) -> Result<FrameOutput> {
        if !frame.deferred_resolves.is_empty() || !frame.gpu_buffer_f.deferred_uv_copies.is_empty()
        {
            return Err("HAL external-image resolution is not implemented".into());
        }
        for updates in updates {
            self.update(updates)?;
        }
        let data = HashMap::from([
            (
                "sGpuBufferF",
                self.data_texture(&frame.gpu_buffer_f.data, wgt::TextureFormat::Rgba32Float)?,
            ),
            (
                "sGpuBufferI",
                self.data_texture(&frame.gpu_buffer_i.data, wgt::TextureFormat::Rgba32Sint)?,
            ),
            (
                "sTransformPalette",
                self.data_texture(&frame.transform_palette, wgt::TextureFormat::Rgba32Float)?,
            ),
            (
                "sRenderTasks",
                self.data_texture(
                    &frame.render_tasks.task_data,
                    wgt::TextureFormat::Rgba32Float,
                )?,
            ),
        ]);
        let mut stats = DrawStats::default();
        if !frame.has_been_rendered {
            for pass in &frame.passes {
                if !pass.alpha.targets.is_empty() {
                    return Err("HAL alpha targets are not implemented".into());
                }
                for target in &pass.color.targets {
                    self.target(target, &data, &mut stats)?;
                }
                for target in pass.texture_cache.values() {
                    self.target(target, &data, &mut stats)?;
                }
                for target in &pass.picture_cache {
                    let texture = self.surface(&target.surface)?;
                    match &target.kind {
                        PictureCacheTargetKind::Draw {
                            alpha_batch_container,
                        } => {
                            let mut draws = Vec::new();
                            if let Some(color) = target.clear_color {
                                draws.push(self.clear(target.dirty_rect, color));
                            }
                            self.batches(alpha_batch_container, target.dirty_rect, &mut draws)?;
                            self.draw_pass(&texture, &draws, &data, &mut stats)?;
                        }
                        PictureCacheTargetKind::Blit { .. } => {
                            return Err("HAL picture blits are not implemented".into())
                        }
                    }
                }
            }
        }
        let size = frame.device_rect.size();
        if frame.device_rect.min != DeviceIntPoint::zero() {
            return Err("HAL viewport offsets are not implemented".into());
        }
        let output = Texture::new(
            &self.owner,
            size.width as u32,
            size.height as u32,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            true,
        )?;
        let mut draws = vec![self.clear(frame.device_rect, clear)];
        for tile in frame.composite_state.tiles.iter().rev() {
            let state = &frame.composite_state;
            let rect = state.get_device_rect(&tile.local_rect, tile.transform_index);
            let valid = state.get_device_rect(&tile.local_valid_rect, tile.transform_index);
            let Some(clip_rect) = tile
                .device_clip_rect
                .intersection(&valid)
                .and_then(|r| r.intersection(&frame.device_rect.to_f32()))
            else {
                continue;
            };
            let transform = state.get_device_transform(tile.transform_index);
            let flip = (transform.scale.x < 0.0, transform.scale.y < 0.0);
            let clip = tile
                .clip_index
                .map(|index| state.get_compositor_clip(index));
            let (instance, source) = match tile.surface {
                CompositeTileSurface::Color { color } => (
                    CompositeInstance::new(rect, clip_rect, color.premultiplied(), flip, clip),
                    self.dummy.clone(),
                ),
                CompositeTileSurface::Texture { ref surface } => (
                    CompositeInstance::new(rect, clip_rect, PremultipliedColorF::WHITE, flip, clip),
                    self.surface(surface)?,
                ),
                CompositeTileSurface::ExternalSurface {
                    external_surface_index,
                } => {
                    let surface = &state.external_surfaces[external_surface_index.0];
                    match &surface.color_data {
                        ResolvedExternalSurfaceColorData::Rgb { plane, .. } => (
                            CompositeInstance::new_rgb(
                                rect,
                                clip_rect,
                                PremultipliedColorF::WHITE,
                                plane.uv_rect,
                                plane.texture.uses_normalized_uvs(),
                                flip,
                                clip,
                            ),
                            self.source(plane.texture)?,
                        ),
                        _ => return Err("HAL YUV composition is not implemented".into()),
                    }
                }
            };
            draws.push(Draw {
                shader: Shader::Composite,
                blend: 1,
                depth: 0,
                count: 1,
                instances: bytes(&[instance]).to_vec(),
                source,
                scissor: frame.device_rect,
            });
        }
        self.draw_pass(&output, &draws, &data, &mut stats)?;
        let layout = self.owner.layout(output.size.width, output.size.height)?;
        let buffer = self.owner.readback_buffer(&layout)?;
        let mut commands = Commands::<A>::new(&self.owner.open)?;
        output.transition(commands.encoder(), wgt::TextureUses::COPY_SRC);
        unsafe {
            copy_readback::<A>(
                commands.encoder(),
                &output.raw,
                &buffer,
                &layout,
                output.size,
                hal::FormatAspects::COLOR,
            );
        }
        commands.submit_and_wait()?;
        let pixels = self.owner.map_readback(&buffer, &layout)?;
        Ok(FrameOutput {
            size: [output.size.width, output.size.height],
            pixels,
            stats,
        })
    }
}

fn vertex_layouts(
    descriptor: &VertexDescriptor,
    shader: &ShaderArtifact,
) -> Result<(Vec<wgt::VertexAttribute>, Vec<wgt::VertexAttribute>, u64)> {
    let mut output = [Vec::new(), Vec::new()];
    let mut stride = 0;
    for (slot, attributes) in [descriptor.vertex_attributes, descriptor.instance_attributes]
        .iter()
        .enumerate()
    {
        let mut offset = 0;
        for attribute in *attributes {
            let (scalar, format, size) = match (&attribute.kind, attribute.count) {
                (VertexAttributeKind::U8Norm, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm8x2, 2)
                }
                (VertexAttributeKind::F32, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x2, 8)
                }
                (VertexAttributeKind::F32, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x4, 16)
                }
                (VertexAttributeKind::I32, 4) => {
                    (ScalarType::Sint, wgt::VertexFormat::Sint32x4, 16)
                }
                _ => return Err(format!("Unsupported HAL vertex attribute {attribute:?}")),
            };
            if let Some(input) = shader
                .inputs
                .iter()
                .find(|input| input.name == attribute.name)
            {
                if input.scalar != scalar || input.components != attribute.count {
                    return Err(format!("HAL vertex interface mismatch for {}", input.name));
                }
                output[slot].push(wgt::VertexAttribute {
                    format,
                    offset,
                    shader_location: input.location,
                });
            }
            offset += size;
        }
        if slot == 1 {
            stride = offset;
        }
    }
    if output[0].len() + output[1].len() != shader.inputs.len() {
        return Err("Missing HAL shader vertex attributes".into());
    }
    let [vertex, instances] = output;
    Ok((vertex, instances, stride))
}
