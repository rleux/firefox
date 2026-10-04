/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Device};
use ash::vk;

#[derive(Clone, Debug)]
pub struct DmaBufFormat {
    format: wgt::TextureFormat,
    usage: wgt::TextureUses,
    modifier: u64,
    max_size: [u32; 2],
    max_resource_size: u64,
    exportable: bool,
    dedicated_only: bool,
}

impl DmaBufFormat {
    pub fn format(&self) -> wgt::TextureFormat {
        self.format
    }
    pub fn usage(&self) -> wgt::TextureUses {
        self.usage
    }
    pub fn modifier(&self) -> u64 {
        self.modifier
    }
    pub fn max_size(&self) -> [u32; 2] {
        self.max_size
    }
    pub fn max_resource_size(&self) -> u64 {
        self.max_resource_size
    }
    pub fn exportable(&self) -> bool {
        self.exportable
    }
    pub fn dedicated_only(&self) -> bool {
        self.dedicated_only
    }

    /// Checks dimensions; allocation requirements still need validation after image creation.
    pub fn supports_extent(&self, size: [u32; 2]) -> bool {
        size[0] != 0 && size[1] != 0 && size[0] <= self.max_size[0] && size[1] <= self.max_size[1]
    }
}

pub(super) fn image_parameters(
    format: wgt::TextureFormat,
    usage: wgt::TextureUses,
) -> Result<(vk::Format, vk::ImageUsageFlags, vk::FormatFeatureFlags), String> {
    let format = match format {
        wgt::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        wgt::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        _ => return Err("DMA-BUF sampling requires RGBA8 or BGRA8".into()),
    };
    let allowed = wgt::TextureUses::RESOURCE
        | wgt::TextureUses::COPY_SRC
        | wgt::TextureUses::COPY_DST
        | wgt::TextureUses::COLOR_TARGET;
    if !usage.contains(wgt::TextureUses::RESOURCE) || !allowed.contains(usage) {
        return Err("Unsupported sampled DMA-BUF usage".into());
    }
    let mut vk_usage = vk::ImageUsageFlags::SAMPLED;
    let mut required =
        vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    for (uses, image, features) in [
        (
            wgt::TextureUses::COPY_SRC,
            vk::ImageUsageFlags::TRANSFER_SRC,
            vk::FormatFeatureFlags::TRANSFER_SRC,
        ),
        (
            wgt::TextureUses::COPY_DST,
            vk::ImageUsageFlags::TRANSFER_DST,
            vk::FormatFeatureFlags::TRANSFER_DST,
        ),
        (
            wgt::TextureUses::COLOR_TARGET,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
            vk::FormatFeatureFlags::COLOR_ATTACHMENT,
        ),
    ] {
        if usage.contains(uses) {
            vk_usage |= image;
            required |= features;
        }
    }
    Ok((format, vk_usage, required))
}

fn eligible_modifier(
    entry: &vk::DrmFormatModifierPropertiesEXT,
    required: vk::FormatFeatureFlags,
) -> bool {
    entry.drm_format_modifier_plane_count == 1
        && entry.drm_format_modifier_tiling_features.contains(required)
}

impl Device {
    /// Query single-memory-plane, linearly filterable RGB imports for this usage.
    pub fn dma_buf_formats(
        &self,
        format: wgt::TextureFormat,
        usage: wgt::TextureUses,
    ) -> Result<Vec<DmaBufFormat>, String> {
        let (raw_format, raw_usage, required) = image_parameters(format, usage)?;
        if !self
            .features
            .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Ok(Vec::new());
        }
        let raw = self.open.device.shared_instance().raw_instance();
        let physical = self.open.device.raw_physical_device();
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        unsafe {
            raw.get_physical_device_format_properties2(
                physical,
                raw_format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        list.p_drm_format_modifier_properties = entries.as_mut_ptr();
        unsafe {
            raw.get_physical_device_format_properties2(
                physical,
                raw_format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        entries.truncate(list.drm_format_modifier_count as usize);
        let mut formats = Vec::new();
        for entry in entries
            .iter()
            .filter(|entry| eligible_modifier(entry, required))
        {
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(entry.drm_format_modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(raw_format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(raw_usage)
                .push_next(&mut external)
                .push_next(&mut modifier);
            let mut external_properties = vk::ExternalImageFormatProperties::default();
            let mut properties =
                vk::ImageFormatProperties2::default().push_next(&mut external_properties);
            match unsafe {
                raw.get_physical_device_image_format_properties2(physical, &info, &mut properties)
            } {
                Ok(()) => {}
                Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED) => continue,
                Err(error) => return Err(format!("Querying DMA-BUF format: {error:?}")),
            }
            let limits = properties.image_format_properties;
            let memory = external_properties.external_memory_properties;
            if !memory
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
                || !memory
                    .compatible_handle_types
                    .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
                || limits.max_mip_levels == 0
                || limits.max_array_layers == 0
                || limits.max_extent.depth == 0
                || limits.max_resource_size == 0
            {
                continue;
            }
            let maximum = self
                .capabilities
                .limits
                .max_texture_dimension_2d
                .min(i32::MAX as u32);
            let max_size = [
                limits.max_extent.width.min(maximum),
                limits.max_extent.height.min(maximum),
            ];
            if max_size.contains(&0) {
                continue;
            }
            formats.push(DmaBufFormat {
                format,
                usage,
                modifier: entry.drm_format_modifier,
                max_size,
                max_resource_size: limits.max_resource_size,
                exportable: memory
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE),
                dedicated_only: memory
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::DEDICATED_ONLY),
            });
        }
        Ok(formats)
    }
}

#[cfg(test)]
#[path = "dmabuf_tests.rs"]
mod tests;
