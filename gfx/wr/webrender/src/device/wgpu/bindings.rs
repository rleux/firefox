/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::pipeline::DrawPipeline;
use super::samplers::Samplers;
use super::{core, Failure};
use crate::device::TextureFilter;
use std::{borrow::Cow, sync::Arc};

impl DrawPipeline {
    /// Resources follow the shader's reflected binding order.
    pub fn create_bindings(
        &self,
        projection: Option<Arc<core::resource::Buffer>>,
        textures: &[(Arc<core::resource::TextureView>, TextureFilter)],
        storage: &[Arc<core::resource::Buffer>],
        samplers: &Samplers,
    ) -> Result<Arc<core::binding_model::BindGroup>, Failure> {
        use core::binding_model::{BindGroupEntry, BindingResource, BufferBinding};
        if projection.is_some() != (self.shader.projection_stages != 0)
            || textures.len() != self.shader.textures.len()
            || storage.len() != self.shader.storage_buffers.len()
        {
            return Err(Failure::validation(
                "Draw resources do not match the shader's bindings",
            ));
        }
        let mut entries = Vec::new();
        if let Some(buffer) = projection {
            entries.push(BindGroupEntry {
                binding: 0,
                resource: BindingResource::Buffer(BufferBinding {
                    buffer,
                    offset: 0,
                    size: Some(64),
                }),
            });
        }
        for (binding, buffer) in self.shader.storage_buffers.iter().zip(storage) {
            entries.push(BindGroupEntry {
                binding: binding.binding,
                resource: BindingResource::Buffer(BufferBinding {
                    buffer: buffer.clone(),
                    offset: 0,
                    size: None,
                }),
            });
        }
        for (binding, (view, filter)) in self.shader.textures.iter().zip(textures) {
            entries.push(BindGroupEntry {
                binding: binding.binding,
                resource: BindingResource::TextureView(view.clone()),
            });
            if binding.sampler_stages != 0 {
                entries.push(BindGroupEntry {
                    binding: binding.binding + 1,
                    resource: BindingResource::Sampler(samplers.get(
                        if binding.name.starts_with("sColor") {
                            *filter
                        } else {
                            TextureFilter::Nearest
                        },
                    )),
                });
            }
        }
        let group =
            self.owner
                .device
                .create_bind_group(&core::binding_model::BindGroupDescriptor {
                    label: Some("WR draw bindings".into()),
                    layout: self.prepared.layouts.bindings.clone(),
                    entries: Cow::Owned(entries),
                });
        self.owner.check()?;
        Ok(group)
    }
}
