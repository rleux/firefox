/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::bindings::DrawBindings;
use super::draw::DrawPass;
#[cfg(test)]
use super::draw::{upload_projection, DrawBatch};
use super::pipeline::DrawPipeline;
use super::{Buffer, Recording, Samplers, SubmissionQueue, Texture, TextureFilter};
#[cfg(test)]
use api::units::DeviceIntRect;
use crate::device::TextureSlot;
use euclid::default::Transform3D;
use smallvec::SmallVec;
use std::cell::Cell;
use std::rc::Rc;
use webrender_build::vulkan::{ScalarType, ShaderArtifact};

#[derive(Clone)]
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
    textures: SmallVec<[(Rc<Texture>, TextureFilter); 16]>,
    buffers: SmallVec<[Rc<Buffer>; 4]>,
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

    pub fn shader(&self) -> &'static ShaderArtifact {
        self.shader
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
        mut slot_resource: impl FnMut(usize) -> Option<Option<ShaderResource>>,
        fallback: Option<&Rc<Texture>>,
    ) -> Result<ResolvedProgram, String> {
        if !std::ptr::eq(self.shader, pipeline.shader) {
            return Err("Vulkan pipeline does not match the program's shader variant".into());
        }
        let mut textures = SmallVec::new();
        for (binding, &slot) in self.shader.textures.iter().zip(&self.texture_slots) {
            match (slot_resource(slot), fallback) {
                (Some(Some(ShaderResource::Texture { texture, filter })), _) => {
                    let filter = filter.unwrap_or_else(|| texture.filter());
                    textures.push((texture, filter));
                }
                (Some(None), Some(texture))
                    if binding.scalar == ScalarType::Float
                        && matches!(
                            binding.name,
                            "sColor0" | "sColor1" | "sColor2" | "sClipMask"
                        ) =>
                {
                    textures.push((texture.clone(), texture.filter()));
                }
                _ => {
                    return Err(format!(
                        "Missing Vulkan texture {} in slot {slot}",
                        binding.name
                    ))
                }
            }
        }
        let mut buffers = SmallVec::new();
        for (binding, &slot) in self.shader.storage_buffers.iter().zip(&self.storage_slots) {
            match slot_resource(slot).flatten() {
                Some(ShaderResource::Storage(buffer)) => buffers.push(buffer),
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
    pub fn cached_bindings(
        &self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        samplers: Option<&Rc<Samplers>>,
    ) -> Result<DrawBindings, String> {
        commands.with_binding_cache(|commands, cache| {
            cache.resolve(commands, uploads, &self.pipeline, Some(&self.projection), &self.textures, &self.buffers, samplers)
        })
    }

    #[cfg(test)]
    pub fn bindings(
        &self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        samplers: Option<&Rc<Samplers>>,
    ) -> Result<DrawBindings, String> {
        let projection = if self.pipeline.shader.projection_stages != 0 {
            Some(upload_projection(commands, uploads, &self.projection)?)
        } else {
            None
        };
        DrawBindings::new(
            &self.pipeline,
            projection,
            self.textures.to_vec(),
            self.buffers.to_vec(),
            samplers.cloned(),
        )
    }

    #[cfg(test)]
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

#[cfg(test)]
#[path = "program_tests.rs"]
mod tests;
