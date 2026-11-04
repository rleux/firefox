/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, wgt, Device, Failure};
use crate::device::TextureFilter;
use std::sync::Arc;

pub(super) struct Samplers([Arc<core::resource::Sampler>; 3]);

impl Samplers {
    pub fn new(owner: &Device) -> Result<Self, Failure> {
        let create = |filter, mipmap| {
            let sampler = owner
                .device
                .create_sampler(&core::resource::SamplerDescriptor {
                    label: Some("WR sampler".into()),
                    mag_filter: filter,
                    min_filter: filter,
                    mipmap_filter: mipmap,
                    lod_max_clamp: if mipmap == wgt::MipmapFilterMode::Linear {
                        32.0
                    } else {
                        0.0
                    },
                    address_modes: [wgt::AddressMode::ClampToEdge; 3],
                    lod_min_clamp: 0.0,
                    compare: None,
                    anisotropy_clamp: 1,
                    border_color: None,
                });
            owner.check()?;
            Ok(sampler)
        };
        Ok(Self([
            create(wgt::FilterMode::Nearest, wgt::MipmapFilterMode::Nearest)?,
            create(wgt::FilterMode::Linear, wgt::MipmapFilterMode::Nearest)?,
            create(wgt::FilterMode::Linear, wgt::MipmapFilterMode::Linear)?,
        ]))
    }

    pub fn get(&self, filter: TextureFilter) -> Arc<core::resource::Sampler> {
        self.0[match filter {
            TextureFilter::Nearest => 0,
            TextureFilter::Linear => 1,
            TextureFilter::Trilinear => 2,
        }]
        .clone()
    }
}
