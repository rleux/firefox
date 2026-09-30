/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::pipeline::DrawPipeline;
use super::resources::Owned;
use super::{hal, wgt, Buffer, Samplers, Recording, Texture, TextureFilter};
use std::rc::Rc;
use webrender_build::vulkan::ScalarType;
use wgpu_hal::{Adapter as _, Device as _};

pub(super) struct DrawBindings {
    pub raw: Owned<hal::vulkan::BindGroup>,
    projection: Option<Rc<Buffer>>,
    textures: Vec<(Rc<Texture>, TextureFilter)>,
    buffers: Vec<Rc<Buffer>>,
    _samplers: Option<Rc<Samplers>>,
    pub(super) pipeline: Rc<DrawPipeline>,
}

impl DrawBindings {
    pub fn buffer_uses(&self) -> impl Iterator<Item = (&Rc<Buffer>, wgt::BufferUses)> {
        self.projection
            .iter()
            .map(|buffer| (buffer, wgt::BufferUses::UNIFORM))
            .chain(
                self.buffers
                    .iter()
                    .map(|buffer| (buffer, wgt::BufferUses::STORAGE_READ_ONLY)),
            )
    }

    pub fn textures(&self) -> impl Iterator<Item = &Rc<Texture>> {
        self.textures.iter().map(|(texture, _)| texture)
    }

    /// Texture and storage buffer vectors follow the shader's reflected order.
    pub fn new(
        pipeline: &Rc<DrawPipeline>,
        projection: Option<Rc<Buffer>>,
        textures: Vec<(Rc<Texture>, TextureFilter)>,
        buffers: Vec<Rc<Buffer>>,
        samplers: Option<Rc<Samplers>>,
    ) -> Result<Rc<Self>, String> {
        let owner = &pipeline.raw.owner;
        if owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let shader = pipeline.shader;
        if textures.len() != shader.textures.len() || buffers.len() != shader.storage_buffers.len()
        {
            return Err("Draw resources do not match the shader's bindings".into());
        }
        let projection = if shader.projection_stages != 0 {
            let buffer = projection.ok_or("Missing draw projection buffer")?;
            if !Rc::ptr_eq(&buffer.raw.owner, owner)
                || !buffer.usage.contains(wgt::BufferUses::UNIFORM)
                || buffer.binding_size() < 64
                || buffer.binding_size()
                    > u64::from(owner.capabilities.limits.max_uniform_buffer_binding_size)
            {
                return Err("Invalid draw projection buffer".into());
            }
            Some(buffer)
        } else {
            None
        };
        for buffer in &buffers {
            if !Rc::ptr_eq(&buffer.raw.owner, owner)
                || !buffer.usage.contains(wgt::BufferUses::STORAGE_READ_ONLY)
                || buffer.binding_size() < 16
                || buffer.binding_size() % 16 != 0
                || buffer.binding_size() > owner.capabilities.limits.max_storage_buffer_binding_size
            {
                return Err("Invalid draw storage buffer".into());
            }
        }
        let needs_sampler = shader
            .textures
            .iter()
            .any(|binding| binding.sampler_stages != 0);
        let samplers = if needs_sampler {
            let samplers = samplers.ok_or("Missing draw samplers")?;
            if !Rc::ptr_eq(samplers.owner(), owner) {
                return Err("Draw samplers belong to another device".into());
            }
            Some(samplers)
        } else {
            None
        };
        for (binding, (texture, filter)) in shader.textures.iter().zip(&textures) {
            let sample_type = texture.format().sample_type(None, Some(owner.features));
            let compatible = matches!(
                (binding.scalar, sample_type),
                (
                    ScalarType::Float,
                    Some(wgt::TextureSampleType::Float { .. })
                ) | (ScalarType::Sint, Some(wgt::TextureSampleType::Sint))
                    | (ScalarType::Uint, Some(wgt::TextureSampleType::Uint))
            );
            if !Rc::ptr_eq(&texture.raw.owner, owner) || !compatible {
                return Err(format!("Invalid draw texture {}", binding.name));
            }
            if binding.sampler_stages != 0
                && binding.name.starts_with("sColor")
                && *filter != TextureFilter::Nearest
                && !unsafe { owner.adapter.texture_format_capabilities(texture.format()) }
                    .contains(hal::TextureFormatCapabilities::SAMPLED_LINEAR)
            {
                return Err(format!(
                    "Draw texture {} does not support filtering",
                    binding.name
                ));
            }
        }
        let mut entries = Vec::new();
        let mut native_buffers = Vec::new();
        if let Some(buffer) = &projection {
            entries.push(hal::BindGroupEntry {
                binding: 0,
                resource_index: 0,
                count: 1,
            });
            native_buffers.push(buffer.binding());
        }
        for (binding, buffer) in shader.storage_buffers.iter().zip(&buffers) {
            entries.push(hal::BindGroupEntry {
                binding: binding.binding,
                resource_index: native_buffers.len() as u32,
                count: 1,
            });
            native_buffers.push(buffer.binding());
        }
        let mut native_textures = Vec::new();
        let mut native_samplers = Vec::new();
        for (binding, (texture, filter)) in shader.textures.iter().zip(&textures) {
            entries.push(hal::BindGroupEntry {
                binding: binding.binding,
                resource_index: native_textures.len() as u32,
                count: 1,
            });
            native_textures.push(hal::TextureBinding {
                view: texture.view(),
                usage: wgt::TextureUses::RESOURCE,
            });
            if binding.sampler_stages != 0 {
                entries.push(hal::BindGroupEntry {
                    binding: binding.binding + 1,
                    resource_index: native_samplers.len() as u32,
                    count: 1,
                });
                let filter = if binding.name.starts_with("sColor") {
                    *filter
                } else {
                    TextureFilter::Nearest
                };
                native_samplers.push(samplers.as_ref().unwrap().get(filter));
            }
        }
        entries.sort_by_key(|entry| entry.binding);
        let raw = unsafe {
            owner
                .open
                .device
                .create_bind_group(&hal::BindGroupDescriptor {
                    label: Some("WR draw bindings"),
                    layout: &pipeline.bindings,
                    buffers: &native_buffers,
                    textures: &native_textures,
                    samplers: &native_samplers,
                    entries: &entries,
                    acceleration_structures: &[],
                    external_textures: &[],
                })
        }
        .map_err(|error| format!("Creating draw bindings: {error:?}"))?;
        Ok(Rc::new(Self {
            raw: Owned::new(owner, raw, hal::vulkan::Device::destroy_bind_group),
            projection,
            textures,
            buffers,
            _samplers: samplers,
            pipeline: pipeline.clone(),
        }))
    }

