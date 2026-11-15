/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, wgt, Device, TextureFilter};
use std::sync::Arc;

pub(super) struct Image {
    pub raw: Arc<core::resource::Texture>,
    pub sampled: Arc<core::resource::TextureView>,
    pub levels: Vec<Arc<core::resource::TextureView>>,
    pub filter: TextureFilter,
}

impl Image {
    pub fn new(
        owner: &Arc<Device>,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        filter: TextureFilter,
        renderable: bool,
    ) -> Result<Arc<Self>, String> {
        let mip_level_count = if filter == TextureFilter::Trilinear {
            width
                .max(height)
                .checked_ilog2()
                .ok_or("Invalid mipmap size")?
                + 1
        } else {
            1
        };
        let mut usage = wgt::TextureUsages::TEXTURE_BINDING
            | wgt::TextureUsages::COPY_SRC
            | wgt::TextureUsages::COPY_DST;
        if renderable || mip_level_count > 1 {
            usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
        }
        let raw = owner
            .create_texture(&core::resource::TextureDescriptor {
                label: Some("WR image".into()),
                size: wgt::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage,
                view_formats: vec![],
            })
            .map_err(|e| e.to_string())?;
        let sampled = raw.create_view(&Default::default());
        let mut levels = Vec::new();
        for mip in 0..mip_level_count {
            levels.push(raw.create_view(&core::resource::TextureViewDescriptor {
                range: wgt::ImageSubresourceRange {
                    base_mip_level: mip,
                    mip_level_count: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            }));
        }
        owner.check().map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            raw,
            sampled,
            levels,
            filter,
        }))
    }
    pub fn from_surface(
        owner: &Device,
        raw: Arc<core::resource::Texture>,
    ) -> Result<Arc<Self>, String> {
        let view = raw.create_view(&Default::default());
        owner.check().map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            raw,
            sampled: view.clone(),
            levels: vec![view],
            filter: TextureFilter::Nearest,
        }))
    }
    pub fn size(&self) -> wgt::Extent3d {
        self.raw.descriptor().size
    }
    pub fn format(&self) -> wgt::TextureFormat {
        self.raw.descriptor().format
    }
}
