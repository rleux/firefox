/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::internal_types::FastHashMap;
use super::bindings::{BindingResources, DrawBindings};
use smallvec::SmallVec;
use super::pipeline::DrawPipeline;
use super::shader::ShaderLayouts;
use super::{wgt, Buffer, Device, Recording, Samplers, SubmissionQueue, Texture, TextureFilter};
use std::{rc::{Rc, Weak}};

#[derive(Eq, Hash, PartialEq)]
struct BindingKey {
    layout: *const ShaderLayouts,
    projection: *const Buffer,
    textures: SmallVec<[(*const Texture, u8); 16]>,
    buffers: SmallVec<[(*const Buffer, u64); 4]>,
    samplers: *const Samplers,
}

struct UniformArena {
    buffer: Rc<Buffer>,
    recording: Weak<()>,
    next: u32,
    values: FastHashMap<[u32; 16], u32>,
}

#[derive(Default)]
pub(super) struct BindingCache {
    bindings: FastHashMap<BindingKey, Rc<BindingResources>>,
    previous_bindings: FastHashMap<BindingKey, Rc<BindingResources>>,
    uniforms: Option<UniformArena>,
    pub(super) clears: super::clear::ClearCache,
}

impl BindingCache {
    pub fn clear(&mut self) {
        self.bindings.clear();
        self.previous_bindings.clear();
        self.uniforms = None;
        self.clears.clear();
    }

    pub(super) fn uniform(
        &mut self,
        commands: &mut Recording<'_>,
        owner: &Rc<Device>,
        values: [u32; 16],
        allocate: impl FnOnce(&mut Recording<'_>, usize) -> Result<Rc<Buffer>, String>,
    ) -> Result<(Rc<Buffer>, u32), String> {
        let recording = commands.recording_id(owner)?;
        let alignment = owner.capabilities.limits.min_uniform_buffer_offset_alignment.max(64);
        let fresh = self.uniforms.as_ref().map_or(true, |arena| {
            arena.recording.upgrade().as_ref().map_or(true, |active| !Rc::ptr_eq(active, &recording))
                || (!arena.values.contains_key(&values) && u64::from(arena.next) + 64 > arena.buffer.size())
        });
        if fresh {
            let capacity = owner.capabilities.limits.max_buffer_size.min(65536) as usize;
            if capacity < 64 { return Err("Uniform buffer capacity is too small".into()); }
            let buffer = allocate(commands, capacity)?;
            commands.flush_uniform_before_submit(&buffer);
            #[cfg(test)]
            owner.trace.borrow_mut().push(super::tests::Command::UniformArena(capacity));
            self.uniforms = Some(UniformArena { buffer, recording: Rc::downgrade(&recording), next: 0, values: FastHashMap::default() });
        }
        let arena = self.uniforms.as_mut().unwrap();
        let offset = if let Some(offset) = arena.values.get(&values) {
            *offset
        } else {
            let offset = arena.next;
            let mut bytes = [0; 64];
            for (destination, value) in bytes.chunks_exact_mut(4).zip(values) {
                destination.copy_from_slice(&value.to_ne_bytes());
            }
            // Each slice is written once, before its recording can be submitted.
            unsafe { arena.buffer.write_unsubmitted_range(u64::from(offset), &bytes)?; }
            arena.next = offset.checked_add(alignment).ok_or("Uniform arena offset overflow")?;
            arena.values.insert(values, offset);
            offset
        };
        Ok((arena.buffer.clone(), offset))
    }

    pub fn resolve(
        &mut self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        pipeline: &Rc<DrawPipeline>,
        projection: Option<&[f32; 16]>,
        textures: &[(Rc<Texture>, TextureFilter)],
        buffers: &[Rc<Buffer>],
        samplers: Option<&Rc<Samplers>>,
    ) -> Result<DrawBindings, String> {
        let (projection, offset) = if pipeline.shader.projection_stages != 0 {
            let values = projection.ok_or("Missing draw projection")?.map(f32::to_bits);
            let owner = &pipeline.raw.owner;
            let (buffer, offset) = self.uniform(commands, owner, values, |commands, capacity| {
                uploads.upload_in_recording(commands, capacity, wgt::BufferUses::UNIFORM, |_| Ok(()))
            })?;
            (Some(buffer), offset)
        } else {
            (None, 0)
        };
        let key = BindingKey {
            layout: Rc::as_ptr(&pipeline.layouts),
            projection: projection.as_ref().map_or(std::ptr::null(), Rc::as_ptr),
            textures: textures.iter().map(|(texture, filter)| (Rc::as_ptr(texture), match filter {
                TextureFilter::Nearest => 0, TextureFilter::Linear => 1, TextureFilter::Trilinear => 2,
            })).collect(),
            buffers: buffers.iter().map(|buffer| (Rc::as_ptr(buffer), buffer.binding_size())).collect(),
            samplers: samplers.map_or(std::ptr::null(), Rc::as_ptr),
        };
        let binding = if let Some(binding) = self.bindings.get(&key) {
            binding.clone()
        } else {
            let binding = match self.previous_bindings.remove(&key) {
                Some(binding) => binding,
                None => BindingResources::new(pipeline, projection, textures.to_vec(), buffers.to_vec(), samplers.cloned())?,
            };
            if self.bindings.len() >= 256 {
                std::mem::swap(&mut self.bindings, &mut self.previous_bindings);
                self.bindings.clear();
            }
            self.bindings.insert(key, binding.clone());
            binding
        };
        Ok(binding.for_draw(pipeline, offset))
    }
}

#[cfg(test)]
#[path = "binding_cache_tests.rs"]
mod tests;
