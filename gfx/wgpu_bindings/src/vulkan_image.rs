/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::server::{
    dedicated_image_memory_requirements, dmabuf_image_properties, Global, ScopedVkImage,
    VulkanDmaBufInfo, DMABUF_DEVICE_EXTENSIONS,
};
use crate::vulkan_timeline::{submit_with_timelines, VulkanTimeline, VulkanTimelinePoint};
use crate::FfiTextureDescriptor;
use ash::{khr, vk};
use std::fs::File;
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use std::sync::Arc;
use wgc::resource::ParentDevice;
use wgpu_core_remote_types::id;

/// Import an allocation as a fresh, uncleared wgpu texture.
/// # Safety
/// The FD and metadata must describe the allocation from create_webrender_dma_buf.
/// No peer may access it while wgpu owns it. Recycled allocations must be acquired
/// from the external queue family before any wgpu commands use the new texture.
pub unsafe fn import_image(
    device: Arc<wgc::device::Device>,
    fd: BorrowedFd<'_>,
    desc: &wgc::resource::TextureDescriptor<'_>,
    info: &VulkanDmaBufInfo,
) -> Result<Arc<wgc::resource::Texture>, String> {
    device
        .validate_texture_descriptor(desc)
        .map_err(|e| format!("{e:?}"))?;
    let format = if info.rgba {
        wgt::TextureFormat::Rgba8Unorm
    } else {
        wgt::TextureFormat::Bgra8Unorm
    };
    let mut allowed = wgt::TextureUsages::COPY_DST | wgt::TextureUsages::TEXTURE_BINDING;
    let mut uses = wgt::TextureUses::COPY_DST | wgt::TextureUses::RESOURCE;
    let mut usage = vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
    let mut features = vk::FormatFeatureFlags::TRANSFER_DST
        | vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    if info.copy_src {
        allowed |= wgt::TextureUsages::COPY_SRC;
        uses |= wgt::TextureUses::COPY_SRC;
        usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        features |= vk::FormatFeatureFlags::TRANSFER_SRC;
    }
    if info.color_target {
        allowed |= wgt::TextureUsages::RENDER_ATTACHMENT;
        uses |= wgt::TextureUses::COLOR_TARGET;
        usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
        features |= vk::FormatFeatureFlags::COLOR_ATTACHMENT;
    }
    if !info.layout.is_valid
        || info.layout.plane_count != 1
        || !info.copy_dst
        || desc.format != format
        || !allowed.contains(desc.usage)
        || desc.dimension != wgt::TextureDimension::D2
        || desc.size.depth_or_array_layers != 1
        || desc.sample_count != 1
        || desc.mip_level_count != 1
        || desc.view_formats.iter().any(|f| *f != format)
    {
        return Err("Incompatible Vulkan image descriptor".into());
    }
    let hal = device
        .clone()
        .as_hal::<wgc::api::Vulkan>()
        .ok_or("Vulkan device unavailable")?;
    if DMABUF_DEVICE_EXTENSIONS
        .iter()
        .any(|name| !hal.enabled_device_extensions().contains(name))
    {
        return Err("Vulkan DMA-BUF extensions are not enabled".into());
    }
    let instance = hal.shared_instance().raw_instance();
    let physical = hal.raw_physical_device();
    if instance
        .get_physical_device_properties(physical)
        .api_version
        < vk::API_VERSION_1_1
    {
        return Err("Vulkan 1.1 is required for shared images".into());
    }
    let mut ids = vk::PhysicalDeviceIDProperties::default();
    instance.get_physical_device_properties2(
        physical,
        &mut vk::PhysicalDeviceProperties2::default().push_next(&mut ids),
    );
    if ids.device_uuid != info.device_uuid || ids.driver_uuid != info.driver_uuid {
        return Err("Vulkan image device or driver mismatch".into());
    }
    let raw_format = if info.rgba {
        vk::Format::R8G8B8A8_UNORM
    } else {
        vk::Format::B8G8R8A8_UNORM
    };
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    instance.get_physical_device_format_properties2(
        physical,
        raw_format,
        &mut vk::FormatProperties2::default().push_next(&mut list),
    );
    let mut modifiers = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut modifiers);
    instance.get_physical_device_format_properties2(
        physical,
        raw_format,
        &mut vk::FormatProperties2::default().push_next(&mut list),
    );
    if !modifiers.iter().any(|m| {
        m.drm_format_modifier == info.layout.modifier
            && m.drm_format_modifier_plane_count == 1
            && m.drm_format_modifier_tiling_features.contains(features)
    }) {
        return Err("Unsupported Vulkan image modifier".into());
    }
    let limits =
        dmabuf_image_properties(instance, physical, raw_format, info.layout.modifier, usage)
            .ok_or("Unsupported Vulkan image usage")?;
    if desc.size.width > limits.max_extent.width || desc.size.height > limits.max_extent.height {
        return Err("Vulkan image dimensions exceed modifier limits".into());
    }
    let file = File::from(fd.try_clone_to_owned().map_err(|e| e.to_string())?);
    let bytes = file.metadata().map_err(|e| e.to_string())?.len();
    let offset = info.layout.offsets[0];
    let stride = info.layout.strides[0];
    if stride == 0 || offset >= bytes {
        return Err("Invalid Vulkan image layout".into());
    }
    if info.layout.modifier == 0 {
        let row = u64::from(desc.size.width) * 4;
        let end = stride
            .checked_mul(u64::from(desc.size.height - 1))
            .and_then(|n| n.checked_add(offset))
            .and_then(|n| n.checked_add(row));
        if stride < row || stride % 4 != 0 || offset % 4 != 0 || end.is_none_or(|n| n > bytes) {
            return Err("Invalid linear Vulkan image layout".into());
        }
    }
    let plane = [vk::SubresourceLayout::default()
        .offset(offset)
        .row_pitch(stride)];
    let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(info.layout.modifier)
        .plane_layouts(&plane);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw = hal.raw_device();
    let image = raw
        .create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(raw_format)
                .extent(vk::Extent3D {
                    width: desc.size.width,
                    height: desc.size.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut modifier)
                .push_next(&mut external),
            None,
        )
        .map_err(|e| format!("Creating Vulkan image: {e:?}"))?;
    let mut owned = ScopedVkImage::new(raw, image);
    let requirements = dedicated_image_memory_requirements(instance, raw, image);
    if requirements.size > bytes || requirements.size > limits.max_resource_size {
        return Err("Vulkan image requirements exceed allocation limits".into());
    }
    let extension = khr::external_memory_fd::Device::new(instance, raw);
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    extension
        .get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            file.as_raw_fd(),
            &mut fd_properties,
        )
        .map_err(|e| format!("Querying DMA-BUF memory: {e:?}"))?;
    let properties = instance.get_physical_device_memory_properties(physical);
    let bits = requirements.memory_type_bits & fd_properties.memory_type_bits;
    let index = (0..properties.memory_type_count)
        .find(|&i| {
            bits & (1 << i) != 0
                && !properties.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::PROTECTED)
        })
        .ok_or("No compatible unprotected memory type")?;
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(file.as_raw_fd());
    owned.memory = raw
        .allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(index)
                .push_next(&mut dedicated)
                .push_next(&mut import),
            None,
        )
        .map_err(|e| format!("Importing Vulkan memory: {e:?}"))?;
    let _ = file.into_raw_fd();
    raw.bind_image_memory(image, owned.memory, 0)
        .map_err(|e| format!("Binding Vulkan memory: {e:?}"))?;
    let texture = wgh::vulkan::Device::texture_from_raw(
        &hal,
        image,
        &wgh::TextureDescriptor {
            label: None,
            size: desc.size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format,
            usage: uses,
            memory_flags: wgh::MemoryFlags::empty(),
            view_formats: vec![],
        },
        None,
        wgh::vulkan::TextureMemory::Dedicated(owned.memory),
    );
    owned.release();
    let (texture, error) = device.create_texture_from_hal(
        Box::new(texture),
        desc,
        wgt::TextureUses::UNINITIALIZED,
        false,
    );
    match error {
        Some(error) => Err(format!("Registering Vulkan texture: {error:?}")),
        None => Ok(texture),
    }
}

