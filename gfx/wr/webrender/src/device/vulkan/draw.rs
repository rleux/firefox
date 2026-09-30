/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::bindings::DrawBindings;
use super::pipeline::DrawPipeline;
use super::{hal, wgt, Buffer, Recording, Samplers, SubmissionQueue, Texture, TextureFilter};
use api::units::{DeviceIntRect, DeviceIntSize};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use wgpu_hal::CommandEncoder as _;

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

pub(super) struct DrawPass<'a> {
    pub target: &'a Rc<Texture>,
    pub depth: Option<&'a Rc<Texture>>,
    pub clear_color: Option<wgt::Color>,
    pub clear_depth: Option<f32>,
    pub depth_range: Range<f32>,
}

impl DrawPass<'_> {
    pub fn record_batches(
        &self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        quad: &Rc<Buffer>,
        samplers: Option<&Rc<Samplers>>,
        batches: &[DrawBatch<'_>],
    ) -> Result<(), String> {
        let owner = &self.target.raw.owner;
        commands.recording_id(owner)?;
        let size = self.target.size();
        let full =
            DeviceIntRect::from_size(DeviceIntSize::new(size.width as i32, size.height as i32));
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
                Some(uploads.upload_in_recording(
                    commands,
                    64,
                    wgt::BufferUses::UNIFORM,
                    |bytes| {
                        for (destination, value) in bytes.chunks_exact_mut(4).zip(matrix) {
                            destination.copy_from_slice(&value.to_ne_bytes());
                        }
                        Ok(())
                    },
                )?)
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
        let owner = &self.target.raw.owner;
        commands.recording_id(owner)?;
        let size = self.target.size();
        let target_view = self
            .target
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
        let full =
            DeviceIntRect::from_size(DeviceIntSize::new(size.width as i32, size.height as i32));
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
                if texture.samples_attachment(self.target)
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
        self.target
            .transition(commands, wgt::TextureUses::COLOR_TARGET)?;
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
                            view: depth.target_view().unwrap(),
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
            encoder.set_viewport(
                &hal::Rect {
                    x: 0.0,
                    y: 0.0,
                    w: size.width as f32,
                    h: size.height as f32,
                },
                self.depth_range.clone(),
            );
            if !active.is_empty() {
                encoder.set_vertex_buffer(0, quad.binding());
            }
            for (draw, scissor, instances) in active {
                let pipeline = &draw.bindings.pipeline;
                encoder.set_scissor_rect(&hal::Rect {
                    x: scissor.min.x as u32,
                    y: scissor.min.y as u32,
                    w: scissor.width() as u32,
                    h: scissor.height() as u32,
                });
                encoder.set_render_pipeline(&pipeline.raw);
                encoder.set_bind_group(&pipeline.layout, 0, &draw.bindings.raw, &[]);
                encoder.set_vertex_buffer(1, instances);
                encoder.draw(0, 4, 0, draw.instance_count);
            }
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
