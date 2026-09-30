/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::program_store::ProgramStore;
use super::render_pass::RenderPassState;
use super::texture_store::TextureStore;
use super::vertex_array::VertexArrayStore;
use super::{wgt, Buffer, BufferPool, Device, Samplers, SubmissionQueue, Texture, TextureFilter};
use api::units::{DeviceIntRect, DeviceIntSize, FramebufferIntRect};
use crate::device::{Program, RenderPassDescriptor, RenderState, StoreOp};
use std::rc::Rc;

pub(super) struct RenderDevice {
    pub programs: ProgramStore,
    pub textures: TextureStore,
    pub vertex_arrays: VertexArrayStore,
    pub passes: RenderPassState,
    pub submissions: SubmissionQueue,
    quad: Rc<Buffer>,
    samplers: Rc<Samplers>,
    fallback: Rc<Texture>,
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
            passes: RenderPassState::default(),
            submissions,
            quad,
            samplers,
            fallback,
        })
    }

    pub fn begin_render_pass(&mut self, descriptor: &RenderPassDescriptor) -> Result<(), String> {
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
