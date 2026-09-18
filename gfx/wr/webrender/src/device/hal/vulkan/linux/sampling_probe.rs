/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
#[ignore = "Requires Vulkan DMA-BUF sharing on a real adapter"]
fn dmabuf_sampling_contract_probe() {
    let device = ExternalImageDevice::new(&Rc::new(
        create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    ));
    let producer = device.dmabuf_producer().unwrap();
    let owner = &producer.owner;
    let instance = owner.open.device.shared_instance().raw_instance();
    let physical = owner.open.device.raw_physical_device();
    println!("DMA-BUF device/driver: {:?}", ids(owner));
    for format in [api::ImageFormat::RGBA8, api::ImageFormat::BGRA8] {
        let vk_format = owner
            .adapter
            .texture_format_as_raw(plane_format(format).unwrap());
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk_format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        list.p_drm_format_modifier_properties = entries.as_mut_ptr();
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk_format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        entries.truncate(list.drm_format_modifier_count as usize);
        let required = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_SRC;
        let mut sampled = 0;
        for modifier in modifiers(owner, plane_format(format).unwrap()) {
            let properties = entries
                .iter()
                .find(|e| e.drm_format_modifier == modifier)
                .unwrap();
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(vk_format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
                .push_next(&mut external)
                .push_next(&mut drm);
            let mut external_properties = vk::ExternalImageFormatProperties::default();
            let mut result =
                vk::ImageFormatProperties2::default().push_next(&mut external_properties);
            let queried = unsafe {
                instance.get_physical_device_image_format_properties2(physical, &info, &mut result)
            }
            .is_ok();
            let limits = result.image_format_properties;
            let supported = queried
                && properties.drm_format_modifier_plane_count == 1
                && properties
                    .drm_format_modifier_tiling_features
                    .contains(required)
                && external_properties
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE);
            println!("DMA-BUF sampled probe: {format:?} modifier={modifier:#x} supported={supported} extent={:?} max_bytes={}", limits.max_extent, limits.max_resource_size);
            sampled += usize::from(supported);
        }
        assert!(sampled > 0, "No sampled/filterable tuple for {:?}", format);
    }
}
