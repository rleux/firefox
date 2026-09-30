/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::bindings::DrawBindings;
use super::pipeline::DrawPipeline;
use super::{hal, wgt, Buffer, Device, Recording, Samplers, SubmissionQueue, Texture, TextureFilter};
use api::units::{DeviceIntPoint, DeviceIntRect};
use euclid::default::Transform3D;
use crate::internal_types::FastHashMap;
use std::ops::Range;
use std::rc::Rc;

#[cfg(test)]
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

#[derive(Clone)]
pub(super) struct Draw {
    pub bindings: DrawBindings,
    pub instances: Rc<Buffer>,
    pub instance_offset: u64,
    pub instance_count: u32,
    pub scissor: DeviceIntRect,
}

pub(super) enum PassCommand {
    Draw(Draw),
    Clear(Rc<super::clear::Clear>),
}

enum PreparedCommand<'a> {
    Draw(&'a Draw, DeviceIntRect, hal::BufferBinding<'a, dyn hal::DynBuffer, wgt::BufferAddress>),
    Clear(&'a Rc<super::clear::Clear>),
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
    fn target_view(&self) -> Option<&dyn hal::DynTextureView>;
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
    fn target_view(&self) -> Option<&dyn hal::DynTextureView> {
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

pub(super) struct ValidatedPass<'p, 'a, T: ColorAttachment> {
    pass: &'p DrawPass<'a, T>,
    pub bounds: DeviceIntRect,
    color_view: &'p dyn hal::DynTextureView,
    depth_view: Option<&'p dyn hal::DynTextureView>,
}

impl<'a, T: ColorAttachment> DrawPass<'a, T> {
    #[cfg(test)]
    pub fn clear_rect(
        &self,
        commands: &mut Recording<'_>,
        rect: DeviceIntRect,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
    ) -> Result<(), String> {
        let validated = self.validate(commands)?;
        let full = validated.bounds;
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
        let clear = commands.clear(
            self.target.owner(), self.target.format(), self.depth.is_some(),
            rect, color, depth,
        )?;
        validated.record(commands, None, |encoder| unsafe {
            clear.record(encoder, self.target.size(), self.origin);
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
            matrix[column + 1] -= dy * w;
            if self.target.owner().flip_y {
                matrix[column + 1] = -matrix[column + 1];
            }
            if self.target.owner().depth_zero_to_one {
                matrix[column + 2] = (matrix[column + 2] + w) * 0.5;
            }
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
        let validated = self.validate(commands)?;
        let full = validated.bounds;
        let active = batches
            .iter()
            .filter(|batch| {
                batch.instance_count != 0 && batch.scissor.intersection(&full).is_some()
            });
        for batch in active.clone() {
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
        let mut write_batches = active.clone();
        let instances = uploads.upload_instances_iter(commands,
            active.clone().map(|batch| batch.instances.len()), |_, destination| {
                destination.copy_from_slice(write_batches.next().unwrap().instances);
                Ok(())
            })?;
        commands.with_binding_cache(|commands, bindings| {
            let mut operations = Vec::new();
            for (index, batch) in active.enumerate() {
                let bindings = bindings.resolve(commands, uploads, batch.pipeline, batch.projection,
                    batch.textures, batch.buffers, samplers)?;
                let (buffer, range) = instances.buffer_range(index)?;
                operations.push(PassCommand::Draw(Draw {
                    bindings,
                    instances: buffer.clone(),
                    instance_offset: range.start,
                    instance_count: batch.instance_count,
                    scissor: batch.scissor,
                }));
            }
            validated.record_commands(commands, quad, &operations, None)
        })
    }

    #[cfg(test)]
    pub fn record(
        &self,
        commands: &mut Recording<'_>,
        quad: &Rc<Buffer>,
        draws: &[Draw],
    ) -> Result<(), String> {
        let operations: Vec<_> = draws.iter().cloned().map(PassCommand::Draw).collect();
        self.record_commands(commands, quad, &operations, None)
    }

    pub(super) fn record_commands(
        &self,
        commands: &mut Recording<'_>,
        quad: &Rc<Buffer>,
        operations: &[PassCommand],
        attachment_ops: Option<(hal::AttachmentOps, hal::AttachmentOps)>,
    ) -> Result<(), String> {
        self.validate(commands)?.record_commands(commands, quad, operations, attachment_ops)
    }

    pub(super) fn validate(&self, commands: &Recording<'_>) -> Result<ValidatedPass<'_, 'a, T>, String> {
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
        let color_view = self.target.target_view().ok_or("Draw target is not renderable")?;
        if !matches!(
            self.target.format().sample_type(None, Some(owner.features)),
            Some(wgt::TextureSampleType::Float { .. })
        ) {
            return Err("Invalid draw color attachment".into());
        }
        let depth_view = if let Some(depth) = self.depth {
            if !Rc::ptr_eq(&depth.raw.owner, owner)
                || depth.format() != wgt::TextureFormat::Depth32Float
                || depth.size() != size
            {
                return Err("Invalid draw depth attachment".into());
            }
            Some(Texture::target_view(depth).ok_or("Invalid draw depth attachment")?)
        } else {
            if self.clear_depth.is_some() {
                return Err("Depth clear requires an attachment".into());
            }
            None
        };
        if ![self.depth_range.start, self.depth_range.end]
            .iter()
            .all(|value| (0.0..=1.0).contains(value))
            || self
                .clear_depth
                .map_or(false, |value| !(0.0..=1.0).contains(&value))
        {
            return Err("Invalid draw depth range or clear value".into());
        }
        Ok(ValidatedPass {
            pass: self,
            bounds: self.bounds()?,
            color_view,
            depth_view,
        })
    }
}

impl<T: ColorAttachment> ValidatedPass<'_, '_, T> {
    fn record_commands(
        &self,
        commands: &mut Recording<'_>,
        quad: &Rc<Buffer>,
        operations: &[PassCommand],
        attachment_ops: Option<(hal::AttachmentOps, hal::AttachmentOps)>,
    ) -> Result<(), String> {
        let pass = self.pass;
        let full = self.bounds;
        let owner = pass.target.owner();
        let size = pass.target.size();
        let viewport = pass.viewport.unwrap_or_else(|| {
            DeviceIntRect::from_size(api::units::DeviceIntSize::new(
                size.width as i32, size.height as i32,
            ))
        });
        let mut buffers: FastHashMap<*const Buffer, (&Rc<Buffer>, wgt::BufferUses)> = FastHashMap::default();
        fn add_buffer<'a>(
            buffers: &mut FastHashMap<*const Buffer, (&'a Rc<Buffer>, wgt::BufferUses)>,
            buffer: &'a Rc<Buffer>,
            usage: wgt::BufferUses,
        ) {
            buffers
                .entry(Rc::as_ptr(buffer))
                .and_modify(|entry| entry.1 |= usage)
                .or_insert((buffer, usage));
        }
        let mut textures = FastHashMap::default();
        let mut active = Vec::new();
        for operation in operations {
            let draw = match operation {
                PassCommand::Draw(draw) => draw,
                PassCommand::Clear(clear) => {
                    clear.prepare(commands)?;
                    active.push(PreparedCommand::Clear(clear));
                    continue;
                }
            };
            let Some(scissor) = draw.scissor.intersection(&full) else {
                continue;
            };
            if draw.instance_count == 0 {
                continue;
            }
            let pipeline = &draw.bindings.pipeline;
            if !Rc::ptr_eq(&pipeline.raw.owner, owner)
                || pipeline.format != pass.target.format()
                || pipeline.has_depth != pass.depth.is_some()
                || !Rc::ptr_eq(&draw.instances.raw.owner, owner)
            {
                return Err("Draw resources do not match the render pass".into());
            }
            let bytes = pipeline
                .instance_stride
                .checked_mul(u64::from(draw.instance_count))
                .ok_or("Draw instance range overflow")?;
            let instances = draw.instances.vertex_binding(draw.instance_offset, bytes)?;
            for texture in draw.bindings.resources.textures() {
                let std::collections::hash_map::Entry::Vacant(entry) = textures.entry(Rc::as_ptr(texture)) else {
                    continue;
                };
                if pass.target.is_sampled_by(texture)
                    || pass
                        .depth
                        .map_or(false, |depth| texture.samples_attachment(depth))
                {
                    return Err("Draw attachment feedback is unsupported".into());
                }
                if !texture.sample_initialized() {
                    return Err("Draw samples an uninitialized texture".into());
                }
                entry.insert(texture.sampled()?);
            }
            add_buffer(&mut buffers, &draw.instances, wgt::BufferUses::VERTEX);
            for (buffer, usage) in draw.bindings.resources.buffer_uses() {
                add_buffer(&mut buffers, buffer, usage);
            }
            active.push(PreparedCommand::Draw(draw, scissor, instances));
        }
        let has_draws = active.iter().any(|command| matches!(command, PreparedCommand::Draw(..)));
        if has_draws {
            if !Rc::ptr_eq(&quad.raw.owner, owner) {
                return Err("Draw vertices belong to another device".into());
            }
            quad.vertex_binding(0, 16)?;
            add_buffer(&mut buffers, quad, wgt::BufferUses::VERTEX);
        }
        for (_, (buffer, usage)) in buffers {
            buffer.transition(commands, usage)?;
        }
        for texture in textures.values() {
            #[cfg(test)]
            owner.trace.borrow_mut().push(super::tests::Command::PrepareSampledView);
            texture.prepare(commands)?;
        }
        for operation in &active {
            let PreparedCommand::Draw(draw, _, _) = operation else { continue };
            commands.keep(&draw.bindings.resources);
            commands.keep(&draw.bindings.pipeline);
        }
        self.record(commands, attachment_ops, |encoder| unsafe {
            let mut bound = None;
            let mut draw_state = None;
            for operation in active {
                let (draw, scissor, instances) = match operation {
                    PreparedCommand::Clear(clear) => {
                        clear.record(encoder, size, pass.origin);
                        bound = None;
                        draw_state = None;
                        continue;
                    }
                    PreparedCommand::Draw(draw, scissor, instances) => (draw, scissor, instances),
                };
                let pipeline = &draw.bindings.pipeline;
                let instance_binding = (Rc::as_ptr(&draw.instances), instances.offset, instances.size);
                let state = (Rc::as_ptr(pipeline), scissor, instance_binding);
                if draw_state.is_none() {
                    encoder.set_viewport(
                        &hal::Rect {
                            x: viewport.min.x as f32,
                            y: viewport.min.y as f32,
                            w: viewport.width() as f32,
                            h: viewport.height() as f32,
                        },
                        pass.depth_range.clone(),
                    );
                    encoder.set_vertex_buffer(0, quad.binding());
                    #[cfg(test)]
                    owner.trace.borrow_mut().push(super::tests::Command::DrawInvariant);
                }
                if draw_state.map_or(true, |previous: (_, _, _)| previous.0 != state.0) {
                    encoder.set_render_pipeline(&*pipeline.raw);
                    #[cfg(test)]
                    owner.trace.borrow_mut().push(super::tests::Command::DrawPipeline);
                }
                if draw_state.map_or(true, |previous| previous.1 != scissor) {
                    encoder.set_scissor_rect(&hal::Rect {
                        x: (scissor.min.x - pass.origin.x) as u32,
                        y: (scissor.min.y - pass.origin.y) as u32,
                        w: scissor.width() as u32,
                        h: scissor.height() as u32,
                    });
                    #[cfg(test)]
                    owner.trace.borrow_mut().push(super::tests::Command::DrawScissor);
                }
                if draw_state.map_or(true, |previous| previous.2 != instance_binding) {
                    encoder.set_vertex_buffer(1, instances);
                    #[cfg(test)]
                    owner.trace.borrow_mut().push(super::tests::Command::DrawInstances);
                }
                draw_state = Some(state);
                let binding = (Rc::as_ptr(&pipeline.layouts), Rc::as_ptr(&draw.bindings.resources), draw.bindings.projection_offset);
                if bound != Some(binding) {
                    encoder.set_bind_group(&*pipeline.layouts.layout, 0, &*draw.bindings.resources.raw, draw.bindings.projection_offset.as_slice());
                    bound = Some(binding);
                }
                encoder.draw(0, 4, 0, draw.instance_count);
            }
        })
    }


    fn record(
        &self,
        commands: &mut Recording<'_>,
        attachment_ops: Option<(hal::AttachmentOps, hal::AttachmentOps)>,
        record: impl FnOnce(&mut dyn hal::DynCommandEncoder),
    ) -> Result<(), String> {
        let pass = self.pass;
        let size = pass.target.size();
        pass.target.prepare(commands)?;
        if let Some(depth) = pass.depth {
            depth.transition(commands, wgt::TextureUses::DEPTH_WRITE)?;
        }
        let ops = |initialized: bool, clear: bool| {
            (if initialized && !clear {
                hal::AttachmentOps::LOAD
            } else {
                hal::AttachmentOps::LOAD_CLEAR
            }) | hal::AttachmentOps::STORE
        };
        let color_ops = attachment_ops.map_or_else(|| ops(pass.target.initialized(), pass.clear_color.is_some()), |ops| ops.0);
        let depth_ops = attachment_ops.map_or_else(|| ops(pass.depth.map_or(false, |depth| depth.initialized()), pass.clear_depth.is_some()), |ops| ops.1);
        unsafe {
            let encoder = commands.encoder();
            encoder
                .begin_render_pass(&hal::RenderPassDescriptor {
                    label: Some("WR draw pass"),
                    extent: size,
                    sample_count: 1,
                    color_attachments: &[Some(hal::ColorAttachment {
                        target: hal::Attachment {
                            view: self.color_view,
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        ops: color_ops,
                        clear_value: pass.clear_color.unwrap_or(wgt::Color::TRANSPARENT),
                    })],
                    depth_stencil_attachment: self.depth_view.map(|view| hal::DepthStencilAttachment {
                        depth_read_only: false,
                        stencil_read_only: true,
                        target: hal::Attachment {
                            view,
                            usage: wgt::TextureUses::DEPTH_WRITE,
                        },
                        depth_ops,
                        stencil_ops: hal::AttachmentOps::LOAD_DONT_CARE
                            | hal::AttachmentOps::STORE_DISCARD,
                        clear_value: (pass.clear_depth.unwrap_or(1.0), 0),
                    }),
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .map_err(|error| format!("Beginning draw pass: {error:?}"))?;
            #[cfg(test)]
            pass.target.owner().trace.borrow_mut().push(super::tests::Command::BeginPass(color_ops, depth_ops));
            record(encoder);
            encoder.end_render_pass();
            #[cfg(test)]
            pass.target.owner().trace.borrow_mut().push(super::tests::Command::EndPass);
        }
        pass.target.initialize(commands)?;
        if let Some(depth) = pass.depth {
            depth.initialize(commands)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "draw_tests.rs"]
pub(super) mod tests;