    /// Prepare resource reads before beginning the render pass.
    pub fn prepare(self: &Rc<Self>, commands: &mut Recording<'_>) -> Result<(), String> {
        let owner = &self.pipeline.raw.owner;
        commands.recording_id(owner)?;
        if self
            .textures
            .iter()
            .any(|(texture, _)| !texture.sample_initialized())
        {
            return Err("Draw samples an uninitialized texture".into());
        }
        if let Some(buffer) = &self.projection {
            let mut usage = wgt::BufferUses::UNIFORM;
            if self
                .buffers
                .iter()
                .any(|storage| Rc::ptr_eq(storage, buffer))
            {
                usage |= wgt::BufferUses::STORAGE_READ_ONLY;
            }
            buffer.transition(commands, usage)?;
        }
        for buffer in &self.buffers {
            if self
                .projection
                .as_ref()
                .map_or(false, |projection| Rc::ptr_eq(projection, buffer))
            {
                continue;
            }
            buffer.transition(commands, wgt::BufferUses::STORAGE_READ_ONLY)?;
        }
        for (texture, _) in &self.textures {
            texture.transition(commands, wgt::TextureUses::RESOURCE)?;
        }
        commands.keep(self.clone());
        Ok(())
    }
}

#[cfg(all(test, wr_vulkan_shaders))]
#[path = "binding_tests.rs"]
mod tests;
