/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, wgt, Device};
use super::{
    images::Image, pipeline::DrawPipeline, program_store::ProgramStore,
    renderer_properties::RendererProperties, samplers::Samplers, shader::select_draw_shader,
    texture_store::TextureStore, upload_buffers::UploadBuffers, vertex_array::VertexArrayStore,
};
use crate::device::*;
use api::{units::*, ImageFormat};
use euclid::default::Transform3D;
use std::{
    borrow::Cow,
    cell::{Cell, OnceCell},
    collections::VecDeque,
    convert::TryFrom,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

type Result<T, E = String> = std::result::Result<T, E>;

struct Draw {
    pipeline: Arc<DrawPipeline>,
    bindings: Arc<core::binding_model::BindGroup>,
    vertices: Arc<core::resource::Buffer>,
    instances: Option<InstanceSlice>,
    indices: Option<(Arc<core::resource::Buffer>, wgt::IndexFormat)>,
    first: u32,
    count: u32,
    instance_count: u32,
    scissor: DeviceIntRect,
    viewport: DeviceIntRect,
    depth: std::ops::Range<f32>,
}

struct InstanceSlice {
    buffer: Arc<core::resource::Buffer>,
    offset: u64,
    size: u64,
}

#[derive(Default)]
// Pages stay mapped only while their draws are pending; sealed bytes are never reused.
struct InstanceArena {
    pages: Vec<Arc<core::resource::Buffer>>,
    used: u64,
    next_size: u64,
    #[cfg(test)]
    page_size: Option<u64>,
}

impl InstanceArena {
    const PAGE_SIZE: u64 = 16 * 1024;

    fn append(&mut self, owner: &Device, data: &[u8]) -> Result<InstanceSlice> {
        owner.check().map_err(|e| e.to_string())?;
        let size = u64::try_from(data.len()).map_err(|e| e.to_string())?;
        let mapped_size = size
            .checked_add(wgt::COPY_BUFFER_ALIGNMENT - 1)
            .map(|n| n / wgt::COPY_BUFFER_ALIGNMENT * wgt::COPY_BUFFER_ALIGNMENT)
            .filter(|n| *n != 0 && *n <= owner.limits().max_buffer_size && *n <= isize::MAX as u64)
            .ok_or("Vulkan instance upload exceeds limits")?;
        let offset = self
            .used
            .checked_add(wgt::MAP_ALIGNMENT - 1)
            .map(|n| n / wgt::MAP_ALIGNMENT * wgt::MAP_ALIGNMENT)
            .ok_or("Vulkan instance offset overflow")?;
        let fits = self.pages.last().is_some_and(|page| {
            offset
                .checked_add(mapped_size)
                .is_some_and(|end| end <= page.size())
        });
        let offset = if fits {
            offset
        } else {
            let capacity = mapped_size.max(self.next_size.min(owner.limits().max_buffer_size));
            let usage = if owner
                .features()
                .contains(wgt::Features::MAPPABLE_PRIMARY_BUFFERS)
            {
                wgt::BufferUsages::VERTEX | wgt::BufferUsages::MAP_WRITE
            } else {
                wgt::BufferUsages::VERTEX
            };
            let buffer = owner
                .create_buffer(&core::resource::BufferDescriptor {
                    label: Some("WR instance page".into()),
                    size: capacity,
                    usage,
                    mapped_at_creation: true,
                })
                .map_err(|e| e.to_string())?;
            self.pages.push(buffer);
            #[cfg(test)]
            let page_size = self.page_size.unwrap_or(Self::PAGE_SIZE);
            #[cfg(not(test))]
            let page_size = Self::PAGE_SIZE;
            self.next_size = capacity.min(page_size).saturating_mul(2).min(page_size);
            0
        };
        let buffer = self.pages.last().unwrap();
        {
            let mapping = buffer
                .get_mapped_range(offset, Some(mapped_size))
                .map_err(|e| e.to_string())?;
            mapping
                .write_slice()
                .slice(..data.len())
                .copy_from_slice(data);
        }
        self.used = offset + mapped_size;
        Ok(InstanceSlice {
            buffer: buffer.clone(),
            offset,
            size,
        })
    }

    fn seal(&mut self, owner: &Device) -> Result<()> {
        for page in self.pages.drain(..) {
            page.unmap();
        }
        self.used = 0;
        self.next_size = 0;
        owner.check().map_err(|e| e.to_string())
    }
}

struct DrawBindings {
    pipeline: Arc<DrawPipeline>,
    projection: Option<Arc<core::resource::Buffer>>,
    textures: Vec<(Arc<core::resource::TextureView>, TextureFilter)>,
    raw: Arc<core::binding_model::BindGroup>,
}

struct Pass {
    color: Arc<Image>,
    depth: Option<Arc<Image>>,
    descriptor: RenderPassDescriptor,
    level: u32,
    bounds: DeviceIntRect,
    viewport: DeviceIntRect,
    scissor: Option<DeviceIntRect>,
    started: bool,
    pending: Vec<Draw>,
    instances: InstanceArena,
}

pub(in crate::device) struct RenderDevice {
    pub owner: Arc<Device>,
    properties: RendererProperties,
    programs: ProgramStore,
    textures: TextureStore,
    vertex_arrays: VertexArrayStore,
    uploads: UploadBuffers,
    encoder: Option<Arc<core::command::CommandEncoder>>,
    pass: Option<Pass>,
    samplers: Samplers,
    last_bindings: Option<DrawBindings>,
    blit_pipelines: crate::internal_types::FastHashMap<wgt::TextureFormat, Arc<DrawPipeline>>,
    clear_pipelines: crate::internal_types::FastHashMap<
        (wgt::TextureFormat, bool, bool, bool),
        Arc<DrawPipeline>,
    >,
    quad: Arc<core::resource::Buffer>,
    quad_indices: Arc<core::resource::Buffer>,
    fallback: Arc<Image>,
    frame: GpuFrameId,
    inside_frame: bool,
    pub(super) submitted: core::SubmissionIndex,
    pub(super) completed: Arc<AtomicU64>,
    in_flight: VecDeque<core::SubmissionIndex>,
    failure: OnceCell<String>,
    out_of_memory: Cell<bool>,
}

fn bounds(image: &Image) -> DeviceIntRect {
    DeviceIntRect::from_size(DeviceIntSize::new(
        image.size().width as i32,
        image.size().height as i32,
    ))
}
fn projection(transform: &Transform3D<f32>) -> [f32; 16] {
    // Passthrough needs the HAL viewport Y flip and GL-to-Vulkan depth conversion.
    let mut result = transform.to_array();
    for c in [0, 4, 8, 12] {
        result[c + 1] = -result[c + 1];
        result[c + 2] = (result[c + 2] + result[c + 3]) * 0.5;
    }
    result
}

impl RenderDevice {
    pub fn new(owner: Arc<Device>) -> Result<Self> {
        let quad = owner
            .create_buffer_with_data(
                "WR quad",
                &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
                wgt::BufferUsages::VERTEX,
            )
            .map_err(|e| e.to_string())?;
        let indices: Vec<_> = [0u16, 1, 2, 2, 1, 3]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let quad_indices = owner
            .create_buffer_with_data("WR quad indices", &indices, wgt::BufferUsages::INDEX)
            .map_err(|e| e.to_string())?;
        let fallback = Image::new(
            &owner,
            1,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )?;
        let samplers = Samplers::new(&owner).map_err(|e| e.to_string())?;
        let mut result = Self {
            properties: RendererProperties::new(&owner),
            programs: ProgramStore::default(),
            textures: TextureStore::new(&owner),
            vertex_arrays: VertexArrayStore::default(),
            uploads: UploadBuffers::default(),
            encoder: None,
            pass: None,
            samplers,
            last_bindings: None,
            blit_pipelines: Default::default(),
            clear_pipelines: Default::default(),
            quad,
            quad_indices,
            fallback,
            frame: GpuFrameId::new(0),
            inside_frame: false,
            submitted: 0,
            completed: Arc::new(AtomicU64::new(0)),
            in_flight: VecDeque::new(),
            failure: OnceCell::new(),
            out_of_memory: Cell::new(false),
            owner,
        };
        let encoder = result.commands()?;
        result
            .owner
            .upload_texture(
                &encoder,
                &result.fallback.raw,
                0,
                bounds(&result.fallback),
                &[255; 4],
                None,
                0,
                None,
            )
            .map_err(|e| e.to_string())?;
        Ok(result)
    }
    pub fn failure(&self) -> Option<&str> {
        if self.failure.get().is_none() {
            if let Some(error) = self.owner.failure() {
                if error.kind == wgt::error::ErrorType::OutOfMemory {
                    self.out_of_memory.set(true);
                }
                let _ = self.failure.set(error.to_string());
            }
        }
        self.failure.get().map(String::as_str)
    }
    pub fn operation<T>(&mut self, op: impl FnOnce(&mut Self) -> Result<T>) -> Option<T> {
        if self.failure.get().is_some() {
            return None;
        }
        let result = op(self);
        self.record_result(result)
    }
    fn record_result<T>(&mut self, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                if self.failure.get().is_none()
                    && self
                        .owner
                        .failure()
                        .is_some_and(|e| e.kind == wgt::error::ErrorType::OutOfMemory)
                {
                    self.out_of_memory.set(true);
                }
                let _ = self.failure.set(error);
                self.pass = None;
                self.last_bindings = None;
                self.inside_frame = false;
                self.encoder = None;
                None
            }
        }
    }
    fn commands(&mut self) -> Result<Arc<core::command::CommandEncoder>> {
        if self.encoder.is_none() {
            self.encoder = Some(
                self.owner
                    .create_command_encoder("WR commands")
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(self.encoder.as_ref().unwrap().clone())
    }
    pub(super) fn poll(&mut self) -> Result<()> {
        self.owner.poll().map_err(|e| e.to_string())?;
        while self
            .in_flight
            .front()
            .is_some_and(|i| *i <= self.completed.load(Ordering::Acquire))
        {
            self.in_flight.pop_front();
        }
        Ok(())
    }
    pub(super) fn submit(&mut self) -> Result<core::SubmissionIndex> {
        self.flush_pass(StoreOp::Store)?;
        self.poll()?;
        if self.in_flight.len() >= 2 {
            let index = self.in_flight.pop_front().unwrap();
            self.owner.wait_for(index).map_err(|e| e.to_string())?;
            self.completed.fetch_max(index, Ordering::Release);
        }
        let commands = self
            .encoder
            .take()
            .map(|encoder| encoder.finish(&wgt::CommandBufferDescriptor::default()));
        let index = self
            .owner
            .submit(commands.as_slice())
            .map_err(|e| e.to_string())?;
        let completed = self.completed.clone();
        self.owner.queue.on_submitted_work_done(Box::new(move || {
            completed.fetch_max(index, Ordering::Release);
        }));
        self.owner.check().map_err(|e| e.to_string())?;
        self.submitted = index;
        self.in_flight.push_back(index);
        Ok(index)
    }
    pub fn begin_frame(&mut self) -> Result<GpuFrameId> {
        if self.inside_frame {
            return Err("A Vulkan frame is already active".into());
        }
        self.poll()?;
        self.frame = GpuFrameId::new(
            self.frame
                .0
                .checked_add(1)
                .ok_or("Vulkan frame identifiers exhausted")?,
        );
        self.inside_frame = true;
        self.programs.unbind();
        self.vertex_arrays.unbind();
        self.textures.begin_frame(self.frame);
        Ok(self.frame)
    }
    pub fn end_frame(&mut self) -> Result<core::SubmissionIndex> {
        if !self.inside_frame || self.pass.is_some() {
            return Err("Vulkan frame has unfinished passes".into());
        }
        let index = self.submit()?;
        self.inside_frame = false;
        Ok(index)
    }
    pub fn begin_render_pass(&mut self, descriptor: &RenderPassDescriptor) -> Result<()> {
        if !self.inside_frame || self.pass.is_some() {
            return Err("Invalid Vulkan render-pass nesting".into());
        }
        let (color, depth, viewport) = self.textures.draw_target(descriptor.target)?;
        if matches!(descriptor.depth_load, LoadOp::Clear(_)) && depth.is_none() {
            return Err("Depth clear requires an attachment".into());
        }
        let viewport = viewport.unwrap_or_else(|| bounds(&color));
        self.textures.clear_color_bindings();
        let target_bounds = bounds(&color);
        self.pass = Some(Pass {
            color,
            depth,
            viewport,
            level: 0,
            bounds: target_bounds,
            descriptor: *descriptor,
            scissor: None,
            started: false,
            pending: Vec::new(),
            instances: InstanceArena::default(),
        });
        Ok(())
    }
    pub fn end_render_pass(&mut self, depth_store: StoreOp) -> Result<()> {
        if self.pass.is_none() {
            return Err("No Vulkan render pass is active".into());
        }
        self.flush_pass(depth_store)?;
        self.pass = None;
        Ok(())
    }
    fn flush_pass(&mut self, depth_store: StoreOp) -> Result<()> {
        let Some(mut pass) = self.pass.take() else {
            return Ok(());
        };
        if pass.started && pass.pending.is_empty() && depth_store == StoreOp::Store {
            self.pass = Some(pass);
            return Ok(());
        }
        let full = pass.bounds;
        let area = pass
            .descriptor
            .render_area
            .unwrap_or(full)
            .intersection(&full)
            .unwrap_or_else(DeviceIntRect::zero);
        let whole = area == full && pass.viewport == full;
        let color_load = if pass.started {
            LoadOp::Load
        } else {
            pass.descriptor.color_load
        };
        let depth_load = if pass.started {
            LoadOp::Load
        } else {
            pass.descriptor.depth_load
        };
        if !whole && !pass.started {
            let color = match color_load {
                LoadOp::Clear(c) => Some(c),
                _ => None,
            };
            let depth = match depth_load {
                LoadOp::Clear(d) => Some(d),
                _ => None,
            };
            if let Some(clear) = self.clear_draw(&pass, area, color, depth)? {
                pass.pending.insert(0, clear);
            }
        }
        pass.instances.seal(&self.owner)?;
        let encoder = self.commands()?;
        let mut native = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
            label: Some("WR render pass".into()),
            color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
                view: pass.color.levels[pass.level as usize].clone(),
                depth_slice: None,
                resolve_target: None,
                load_op: match color_load {
                    LoadOp::Clear(c) if whole => wgt::LoadOp::Clear(wgt::Color {
                        r: c[0] as f64,
                        g: c[1] as f64,
                        b: c[2] as f64,
                        a: c[3] as f64,
                    }),
                    LoadOp::DontCare if whole => wgt::LoadOp::Clear(wgt::Color::TRANSPARENT),
                    _ => wgt::LoadOp::Load,
                },
                store_op: wgt::StoreOp::Store,
            })]),
            depth_stencil_attachment: pass.depth.as_ref().map(|depth| {
                core::command::RenderPassDepthStencilAttachment {
                    view: depth.levels[0].clone(),
                    depth: core::command::PassChannel {
                        load_op: Some(match depth_load {
                            LoadOp::Clear(d) if whole => wgt::LoadOp::Clear(Some(d)),
                            LoadOp::DontCare if whole => wgt::LoadOp::Clear(Some(1.0)),
                            _ => wgt::LoadOp::Load,
                        }),
                        store_op: Some(if depth_store == StoreOp::Discard {
                            wgt::StoreOp::Discard
                        } else {
                            wgt::StoreOp::Store
                        }),
                        read_only: false,
                    },
                    stencil: core::command::PassChannel {
                        load_op: None,
                        store_op: None,
                        read_only: true,
                    },
                }
            }),
            ..Default::default()
        });
        for draw in pass.pending.drain(..) {
            native.set_pipeline(draw.pipeline.raw.clone());
            native.set_bind_group(
                0,
                Some(draw.bindings),
                if draw.pipeline.shader.projection_stages != 0 {
                    &[0]
                } else {
                    &[]
                },
            );
            native.set_viewport(
                draw.viewport.min.x as f32,
                draw.viewport.min.y as f32,
                draw.viewport.width() as f32,
                draw.viewport.height() as f32,
                draw.depth.start,
                draw.depth.end,
            );
            native.set_scissor_rect(
                draw.scissor.min.x as u32,
                draw.scissor.min.y as u32,
                draw.scissor.width() as u32,
                draw.scissor.height() as u32,
            );
            native.set_vertex_buffer(0, Some(draw.vertices), 0, None);
            if let Some(instances) = draw.instances {
                native.set_vertex_buffer(
                    1,
                    Some(instances.buffer),
                    instances.offset,
                    Some(instances.size),
                );
            }
            if let Some((indices, format)) = draw.indices {
                native.set_index_buffer(indices, format, 0, None);
                native.draw_indexed(draw.count, draw.instance_count, draw.first, 0, 0);
            } else {
                native.draw(draw.count, draw.instance_count, draw.first, 0);
            }
        }
        native.end();
        self.owner.check().map_err(|e| e.to_string())?;
        pass.started = true;
        self.pass = Some(pass);
        Ok(())
    }
    fn clear_draw(
        &mut self,
        pass: &Pass,
        rect: DeviceIntRect,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
    ) -> Result<Option<Draw>> {
        if color.is_none() && depth.is_none() {
            return Ok(None);
        }
        if depth.is_some_and(|d| !(0.0..=1.0).contains(&d))
            || (depth.is_some() && pass.depth.is_none())
        {
            return Err("Invalid Vulkan depth clear".into());
        }
        let Some(scissor) = rect.intersection(&pass.bounds) else {
            return Ok(None);
        };
        let key = (
            pass.color.format(),
            pass.depth.is_some(),
            color.is_some(),
            depth.is_some(),
        );
        let pipeline = if let Some(pipeline) = self.clear_pipelines.get(&key) {
            pipeline.clone()
        } else {
            let pipeline = DrawPipeline::new(
                &self.owner,
                select_draw_shader("ps_clear", &[], false)?,
                key.0,
                key.1,
                RenderState {
                    color_write: key.2,
                    depth_write: key.3,
                    depth_test: Some(DepthFunction::Always),
                    ..Default::default()
                },
                wgt::PrimitiveTopology::TriangleList,
            )
            .map_err(|e| e.to_string())?;
            self.clear_pipelines.insert(key, pipeline.clone());
            pipeline
        };
        let full = pass.bounds;
        let matrix = projection(&Transform3D::ortho(
            0.,
            full.width() as f32,
            0.,
            full.height() as f32,
            -1.,
            1.,
        ));
        let uniform = self
            .owner
            .create_buffer_with_data(
                "WR clear projection",
                crate::device::as_bytes(&matrix),
                wgt::BufferUsages::UNIFORM,
            )
            .map_err(|e| e.to_string())?;
        let bindings = pipeline
            .create_bindings(Some(uniform), &[], &[], &self.samplers)
            .map_err(|e| e.to_string())?;
        let c = color.unwrap_or([0.; 4]);
        let instances = self
            .owner
            .create_buffer_with_data(
                "WR clear",
                crate::device::as_bytes(&[
                    0.,
                    0.,
                    full.width() as f32,
                    full.height() as f32,
                    c[0],
                    c[1],
                    c[2],
                    c[3],
                ]),
                wgt::BufferUsages::VERTEX,
            )
            .map_err(|e| e.to_string())?;
        let d = depth.unwrap_or(0.);
        Ok(Some(Draw {
            pipeline,
            bindings,
            vertices: self.quad.clone(),
            instances: Some(InstanceSlice {
                buffer: instances,
                offset: 0,
                size: 32,
            }),
            indices: Some((self.quad_indices.clone(), wgt::IndexFormat::Uint16)),
            first: 0,
            count: 6,
            instance_count: 1,
            scissor,
            viewport: full,
            depth: d..d,
        }))
    }
    pub fn clear_target(
        &mut self,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
        rect: Option<FramebufferIntRect>,
    ) -> Result<()> {
        let mut pass = self.pass.take().ok_or("No Vulkan render pass is active")?;
        let rect = rect.map(|r| r.cast_unit()).unwrap_or(pass.bounds);
        if let Some(draw) = self.clear_draw(&pass, rect, color, depth)? {
            pass.pending.push(draw);
        }
        self.pass = Some(pass);
        Ok(())
    }
    pub fn draw(
        &mut self,
        first: i32,
        count: i32,
        instances: i32,
        base: u32,
        index_format: Option<wgt::IndexFormat>,
        topology: wgt::PrimitiveTopology,
        instance_data: Option<(&Buffer, &[u8])>,
    ) -> Result<()> {
        let pass = self
            .pass
            .as_ref()
            .ok_or("No Vulkan render pass is active")?;
        if first < 0 || count < 0 || instances < 0 {
            return Err("Negative Vulkan draw range".into());
        }
        if count == 0 || instances == 0 {
            return Ok(());
        }
        let Some(scissor) = pass
            .scissor
            .unwrap_or(pass.bounds)
            .intersection(&pass.bounds)
        else {
            return Ok(());
        };
        let viewport = pass.viewport;
        let pipeline = self
            .programs
            .pipeline(
                &self.owner,
                pass.color.format(),
                pass.depth.is_some(),
                topology,
            )
            .map_err(|e| e.to_string())?;
        let program = self.programs.current()?;
        let mut textures = Vec::new();
        for (binding, slot) in program.shader.textures.iter().zip(&program.slots) {
            let resource = self
                .textures
                .binding(*slot)
                .ok_or("Invalid Vulkan texture slot")?;
            let (image, filter) = match resource {
                Some(resource) => resource,
                None if matches!(
                    binding.name,
                    "sColor0" | "sColor1" | "sColor2" | "sClipMask"
                ) =>
                {
                    (self.fallback.clone(), TextureFilter::Nearest)
                }
                None => return Err(format!("Missing Vulkan texture {}", binding.name)),
            };
            if Arc::ptr_eq(&image.raw, &pass.color.raw)
                || pass
                    .depth
                    .as_ref()
                    .is_some_and(|d| Arc::ptr_eq(&image.raw, &d.raw))
            {
                return Err("Vulkan draw attachment feedback is unsupported".into());
            }
            textures.push((image.sampled.clone(), filter));
        }
        let uniform = if program.shader.projection_stages != 0 {
            let mut snapshot = program.projection.borrow_mut();
            if snapshot.is_none() {
                *snapshot = Some(
                    self.owner
                        .create_buffer_with_data(
                            "WR projection snapshot",
                            crate::device::as_bytes(&projection(&program.transform.get())),
                            wgt::BufferUsages::UNIFORM,
                        )
                        .map_err(|e| e.to_string())?,
                );
            }
            snapshot.clone()
        } else {
            None
        };
        let bindings = if let Some(cached) = self.last_bindings.as_ref().filter(|cached| {
            Arc::ptr_eq(&cached.pipeline, &pipeline)
                && match (&cached.projection, &uniform) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
                && cached.textures.len() == textures.len()
                && cached
                    .textures
                    .iter()
                    .zip(&textures)
                    .all(|((a, af), (b, bf))| Arc::ptr_eq(a, b) && af == bf)
        }) {
            cached.raw.clone()
        } else {
            let raw = pipeline
                .create_bindings(uniform.clone(), &textures, &[], &self.samplers)
                .map_err(|e| e.to_string())?;
            self.last_bindings = Some(DrawBindings {
                pipeline: pipeline.clone(),
                projection: uniform,
                textures,
                raw: raw.clone(),
            });
            raw
        };
        let vertex_arrays = &self.vertex_arrays;
        let array = vertex_arrays
            .bound
            .and_then(|id| vertex_arrays.arrays.get(&id))
            .ok_or("No Vulkan vertex array is bound")?;
        let actual = super::vertex_layout::vertex_layouts(&array.descriptor, pipeline.shader)?;
        let expected = super::vertex_layout::vertex_layouts(
            super::shader::draw_vertex_descriptor(pipeline.shader)?,
            pipeline.shader,
        )?;
        if actual != expected {
            return Err("Vulkan vertex array does not match the shader".into());
        }
        let (vertices, instance_buffer, index_buffer, first) = if pipeline.instance_stride != 0 {
            if array.divisor != 1
                || count != 6
                || index_format != Some(wgt::IndexFormat::Uint16)
                || first != 0
            {
                return Err("Vulkan instanced draws require canonical indexed quads".into());
            }
            let data = array
                .instances
                .and_then(|id| vertex_arrays.buffers.get(&id))
                .ok_or("Missing Vulkan instance buffer")?;
            let start = u64::from(base)
                .checked_mul(pipeline.instance_stride)
                .ok_or("Vulkan instance offset overflow")?;
            let end = u64::from(instances as u32)
                .checked_mul(pipeline.instance_stride)
                .and_then(|n| n.checked_add(start))
                .ok_or("Vulkan instance range overflow")?;
            let source = if let Some((buffer, bytes)) = instance_data {
                self.vertex_arrays.buffer_data(buffer)?;
                if array.instances != Some(buffer.id) {
                    return Err("Vulkan streaming buffer does not match the vertex array".into());
                }
                bytes
            } else {
                &data.bytes
            };
            let bytes = source
                .get(
                    usize::try_from(start).map_err(|e| e.to_string())?
                        ..usize::try_from(end).map_err(|e| e.to_string())?,
                )
                .ok_or("Vulkan draw exceeds instance data")?;
            let instance_buffer = self
                .pass
                .as_mut()
                .unwrap()
                .instances
                .append(&self.owner, bytes)?;
            (
                self.quad.clone(),
                Some(instance_buffer),
                Some((self.quad_indices.clone(), wgt::IndexFormat::Uint16)),
                0,
            )
        } else {
            if instance_data.is_some() {
                return Err("Vulkan streaming draws require an instanced shader".into());
            }
            let bytes = &self
                .vertex_arrays
                .buffers
                .get(&array.vertices)
                .ok_or("Missing Vulkan vertices")?
                .bytes;
            let vertex = self
                .owner
                .create_buffer_with_data(
                    "WR debug vertex snapshot",
                    bytes,
                    wgt::BufferUsages::VERTEX,
                )
                .map_err(|e| e.to_string())?;
            let indices = if let Some(format) = index_format {
                let bytes = &array
                    .indices
                    .and_then(|id| vertex_arrays.buffers.get(&id))
                    .ok_or("Missing Vulkan indices")?
                    .bytes;
                Some((
                    self.owner
                        .create_buffer_with_data(
                            "WR debug index snapshot",
                            bytes,
                            wgt::BufferUsages::INDEX,
                        )
                        .map_err(|e| e.to_string())?,
                    format,
                ))
            } else {
                None
            };
            (vertex, None, indices, first as u32)
        };
        let draw = Draw {
            pipeline,
            bindings,
            vertices,
            instances: instance_buffer,
            indices: index_buffer,
            first,
            count: count as u32,
            instance_count: instances as u32,
            scissor,
            viewport,
            depth: 0.0..1.0,
        };
        self.pass.as_mut().unwrap().pending.push(draw);
        Ok(())
    }
    pub fn upload_texture_region(
        &mut self,
        texture: &Texture,
        rect: DeviceIntRect,
        stride: Option<i32>,
        format: Option<ImageFormat>,
        data: &[u8],
    ) -> Result<()> {
        self.flush_pass(StoreOp::Store)?;
        let image = self.textures.image(texture)?;
        let encoder = self.commands()?;
        self.owner
            .upload_texture(&encoder, &image.raw, 0, rect, data, stride, 0, format)
            .map_err(|e| e.to_string())?;
        self.generate_mipmaps(&image)
    }
    pub fn upload_texture_immediate(&mut self, texture: &Texture, data: &[u8]) -> Result<()> {
        self.upload_texture_region(
            texture,
            DeviceIntRect::from_size(texture.size),
            None,
            None,
            data,
        )
    }
    pub fn flush_upload_buffer(
        &mut self,
        buffer: &TransferBuffer,
        mapping: &UploadBufferMapping,
        used: usize,
        chunks: &[UploadChunk<'_>],
    ) -> Result<()> {
        if !chunks.is_empty() {
            self.flush_pass(StoreOp::Store)?;
        }
        let encoder = self.commands()?;
        let bytes = self.uploads.flush(buffer, mapping, used)?;
        let mut images = Vec::new();
        for chunk in chunks {
            let image = self.textures.image(chunk.texture)?;
            self.owner
                .upload_texture(
                    &encoder,
                    &image.raw,
                    0,
                    chunk.rect,
                    bytes,
                    chunk.stride,
                    i32::try_from(chunk.offset).map_err(|e| e.to_string())?,
                    chunk.format_override,
                )
                .map_err(|e| e.to_string())?;
            if !images.iter().any(|i| Arc::ptr_eq(i, &image)) {
                images.push(image);
            }
        }
        for image in images {
            self.generate_mipmaps(&image)?;
        }
        Ok(())
    }
    pub(super) fn capture_into(
        &mut self,
        image: Arc<Image>,
        rect: DeviceIntRect,
        format: ImageFormat,
        output: &mut [u8],
    ) -> Result<()> {
        self.flush_pass(StoreOp::Store)?;
        let encoder = self.commands()?;
        let readback = self
            .owner
            .encode_readback(&encoder, &image.raw, 0, rect, Some(format))
            .map_err(|e| e.to_string())?;
        let index = self.submit()?;
        readback
            .map(&self.owner, index)
            .map_err(|e| e.to_string())?
            .wait_into(&self.owner, output)
            .map_err(|e| e.to_string())
    }
}

#[path = "texture_blit.rs"]
mod texture_blit;

#[cfg(test)]
#[path = "instance_tests.rs"]
mod instance_tests;

#[path = "gpu_backend.rs"]
mod backend;

#[cfg(test)]
#[path = "render_device_tests.rs"]
mod tests;
