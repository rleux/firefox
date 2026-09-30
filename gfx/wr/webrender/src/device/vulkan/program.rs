/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::draw::{DrawBatch, DrawPass};
use super::pipeline::DrawPipeline;
use super::{Buffer, Texture, TextureFilter};
use api::units::DeviceIntRect;
use crate::device::TextureSlot;
use euclid::default::Transform3D;
use std::cell::Cell;
use std::rc::Rc;
use webrender_build::vulkan::ShaderArtifact;

pub(super) enum ShaderResource {
    Texture {
        texture: Rc<Texture>,
        filter: Option<TextureFilter>,
    },
    Storage(Rc<Buffer>),
}

pub(super) struct ProgramState {
    shader: &'static ShaderArtifact,
    texture_slots: Vec<usize>,
    storage_slots: Vec<usize>,
    transform: Cell<Transform3D<f32>>,
}

pub(super) struct ResolvedProgram {
    pipeline: Rc<DrawPipeline>,
    projection: [f32; 16],
    textures: Vec<(Rc<Texture>, TextureFilter)>,
    buffers: Vec<Rc<Buffer>>,
}

impl ProgramState {
    pub fn new(shader: &'static ShaderArtifact) -> Self {
        Self {
            shader,
            texture_slots: vec![0; shader.textures.len()],
            storage_slots: vec![0; shader.storage_buffers.len()],
            transform: Cell::new(Transform3D::identity()),
        }
    }

    pub fn bind_samplers(&mut self, bindings: &[(&str, TextureSlot)]) {
        for (name, slot) in bindings {
            for (binding, index) in self.shader.textures.iter().zip(&mut self.texture_slots) {
                if binding.name == *name {
                    *index = slot.0;
                }
            }
            for (binding, index) in self
                .shader
                .storage_buffers
                .iter()
                .zip(&mut self.storage_slots)
            {
                if binding.name == *name {
                    *index = slot.0;
                }
            }
        }
    }

    pub fn set_transform(&self, transform: &Transform3D<f32>) {
        self.transform.set(*transform);
    }

    pub fn resolve(
        &self,
        pipeline: &Rc<DrawPipeline>,
        pass: &DrawPass<'_>,
        slots: &[Option<ShaderResource>],
    ) -> Result<ResolvedProgram, String> {
        if !std::ptr::eq(self.shader, pipeline.shader) {
            return Err("Vulkan pipeline does not match the program's shader variant".into());
        }
        let mut textures = Vec::with_capacity(self.texture_slots.len());
        for (binding, &slot) in self.shader.textures.iter().zip(&self.texture_slots) {
            match slots.get(slot).and_then(Option::as_ref) {
                Some(ShaderResource::Texture { texture, filter }) => {
                    textures.push((texture.clone(), filter.unwrap_or_else(|| texture.filter())));
                }
                _ => {
                    return Err(format!(
                        "Missing Vulkan texture {} in slot {slot}",
                        binding.name
                    ))
                }
            }
        }
        let mut buffers = Vec::with_capacity(self.storage_slots.len());
        for (binding, &slot) in self.shader.storage_buffers.iter().zip(&self.storage_slots) {
            match slots.get(slot).and_then(Option::as_ref) {
                Some(ShaderResource::Storage(buffer)) => buffers.push(buffer.clone()),
                _ => {
                    return Err(format!(
                        "Missing Vulkan storage buffer {} in slot {slot}",
                        binding.name
                    ))
                }
            }
        }
        Ok(ResolvedProgram {
            pipeline: pipeline.clone(),
            projection: pass.projection(&self.transform.get()),
            textures,
            buffers,
        })
    }
}

impl ResolvedProgram {
    pub fn batch<'a>(
        &'a self,
        instances: &'a [u8],
        instance_count: u32,
        scissor: DeviceIntRect,
    ) -> DrawBatch<'a> {
        DrawBatch {
            pipeline: &self.pipeline,
            projection: (self.pipeline.shader.projection_stages != 0).then_some(&self.projection),
            textures: &self.textures,
            buffers: &self.buffers,
            instances,
            instance_count,
            scissor,
        }
    }
}

#[cfg(all(test, wr_vulkan_shaders))]
#[path = "program_tests.rs"]
mod tests;
