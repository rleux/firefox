/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, wgt, Device, TextureFilter};
use std::rc::Rc;
use wgpu_hal::Device as _;

pub struct Samplers {
    samplers: [Owned<hal::vulkan::Sampler>; 3],
}

impl Samplers {
    pub fn new(owner: &Rc<Device>) -> Result<Self, String> {
        if owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let create = |filter, mipmap| {
            let raw = unsafe {
                owner.open.device.create_sampler(&hal::SamplerDescriptor {
                    label: Some("WR Vulkan sampler"),
                    address_modes: [wgt::AddressMode::ClampToEdge; 3],
                    mag_filter: filter,
                    min_filter: filter,
                    mipmap_filter: mipmap,
                    lod_clamp: 0.0..if mipmap == wgt::MipmapFilterMode::Linear {
                        32.0
                    } else {
                        0.0
                    },
                    compare: None,
                    anisotropy_clamp: 1,
                    border_color: None,
                })
            }
            .map_err(|error| format!("Creating Vulkan sampler: {error:?}"))?;
            Ok::<_, String>(Owned::new(owner, raw, hal::vulkan::Device::destroy_sampler))
        };
        Ok(Self {
            samplers: [
                create(wgt::FilterMode::Nearest, wgt::MipmapFilterMode::Nearest)?,
                create(wgt::FilterMode::Linear, wgt::MipmapFilterMode::Nearest)?,
                create(wgt::FilterMode::Linear, wgt::MipmapFilterMode::Linear)?,
            ],
        })
    }

    pub fn get(&self, filter: TextureFilter) -> &hal::vulkan::Sampler {
        &self.samplers[match filter {
            TextureFilter::Nearest => 0,
            TextureFilter::Linear => 1,
            TextureFilter::Trilinear => 2,
        }]
    }
}
