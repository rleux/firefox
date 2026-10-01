/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::bindings::DrawBindings;
use super::pipeline::DrawPipeline;
use super::{hal, wgt, Buffer, Device, Recording, Samplers, SubmissionQueue, Texture, TextureFilter};
use api::units::{DeviceIntPoint, DeviceIntRect};
use ash::vk;
use euclid::default::Transform3D;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use wgpu_hal::CommandEncoder as _;

pub(super) fn upload_projection(
    commands: &mut Recording<'_>,
    uploads: &SubmissionQueue,
    matrix: &[f32; 16],
) -> Result<Rc<Buffer>, String> {
    uploads.upload_in_recording(commands, 64, wgt::BufferUses::UNIFORM, |bytes| {
        for (destination, value) in bytes.chunks_exact_mut(4).zip(matrix) {
            destination.copy_from_slice(&value.to_ne_bytes());
        }
        Ok(())
    })
}

pub(super) struct Draw {
    pub bindings: Rc<DrawBindings>,
    pub instances: Rc<Buffer>,
    pub instance_offset: u64,
    pub instance_count: u32,
    pub scissor: DeviceIntRect,
}

/// Resource slices follow the pipeline's reflected binding order.
pub(super) struct DrawBatch<'a> {
    pub pipeline: &'a Rc<DrawPipeline>,
    /// Column-major transform producing Vulkan clip coordinates.
    pub projection: Option<&'a [f32; 16]>,
    pub textures: &'a [(Rc<Texture>, TextureFilter)],
    pub buffers: &'a [Rc<Buffer>],
    pub instances: &'a [u8],
    pub instance_count: u32,
    pub scissor: DeviceIntRect,
}

