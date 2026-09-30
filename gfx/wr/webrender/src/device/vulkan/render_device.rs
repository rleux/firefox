/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::program_store::ProgramStore;
use super::render_pass::RenderPassState;
use super::texture_store::TextureStore;
use super::texture_blit::{TextureBlit, TextureBlitter};
use super::vertex_array::VertexArrayStore;
use super::upload_buffers::UploadBuffers;
use super::{
    wgt, Buffer, BufferPool, Device, Samplers, SubmissionQueue, Texture, TextureFilter, TexturePool,
};
use api::ImageFormat;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize, FramebufferIntRect};
use crate::device::{
    DrawTarget, ReadTarget, GpuFrameId, Program, RenderPassDescriptor, RenderState, StoreOp,
    Texture as TextureHandle, TransferBuffer, UploadBufferMapping, UploadChunk,
};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::rc::Rc;

pub(super) struct RenderDevice {
    pub programs: ProgramStore,
    pub textures: TextureStore,
    pub vertex_arrays: VertexArrayStore,
    pub uploads: UploadBuffers,
    pub passes: RenderPassState,
    pub submissions: SubmissionQueue,
    quad: Rc<Buffer>,
    samplers: Rc<Samplers>,
    fallback: Rc<Texture>,
    frame: GpuFrameId,
    inside_frame: bool,
    blitter: Option<(wgt::TextureFormat, TextureBlitter)>,
    scratch: TexturePool,
}

impl RenderDevice {
    pub fn new(owner: &Rc<Device>) -> Result<Self, String> {
        let pool = Rc::new(BufferPool::new(owner));
        let submissions = SubmissionQueue::new(&pool, 2)?;
        let quad = Buffer::new(
            owner,
            &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
            wgt::BufferUses::VERTEX,
        )?;
        let samplers = Rc::new(Samplers::new(owner)?);
        let fallback = Texture::new(
            owner,
            1,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )?;
        fallback.upload(
            &submissions,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[255; 4],
            None,
            0,
            None,
        )?;
        Ok(Self {
            programs: ProgramStore::default(),
            textures: TextureStore::new(owner),
            vertex_arrays: VertexArrayStore::new(&pool),
            uploads: UploadBuffers::new(&pool),
            passes: RenderPassState::default(),
            submissions,
            quad,
            samplers,
            fallback,
            frame: GpuFrameId::new(0),
            inside_frame: false,
            blitter: None,
            scratch: TexturePool::new(owner),
        })
    }

    pub fn upload_texture_region(
        &mut self,
        texture: &TextureHandle,
        rect: DeviceIntRect,
        stride: Option<i32>,
        format: Option<ImageFormat>,
        data: &[u8],
    ) -> Result<(), String> {
        let image = self.textures.image(texture)?;
        image.upload(&self.submissions, rect, data, stride, 0, format)?;
        self.update_mipmaps(&image)
    }

    pub fn upload_texture_immediate(
        &mut self,
        texture: &TextureHandle,
        data: &[u8],
    ) -> Result<(), String> {
        self.upload_texture_region(
            texture,
            DeviceIntRect::from_size(texture.get_dimensions()),
            None,
            None,
            data,
        )
    }

    pub fn flush_upload_buffer(
        &mut self,
        buffer: &TransferBuffer,
        mapping: &UploadBufferMapping,
        size_used: usize,
        chunks: &[UploadChunk<'_>],
    ) -> Result<(), String> {
        self.uploads.flush(
            buffer,
            mapping,
            size_used,
            chunks,
            &self.textures,
            &self.submissions,
        )?;
        let mut updated = HashSet::new();
        for chunk in chunks {
            if chunk.texture.get_filter() == TextureFilter::Trilinear
                && updated.insert(chunk.texture.id)
            {
                let image = self.textures.image(chunk.texture)?;
                self.update_mipmaps(&image)?;
            }
        }
        Ok(())
    }

    fn update_mipmaps(&mut self, texture: &Rc<Texture>) -> Result<(), String> {
        if texture.mip_count() == 1 {
            return Ok(());
        }
        self.prepare_blitter(texture)?;
        self.blitter.as_ref().unwrap().1.generate_mipmaps(
            &mut self.submissions.recording()?,
            &self.submissions,
            &mut self.scratch,
            texture,
        )
    }

    fn prepare_blitter(&mut self, target: &Rc<Texture>) -> Result<(), String> {
        let format = target.format();
        if self.blitter.as_ref().map(|entry| entry.0) != Some(format) {
            let blitter =
                TextureBlitter::new(&target.raw.owner, format, &self.quad, &self.samplers)?;
            self.blitter = Some((format, blitter));
        }
        Ok(())
    }

    pub fn copy_texture_sub_region(
        &mut self,
        source: &TextureHandle,
        x: usize,
        y: usize,
        target: &TextureHandle,
        dx: usize,
        dy: usize,
        width: usize,
        height: usize,
    ) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let rect = |x: usize, y: usize| -> Result<DeviceIntRect, String> {
            let point = |x, y| -> Result<DeviceIntPoint, String> {
                Ok(DeviceIntPoint::new(
                    i32::try_from(x).map_err(|_| "Vulkan copy X exceeds coordinate range")?,
                    i32::try_from(y).map_err(|_| "Vulkan copy Y exceeds coordinate range")?,
                ))
            };
            Ok(DeviceIntRect::new(
                point(x, y)?,
                point(
                    x.checked_add(width).ok_or("Vulkan copy X overflow")?,
                    y.checked_add(height).ok_or("Vulkan copy Y overflow")?,
                )?,
            ))
        };
        let source_rect = rect(x, y)?;
        let target_rect = rect(dx, dy)?;
        let source = self.textures.image(source)?;
        let target = self.textures.image(target)?;
        target.copy_from_texture(
            &mut self.submissions.recording()?,
            &source,
            source_rect,
            target_rect,
        )
    }