/// # Safety
/// The nonnegative FD must remain open and obey import_image's allocation and
/// ownership contract. The texture ID must be available in the global hub.
/// Recycled allocations require their return/ready timeline point; fresh ones
/// use a null timeline and zero value.
#[no_mangle]
pub unsafe extern "C" fn wgpu_vkimage_import_for_webrender(
    global: &Global,
    device_id: id::DeviceId,
    texture_id: id::TextureId,
    desc: &FfiTextureDescriptor,
    fd: i32,
    info: &VulkanDmaBufInfo,
    returned: Option<&VulkanTimeline>,
    value: u64,
) -> bool {
    if fd < 0 || returned.is_none() != (value == 0) {
        return false;
    }
    let device = global.resolve_device_id(device_id);
    match import_image(
        device.clone(),
        BorrowedFd::borrow_raw(fd),
        &desc.to_wgpu(),
        info,
    ) {
        Ok(texture) => {
            if let Some(returned) = returned {
                let Some(queue) = device.get_queue() else {
                    return false;
                };
                if acquire_image(&queue, texture.clone(), returned, value).is_none() {
                    return false;
                }
            }
            global.import_texture(texture, texture_id);
            true
        }
        Err(_) => false,
    }
}

/// Acquire a freshly imported allocation after its previous publication ends.
/// # Safety
/// The texture must not have been used by wgpu yet. The wait must be the submitted
/// consumer return, or the producer's ready point for an unused publication.
/// The peer must have released GENERAL/EXTERNAL ownership. Device/queue access
/// must obey submit_with_timelines's serialization contract.
pub unsafe fn acquire_image(
    queue: &Arc<wgc::device::queue::Queue>,
    texture: Arc<wgc::resource::Texture>,
    returned: &VulkanTimeline,
    value: u64,
) -> Option<u64> {
    let device = queue.device();
    let desc = texture.descriptor();
    if !Arc::ptr_eq(device, texture.device())
        || desc.dimension != wgt::TextureDimension::D2
        || desc.mip_level_count != 1
        || desc.sample_count != 1
        || desc.size.depth_or_array_layers != 1
        || !matches!(
            desc.format,
            wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
        )
    {
        return None;
    }
    device.check_is_valid().ok()?;
    let hal = device.clone().as_hal::<wgc::api::Vulkan>()?;
    let image = texture.clone().as_hal::<wgc::api::Vulkan>()?;
    let acquire = device.create_command_encoder(&Default::default());
    let recorded = acquire.as_hal_mut::<wgc::api::Vulkan, _, _>(|encoder| {
        let Some(encoder) = encoder else {
            return false;
        };
        let barrier = vk::ImageMemoryBarrier::default()
            .image(image.raw_handle())
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(hal.queue_family_index())
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
        hal.raw_device().cmd_pipeline_barrier(
            encoder.raw_handle(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
        true
    });
    drop(image);
    let acquire = acquire.finish(&Default::default());
    if !recorded {
        return None;
    }
    // Track the image after ownership acquisition so explicit destruction also
    // defers its native storage until this submission completes.
    let tracked = device.create_command_encoder(&Default::default());
    tracked.transition_resources(
        std::iter::empty(),
        std::iter::once(wgt::TextureTransition {
            texture: texture.clone(),
            selector: None,
            state: wgt::TextureUses::COPY_DST,
        }),
    );
    let tracked = tracked.finish(&Default::default());
    let result = submit_with_timelines(
        queue,
        &[acquire, tracked],
        &[VulkanTimelinePoint {
            timeline: Some(returned),
            value,
        }],
        &[],
    );
    if result.is_none() {
        texture.destroy();
    }
    result
}

/// Publish a single-plane imported image, consuming its producer texture.
/// # Safety
/// The image must be the allocation being published. Device/queue access must
/// obey submit_with_timelines's serialization contract. No peer may access the
/// allocation before the successful signal; failed publications must be discarded.
pub unsafe fn release_image(
    queue: &Arc<wgc::device::queue::Queue>,
    texture: Arc<wgc::resource::Texture>,
    ready: &VulkanTimeline,
    value: u64,
) -> Option<u64> {
    let device = queue.device();
    let desc = texture.descriptor();
    if !Arc::ptr_eq(device, texture.device())
        || desc.dimension != wgt::TextureDimension::D2
        || desc.mip_level_count != 1
        || desc.sample_count != 1
        || desc.size.depth_or_array_layers != 1
        || !matches!(
            desc.format,
            wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
        )
    {
        return None;
    }
    device.check_is_valid().ok()?;
    let hal = device.clone().as_hal::<wgc::api::Vulkan>()?;
    let image = texture.clone().as_hal::<wgc::api::Vulkan>()?;
    let prepare = device.create_command_encoder(&Default::default());
    prepare.transition_resources(
        std::iter::empty(),
        std::iter::once(wgt::TextureTransition {
            texture: texture.clone(),
            selector: None,
            state: wgt::TextureUses::RESOURCE,
        }),
    );
    let prepare = prepare.finish(&Default::default());
    let release = device.create_command_encoder(&Default::default());
    let recorded = release.as_hal_mut::<wgc::api::Vulkan, _, _>(|encoder| {
        let Some(encoder) = encoder else {
            return false;
        };
        let raw = hal.raw_device();
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = vk::ImageMemoryBarrier::default()
            .image(image.raw_handle())
            .subresource_range(range)
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
        raw.cmd_pipeline_barrier(
            encoder.raw_handle(),
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
        let barrier = barrier
            .old_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(hal.queue_family_index())
            .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_access_mask(vk::AccessFlags::empty());
        raw.cmd_pipeline_barrier(
            encoder.raw_handle(),
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
        true
    });
    drop(image);
    let release = release.finish(&Default::default());
    if !recorded {
        return None;
    }
    let result = submit_with_timelines(
        queue,
        &[prepare, release],
        &[],
        &[VulkanTimelinePoint {
            timeline: Some(ready),
            value,
        }],
    );
    texture.destroy();
    result
}

/// # Safety
/// The texture must be the imported allocation being published. Device/queue
/// access must obey release_image's contract. A zero result forbids publication.
#[no_mangle]
pub unsafe extern "C" fn wgpu_vkimage_release_for_webrender(
    global: &Global,
    queue_id: id::QueueId,
    texture_id: id::TextureId,
    ready: Option<&VulkanTimeline>,
    value: u64,
) -> u64 {
    let Some(ready) = ready else {
        return 0;
    };
    release_image(
        &global.resolve_queue_id(queue_id),
        global.resolve_texture_id(texture_id),
        ready,
        value,
    )
    .unwrap_or(0)
}