pub(super) trait ColorAttachment {
    fn owner(&self) -> &Rc<Device>;
    fn size(&self) -> wgt::Extent3d;
    fn format(&self) -> wgt::TextureFormat;
    fn target_view(&self) -> Option<&hal::vulkan::TextureView>;
    fn initialized(&self) -> bool;
    fn validate_recording(&self, commands: &Recording<'_>) -> Result<(), String>;
    fn prepare(&self, commands: &mut Recording<'_>) -> Result<(), String>;
    fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String>;
    fn is_sampled_by(&self, texture: &Texture) -> bool;
}

impl ColorAttachment for &Rc<Texture> {
    fn owner(&self) -> &Rc<Device> {
        &self.raw.owner
    }
    fn size(&self) -> wgt::Extent3d {
        Texture::size(self)
    }
    fn format(&self) -> wgt::TextureFormat {
        Texture::format(self)
    }
    fn target_view(&self) -> Option<&hal::vulkan::TextureView> {
        Texture::target_view(self)
    }
    fn initialized(&self) -> bool {
        Texture::initialized(self)
    }
    fn validate_recording(&self, commands: &Recording<'_>) -> Result<(), String> {
        commands.recording_id(&self.raw.owner).map(|_| ())
    }
    fn prepare(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        Texture::transition(self, commands, wgt::TextureUses::COLOR_TARGET)
    }
    fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        Texture::initialize(self, commands)
    }
    fn is_sampled_by(&self, texture: &Texture) -> bool {
        texture.samples_attachment(self)
    }
}

pub(super) struct DrawPass<'a, T: ColorAttachment = &'a Rc<Texture>> {
    pub target: T,
    /// Logical pixel coordinate at the attachment's top-left.
    pub origin: DeviceIntPoint,
    /// Attachment-local viewport; None uses the full attachment.
    pub viewport: Option<DeviceIntRect>,
    pub depth: Option<&'a Rc<Texture>>,
    pub clear_color: Option<wgt::Color>,
    pub clear_depth: Option<f32>,
    pub depth_range: Range<f32>,
}

impl<T: ColorAttachment> DrawPass<'_, T> {
    pub fn clear_rect(
        &self,
        commands: &mut Recording<'_>,
        rect: DeviceIntRect,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
    ) -> Result<(), String> {
        let full = self.validate(commands)?;
        if depth.map_or(false, |value| !(0.0..=1.0).contains(&value)) {
            return Err("Invalid Vulkan depth clear value".into());
        }
        if depth.is_some() && self.depth.is_none() {
            return Err("Vulkan depth clear requires an attachment".into());
        }
        if color.is_none() && depth.is_none() {
            return Ok(());
        }
        let Some(rect) = rect.intersection(&full) else {
            return Ok(());
        };
        let mut attachments = [vk::ClearAttachment::default(); 2];
        let mut count = 0;
        if let Some(color) = color {
            attachments[count] = vk::ClearAttachment {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                color_attachment: 0,
                clear_value: vk::ClearValue {
                    color: vk::ClearColorValue { float32: color },
                },
            };
            count += 1;
        }
        if let Some(depth) = depth {
            attachments[count] = vk::ClearAttachment {
                aspect_mask: vk::ImageAspectFlags::DEPTH,
                color_attachment: 0,
                clear_value: vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue { depth, stencil: 0 },
                },
            };
            count += 1;
        }
        self.record_pass(commands, |encoder| unsafe {
            self.target
                .owner()
                .open
                .device
                .raw_device()
                .cmd_clear_attachments(
                    encoder.raw_handle(),
                    &attachments[..count],
                    &[vk::ClearRect {
                        rect: vk::Rect2D {
                            offset: vk::Offset2D {
                                x: rect.min.x - self.origin.x,
                                y: rect.min.y - self.origin.y,
                            },
                            extent: vk::Extent2D {
                                width: rect.width() as u32,
                                height: rect.height() as u32,
                            },
                        },
                        base_array_layer: 0,
                        layer_count: 1,
                    }],
                );
        })
    }

    /// Convert WebRender's GL clip transform for this target's logical origin.
    pub fn projection(&self, transform: &Transform3D<f32>) -> [f32; 16] {
        let size = self.target.size();
        let (width, height) = self.viewport.map_or(
            (size.width as f32, size.height as f32),
            |rect| (rect.width() as f32, rect.height() as f32),
        );
        let dx = 2.0 * self.origin.x as f32 / width;
        let dy = 2.0 * self.origin.y as f32 / height;
        let mut matrix = transform.to_array();
        for column in [0, 4, 8, 12] {
            let w = matrix[column + 3];
            matrix[column] -= dx * w;
            matrix[column + 1] = -matrix[column + 1] + dy * w;
            matrix[column + 2] = (matrix[column + 2] + w) * 0.5;
        }
        matrix
    }

    fn bounds(&self) -> Result<DeviceIntRect, String> {
        let size = self.target.size();
        let x = self
            .origin
            .x
            .checked_add(size.width as i32)
            .ok_or("Vulkan draw target X coordinate overflow")?;
        let y = self
            .origin
            .y
            .checked_add(size.height as i32)
            .ok_or("Vulkan draw target Y coordinate overflow")?;
        Ok(DeviceIntRect::new(self.origin, DeviceIntPoint::new(x, y)))
    }

    pub fn record_batches(
        &self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        quad: &Rc<Buffer>,
        samplers: Option<&Rc<Samplers>>,
        batches: &[DrawBatch<'_>],
    ) -> Result<(), String> {
        let owner = self.target.owner();
        commands.recording_id(owner)?;
        let full = self.bounds()?;
        let active: Vec<_> = batches
            .iter()
            .filter(|batch| {
                batch.instance_count != 0 && batch.scissor.intersection(&full).is_some()
            })
            .collect();
        for batch in &active {
            if !Rc::ptr_eq(&batch.pipeline.raw.owner, owner)
                || batch.pipeline.format != self.target.format()
                || batch.pipeline.has_depth != self.depth.is_some()
            {
                return Err("Draw batch pipeline does not match the render pass".into());
            }
            let bytes = batch
                .pipeline
                .instance_stride
                .checked_mul(u64::from(batch.instance_count))
                .ok_or("Draw batch instance size overflow")?;
            if bytes != batch.instances.len() as u64 {
                return Err("Draw batch data does not match the instance count and stride".into());
            }
            if batch.pipeline.shader.projection_stages != 0 && batch.projection.is_none() {
                return Err("Missing draw batch projection".into());
            }
        }
        let sizes: Vec<_> = active.iter().map(|batch| batch.instances.len()).collect();
        let instances = uploads.upload_instances_with(commands, &sizes, |index, destination| {
            destination.copy_from_slice(active[index].instances);
            Ok(())
        })?;
        let mut draws = Vec::with_capacity(active.len());
        for (index, batch) in active.iter().enumerate() {
            let projection = if batch.pipeline.shader.projection_stages != 0 {
                let matrix = batch.projection.unwrap();
                Some(upload_projection(commands, uploads, matrix)?)
            } else {
                None
            };
            let bindings = DrawBindings::new(
                batch.pipeline,
                projection,
                batch.textures.to_vec(),
                batch.buffers.to_vec(),
                samplers.cloned(),
            )?;
            let (buffer, range) = instances.buffer_range(index)?;
            draws.push(Draw {
                bindings,
                instances: buffer.clone(),
                instance_offset: range.start,
                instance_count: batch.instance_count,
                scissor: batch.scissor,
            });
        }
        self.record(commands, quad, &draws)
    }

    pub fn record(
        &self,
        commands: &mut Recording<'_>,
        quad: &Rc<Buffer>,
        draws: &[Draw],
    ) -> Result<(), String> {
        let full = self.validate(commands)?;
        let owner = self.target.owner();
        let size = self.target.size();
        let viewport = self.viewport.unwrap_or_else(|| {
            DeviceIntRect::from_size(api::units::DeviceIntSize::new(
                size.width as i32, size.height as i32,
            ))
        });
        let mut buffers: HashMap<*const Buffer, (&Rc<Buffer>, wgt::BufferUses)> = HashMap::new();
        fn add_buffer<'a>(
            buffers: &mut HashMap<*const Buffer, (&'a Rc<Buffer>, wgt::BufferUses)>,
            buffer: &'a Rc<Buffer>,
            usage: wgt::BufferUses,
        ) {
            buffers
                .entry(Rc::as_ptr(buffer))
                .and_modify(|entry| entry.1 |= usage)
                .or_insert((buffer, usage));
        }
        let mut active = Vec::new();
        for draw in draws {
            let Some(scissor) = draw.scissor.intersection(&full) else {
                continue;
            };
            if draw.instance_count == 0 {
                continue;
            }
            let pipeline = &draw.bindings.pipeline;
            if !Rc::ptr_eq(&pipeline.raw.owner, owner)
                || pipeline.format != self.target.format()
                || pipeline.has_depth != self.depth.is_some()
                || !Rc::ptr_eq(&draw.instances.raw.owner, owner)
            {
                return Err("Draw resources do not match the render pass".into());
            }
            let bytes = pipeline
                .instance_stride
                .checked_mul(u64::from(draw.instance_count))
                .ok_or("Draw instance range overflow")?;
            let instances = draw.instances.vertex_binding(draw.instance_offset, bytes)?;
            for texture in draw.bindings.textures() {
                if self.target.is_sampled_by(texture)
                    || self
                        .depth
                        .map_or(false, |depth| texture.samples_attachment(depth))
                {
                    return Err("Draw attachment feedback is unsupported".into());
                }
                if !texture.sample_initialized() {
                    return Err("Draw samples an uninitialized texture".into());
                }
            }
            add_buffer(&mut buffers, &draw.instances, wgt::BufferUses::VERTEX);
            for (buffer, usage) in draw.bindings.buffer_uses() {
                add_buffer(&mut buffers, buffer, usage);
            }
            active.push((draw, scissor, instances));
        }
        if !active.is_empty() {
            if !Rc::ptr_eq(&quad.raw.owner, owner) {
                return Err("Draw vertices belong to another device".into());
            }
            quad.vertex_binding(0, 16)?;
            add_buffer(&mut buffers, quad, wgt::BufferUses::VERTEX);
        }
        for (_, (buffer, usage)) in buffers {
            buffer.transition(commands, usage)?;
        }
        for (draw, _, _) in &active {
            for texture in draw.bindings.textures() {
                texture.transition(commands, wgt::TextureUses::RESOURCE)?;
            }
            commands.keep(draw.bindings.clone());
        }
        self.record_pass(commands, |encoder| unsafe {
            encoder.set_viewport(
                &hal::Rect {
                    x: viewport.min.x as f32,
                    y: viewport.min.y as f32,
                    w: viewport.width() as f32,
                    h: viewport.height() as f32,
                },
                self.depth_range.clone(),
            );
            if !active.is_empty() {
                encoder.set_vertex_buffer(0, quad.binding());
            }
            for (draw, scissor, instances) in active {
                let pipeline = &draw.bindings.pipeline;
                encoder.set_scissor_rect(&hal::Rect {
                    x: (scissor.min.x - self.origin.x) as u32,
                    y: (scissor.min.y - self.origin.y) as u32,
                    w: scissor.width() as u32,
                    h: scissor.height() as u32,
                });
                encoder.set_render_pipeline(&pipeline.raw);
                encoder.set_bind_group(&pipeline.layout, 0, &draw.bindings.raw, &[]);
                encoder.set_vertex_buffer(1, instances);
                encoder.draw(0, 4, 0, draw.instance_count);
            }
        })
    }

    pub(super) fn validate(&self, commands: &Recording<'_>) -> Result<DeviceIntRect, String> {
        let owner = self.target.owner();
        self.target.validate_recording(commands)?;
        let size = self.target.size();
        if let Some(rect) = self.viewport {
            if rect.min.x < 0
                || rect.min.y < 0
                || rect.max.x <= rect.min.x
                || rect.max.y <= rect.min.y
                || rect.width() as u32 > owner.max_viewport_dimensions[0]
                || rect.height() as u32 > owner.max_viewport_dimensions[1]
                || (rect.min.x as f32) < owner.viewport_bounds_range[0]
                || (rect.min.y as f32) < owner.viewport_bounds_range[0]
                || rect.max.x as f32 > owner.viewport_bounds_range[1]
                || rect.max.y as f32 > owner.viewport_bounds_range[1]
            {
                return Err("Invalid Vulkan draw viewport".into());
            }
        }
        self.target
            .target_view()
            .ok_or("Draw target is not renderable")?;
        if !matches!(
            self.target.format().sample_type(None, Some(owner.features)),
            Some(wgt::TextureSampleType::Float { .. })
        ) {
            return Err("Invalid draw color attachment".into());
        }
        if let Some(depth) = self.depth {
            if !Rc::ptr_eq(&depth.raw.owner, owner)
                || depth.format() != wgt::TextureFormat::Depth32Float
                || depth.size() != size
                || depth.target_view().is_none()
            {
                return Err("Invalid draw depth attachment".into());
            }
        } else if self.clear_depth.is_some() {
            return Err("Depth clear requires an attachment".into());
        }
        if ![self.depth_range.start, self.depth_range.end]
            .iter()
            .all(|value| (0.0..=1.0).contains(value))
            || self
                .clear_depth
                .map_or(false, |value| !(0.0..=1.0).contains(&value))
        {
            return Err("Invalid draw depth range or clear value".into());
        }
        self.bounds()
    }

    fn record_pass(
        &self,
        commands: &mut Recording<'_>,
        record: impl FnOnce(&mut hal::vulkan::CommandEncoder),
    ) -> Result<(), String> {
        let size = self.target.size();
        let target_view = self.target.target_view().unwrap();
        self.target.prepare(commands)?;
        if let Some(depth) = self.depth {
            depth.transition(commands, wgt::TextureUses::DEPTH_WRITE)?;
        }
        let ops = |initialized: bool, clear: bool| {
            (if initialized && !clear {
                hal::AttachmentOps::LOAD
            } else {
                hal::AttachmentOps::LOAD_CLEAR
            }) | hal::AttachmentOps::STORE
        };
        unsafe {
            let encoder = commands.encoder();
            encoder
                .begin_render_pass(&hal::RenderPassDescriptor {
                    label: Some("WR draw pass"),
                    extent: size,
                    sample_count: 1,
                    color_attachments: &[Some(hal::ColorAttachment {
                        target: hal::Attachment {
                            view: target_view,
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        ops: ops(self.target.initialized(), self.clear_color.is_some()),
                        clear_value: self.clear_color.unwrap_or(wgt::Color::TRANSPARENT),
                    })],
                    depth_stencil_attachment: self.depth.map(|depth| hal::DepthStencilAttachment {
                        depth_read_only: false,
                        stencil_read_only: true,
                        target: hal::Attachment {
                            view: Texture::target_view(depth).unwrap(),
                            usage: wgt::TextureUses::DEPTH_WRITE,
                        },
                        depth_ops: ops(depth.initialized(), self.clear_depth.is_some()),
                        stencil_ops: hal::AttachmentOps::LOAD_DONT_CARE
                            | hal::AttachmentOps::STORE_DISCARD,
                        clear_value: (self.clear_depth.unwrap_or(1.0), 0),
                    }),
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .map_err(|error| format!("Beginning draw pass: {error:?}"))?;
            record(encoder);
            encoder.end_render_pass();
        }
        self.target.initialize(commands)?;
        if let Some(depth) = self.depth {
            depth.initialize(commands)?;
        }
        Ok(())
    }
}

#[cfg(all(test, wr_vulkan_shaders))]
#[path = "draw_tests.rs"]
pub(super) mod tests;
