/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
fn dma_buf_format_and_usage_are_explicit() {
    for (format, raw) in [
        (wgt::TextureFormat::Rgba8Unorm, vk::Format::R8G8B8A8_UNORM),
        (wgt::TextureFormat::Bgra8Unorm, vk::Format::B8G8R8A8_UNORM),
    ] {
        assert_eq!(
            image_parameters(format, wgt::TextureUses::RESOURCE)
                .unwrap()
                .0,
            raw
        );
    }
    assert!(image_parameters(wgt::TextureFormat::R8Unorm, wgt::TextureUses::RESOURCE).is_err());
    assert!(image_parameters(
        wgt::TextureFormat::Rgba8UnormSrgb,
        wgt::TextureUses::RESOURCE
    )
    .is_err());
    for usage in [
        wgt::TextureUses::empty(),
        wgt::TextureUses::COPY_SRC,
        wgt::TextureUses::RESOURCE | wgt::TextureUses::STORAGE_READ_WRITE,
    ] {
        assert!(image_parameters(wgt::TextureFormat::Rgba8Unorm, usage).is_err());
    }
    let (_, usage, required) = image_parameters(
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureUses::RESOURCE
            | wgt::TextureUses::COPY_SRC
            | wgt::TextureUses::COPY_DST
            | wgt::TextureUses::COLOR_TARGET,
    )
    .unwrap();
    assert_eq!(
        usage,
        vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::COLOR_ATTACHMENT
    );
    assert!(required.contains(
        vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT
    ));
}

#[test]
fn dma_buf_modifiers_require_one_memory_plane_and_filtering() {
    let (_, _, required) =
        image_parameters(wgt::TextureFormat::Rgba8Unorm, wgt::TextureUses::RESOURCE).unwrap();
    let mut entry = vk::DrmFormatModifierPropertiesEXT::default()
        .drm_format_modifier_plane_count(1)
        .drm_format_modifier_tiling_features(required);
    assert!(eligible_modifier(&entry, required));
    for count in [0, 2, 3, 4] {
        entry.drm_format_modifier_plane_count = count;
        assert!(!eligible_modifier(&entry, required));
    }
    entry.drm_format_modifier_plane_count = 1;
    entry.drm_format_modifier_tiling_features = vk::FormatFeatureFlags::SAMPLED_IMAGE;
    assert!(!eligible_modifier(&entry, required));
}

#[test]
fn dma_buf_size_checks_both_dimensions() {
    let format = DmaBufFormat {
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::RESOURCE,
        modifier: 0,
        max_size: [32, 16],
        max_resource_size: 4096,
        exportable: true,
        dedicated_only: false,
    };
    for size in [[1, 1], [32, 16]] {
        assert!(format.supports_extent(size));
    }
    for size in [[0, 1], [1, 0], [33, 1], [1, 17], [u32::MAX, u32::MAX]] {
        assert!(!format.supports_extent(size));
    }
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF modifiers and validation"]
fn dma_buf_queries_match_native_image_creation() {
    use crate::device::wgpu::{
        Options,
        tests::{validation_logging, ERRORS},
    };
    use std::sync::atomic::Ordering;
    validation_logging();
    {
        let owner = Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let full_usage = wgt::TextureUses::RESOURCE
            | wgt::TextureUses::COPY_SRC
            | wgt::TextureUses::COPY_DST
            | wgt::TextureUses::COLOR_TARGET;
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for usage in [wgt::TextureUses::RESOURCE, full_usage] {
                let formats = owner.dma_buf_formats(format, usage).unwrap();
                assert!(formats
                    .iter()
                    .any(|format| format.modifier() == 0 && format.exportable()));
                assert!(formats
                    .iter()
                    .any(|format| format.modifier() != 0 && format.exportable()));
                let (raw_format, raw_usage, _) = image_parameters(format, usage).unwrap();
                for supported in formats {
                    assert!(supported.supports_extent([17, 13]));
                    assert_eq!(supported.format(), format);
                    assert_eq!(supported.usage(), usage);
                    let modifiers = [supported.modifier()];
                    let mut modifier = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                        .drm_format_modifiers(&modifiers);
                    let mut external = vk::ExternalMemoryImageCreateInfo::default()
                        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                    let info = vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(raw_format)
                        .extent(vk::Extent3D {
                            width: 17,
                            height: 13,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                        .usage(raw_usage)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED)
                        .push_next(&mut external)
                        .push_next(&mut modifier);
                    unsafe {
                        let raw = owner.raw_device().raw_device();
                        let image = raw.create_image(&info, None).unwrap();
                        let memory = raw.get_image_memory_requirements(image);
                        raw.destroy_image(image, None);
                        assert!(memory.size > 0 && memory.size <= supported.max_resource_size());
                    }
                }
            }
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