    pub fn blit_render_target(
        &mut self,
        source: ReadTarget,
        source_rect: FramebufferIntRect,
        target: DrawTarget,
        target_rect: FramebufferIntRect,
        filter: TextureFilter,
    ) -> Result<(), String> {
        let source = self.textures.read_target(source)?;
        let (target, _, _) = self.textures.draw_target(target)?;
        self.prepare_blitter(&target)?;
        self.blitter.as_ref().unwrap().1.record(
            &mut self.submissions.recording()?,
            &self.submissions,
            &mut self.scratch,
            TextureBlit {
                source: &source,
                target: &target,
                source_rect: source_rect.cast_unit(),
                target_rect: target_rect.cast_unit(),
                filter,
            },
        )
    }

    pub fn invalidate_render_target(&mut self, texture: &TextureHandle) -> Result<(), String> {
        self.textures
            .invalidate_render_target(texture, &mut self.submissions.recording()?)
    }

    pub fn begin_frame(&mut self) -> Result<GpuFrameId, String> {
        if self.inside_frame {
            return Err("A Vulkan frame is already active".into());
        }
        let frame = GpuFrameId::new(
            self.frame
                .0
                .checked_add(1)
                .ok_or("Vulkan frame identifier overflow")?,
        );
        self.submissions.poll()?;
        self.textures.begin_frame(frame);
        self.programs.unbind();
        self.vertex_arrays.unbind();
        self.frame = frame;
        self.inside_frame = true;
        Ok(frame)
    }

    pub fn end_frame(&mut self) -> Result<u64, String> {
        if !self.inside_frame || self.passes.is_active() {
            return Err("Vulkan frame must be active with no unfinished render pass".into());
        }
        let serial = self.submissions.submit()?;
        self.inside_frame = false;
        Ok(serial)
    }

    pub fn begin_render_pass(&mut self, descriptor: &RenderPassDescriptor) -> Result<(), String> {
        if !self.inside_frame {
            return Err("Vulkan render pass requires an active frame".into());
        }
        self.passes.begin(
            &mut self.submissions.recording()?,
            &mut self.textures,
            descriptor,
        )
    }

    pub fn bind_pipeline(&mut self, program: &Program, state: RenderState) -> Result<bool, String> {
        self.programs
            .bind_pipeline(program, state, &self.passes.draw_pass()?)
    }

    pub fn draw_instanced(&self, base: u32, count: u32) -> Result<(), String> {
        let pass = self.passes.draw_pass()?;
        let Some(instances) = self.vertex_arrays.instances(base, count)? else {
            return Ok(());
        };
        let scissor = self.passes.scissor_rect()?;
        if scissor.is_empty() {
            return Ok(());
        }
        let program = self.programs.resolve_current(
            &pass,
            &self.textures.bindings(),
            Some(&self.fallback),
        )?;
        let mut commands = self.submissions.recording()?;
        let bindings = program.bindings(&mut commands, &self.submissions, Some(&self.samplers))?;
        let draw = instances.draw(bindings, scissor)?;
        pass.record(&mut commands, &self.quad, &[draw])
    }

    pub fn clear_target(
        &self,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
        rect: Option<FramebufferIntRect>,
    ) -> Result<(), String> {
        self.passes
            .clear(&mut self.submissions.recording()?, color, depth, rect)
    }

    pub fn end_render_pass(&mut self, depth_store: StoreOp) -> Result<(), String> {
        self.passes
            .end(&mut self.submissions.recording()?, depth_store)
    }
}

#[cfg(test)]
#[path = "render_device_tests.rs"]
mod tests;
