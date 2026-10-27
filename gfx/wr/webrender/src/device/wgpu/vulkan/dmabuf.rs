/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub use super::hal::dma_buf::DmaBufFormat;
use super::{wgt, Device, ForeignRgbLayout};

#[path = "dmabuf_image.rs"]
mod image;
pub use self::image::{DmaBufImage, DmaBufImageDescriptor};

#[derive(Clone)]
pub struct DmaBufCapabilities {
    pub(super) native: std::sync::Arc<super::hal::dma_buf::Capabilities>,
}

impl DmaBufCapabilities {
    pub fn foreign_drm_node(&self) -> Option<[u64; 2]> {
        self.native.foreign_drm_node()
    }

    pub fn supports_foreign_rgb(&self, drm_node: [u64; 2], layout: ForeignRgbLayout) -> bool {
        self.native.foreign_drm_node() == Some(drm_node)
            && self.native.formats().iter().any(|format| {
                format.format() == layout.format()
                    && format.usage() == wgt::TextureUses::RESOURCE
                    && format.modifier() == 0
                    && format.supports_extent(layout.size())
            })
    }

    /// Match immutable device/format limits; import still validates the allocation.
    pub fn supports(&self, image: &DmaBufImageDescriptor) -> bool {
        let (device_uuid, driver_uuid) = self.native.identity();
        device_uuid == image.device_uuid
            && driver_uuid == image.driver_uuid
            && self.native.formats().iter().any(|format| {
                format.format() == image.format
                    && format.usage() == image.usage
                    && format.modifier() == image.modifier
                    && format.supports_extent(image.size)
            })
    }
}

impl Device {
    pub fn dma_buf_capabilities(&self) -> Result<DmaBufCapabilities, String> {
        if let Some(caps) = self.native_cache.capabilities.get() {
            return Ok(caps.clone());
        }
        if self.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let native = super::hal::dma_buf::query_capabilities(
            self.open.clone(),
            &*self.adapter,
            self.capabilities.limits.max_texture_dimension_2d,
        )?;
        let caps = DmaBufCapabilities { native };
        let _ = self.native_cache.capabilities.set(caps.clone());
        Ok(caps)
    }
    pub fn dma_buf_formats(
        &self,
        format: wgt::TextureFormat,
        usage: wgt::TextureUses,
    ) -> Result<Vec<DmaBufFormat>, String> {
        self.dma_buf_capabilities()?
            .native
            .formats_for(format, usage)
    }
}
#[cfg(test)]
#[path = "dmabuf_tests.rs"]
mod tests;
