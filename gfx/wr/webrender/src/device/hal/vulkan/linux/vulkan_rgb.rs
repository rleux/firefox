/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::os::fd::{AsRawFd, IntoRawFd};

struct PendingImage<'a> {
    device: &'a ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for PendingImage<'_> {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

unsafe fn import_texture(
    owner: &Rc<Device<V>>,
    plane: &DmaBufPlane,
    desc: &hal::TextureDescriptor,
    bytes: u64,
) -> Result<hal::vulkan::Texture> {
    let device = &owner.open.device;
    let raw = device.raw_device();
    let layout = plane.layout;
    let planes = [vk::SubresourceLayout::default()
        .offset(layout.offset)
        .row_pitch(layout.stride)];
    let mut drm = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(layout.modifier)
        .plane_layouts(&planes);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(owner.adapter.texture_format_as_raw(desc.format))
        .extent(vk::Extent3D {
            width: layout.size[0],
            height: layout.size[1],
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external)
        .push_next(&mut drm);
    let image = raw
        .create_image(&info, None)
        .map_err(|error| format!("Creating sampled DMA-BUF image: {error:?}"))?;
    let mut pending = PendingImage {
        device: raw,
        image,
        memory: vk::DeviceMemory::null(),
    };
    let requirements = raw.get_image_memory_requirements(image);
    if requirements.size > bytes {
        return Err("Vulkan image requirements exceed the DMA-BUF allocation".into());
    }
    let fd = plane.fd.try_clone().map_err(|error| error.to_string())?;
    let extension =
        ash::khr::external_memory_fd::Device::new(device.shared_instance().raw_instance(), raw);
    let mut properties = vk::MemoryFdPropertiesKHR::default();
    extension
        .get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut properties,
        )
        .map_err(|error| format!("Querying DMA-BUF memory types: {error:?}"))?;
    let memory_types = requirements.memory_type_bits & properties.memory_type_bits;
    if memory_types == 0 {
        return Err("No compatible sampled DMA-BUF memory type".into());
    }
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(fd.as_raw_fd());
    let allocation = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_types.trailing_zeros())
        .push_next(&mut dedicated)
        .push_next(&mut import);
    pending.memory = raw
        .allocate_memory(&allocation, None)
        .map_err(|error| format!("Importing sampled DMA-BUF memory: {error:?}"))?;
    let _ = fd.into_raw_fd();
    raw.bind_image_memory(image, pending.memory, 0)
        .map_err(|error| format!("Binding sampled DMA-BUF memory: {error:?}"))?;
    let texture = device.texture_from_raw(
        image,
        desc,
        None,
        hal::vulkan::TextureMemory::Dedicated(pending.memory),
    );
    pending.image = vk::Image::null();
    pending.memory = vk::DeviceMemory::null();
    Ok(texture)
}

#[derive(Clone)]
pub struct VulkanDmaBufImage(ForeignRgbImage);

pub struct WeakVulkanDmaBufImage(WeakForeignRgbImage);

impl WeakVulkanDmaBufImage {
    pub fn upgrade(&self) -> Option<VulkanDmaBufImage> {
        self.0.upgrade().map(VulkanDmaBufImage)
    }
}

impl VulkanDmaBufImage {
    pub fn downgrade(&self) -> WeakVulkanDmaBufImage {
        WeakVulkanDmaBufImage(self.0.downgrade())
    }

    pub fn belongs_to(&self, device: &ExternalImageDevice) -> bool {
        device.0.as_any().downcast_ref::<Producer<V>>().map_or(false, |producer| {
            Rc::ptr_eq(
                &self.0 .0.release.access.as_ref().unwrap().owner,
                &producer.owner,
            )
        })
    }

    pub fn lease(&self, uv: TexelRect) -> Result<ExternalImageLease> {
        if self.0 .0.release.status.get() == ExternalImageRelease::Abandoned {
            return Err("Vulkan DMA-BUF publication was abandoned".into());
        }
        self.0.lease(uv)
    }
}

fn sampled_limits(owner: &Device<V>, layout: DmaBufLayout) -> Result<vk::ImageFormatProperties> {
    layout.validate()?;
    if !supported(owner)
        || !matches!(
            layout.format,
            api::ImageFormat::RGBA8 | api::ImageFormat::BGRA8
        )
    {
        return Err("Sampled Vulkan DMA-BUF format or sharing is unavailable".into());
    }
    if (layout.device_uuid, layout.driver_uuid) != ids(owner) {
        return Err("Sampled DMA-BUF device/driver identity differs".into());
    }
    let raw = owner.open.device.shared_instance().raw_instance();
    let physical = owner.open.device.raw_physical_device();
    let format = owner
        .adapter
        .texture_format_as_raw(plane_format(layout.format)?);
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    unsafe {
        raw.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut list),
        );
    }
    let mut entries = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list.p_drm_format_modifier_properties = entries.as_mut_ptr();
    unsafe {
        raw.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut list),
        );
    }
    entries.truncate(list.drm_format_modifier_count as usize);
    let required = vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        | vk::FormatFeatureFlags::TRANSFER_SRC;
    if !entries.iter().any(|entry| {
        entry.drm_format_modifier == layout.modifier
            && entry.drm_format_modifier_plane_count == 1
            && entry.drm_format_modifier_tiling_features.contains(required)
    }) {
        return Err("Vulkan DMA-BUF modifier cannot be sampled, filtered and captured".into());
    }
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(layout.modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .push_next(&mut external)
        .push_next(&mut drm);
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external_properties);
    unsafe { raw.get_physical_device_image_format_properties2(physical, &info, &mut properties) }
        .map_err(|error| format!("Querying sampled Vulkan DMA-BUF: {error:?}"))?;
    let limits = properties.image_format_properties;
    if !external_properties
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    {
        return Err("Sampled Vulkan DMA-BUF is not importable".into());
    }
    owner.layout(layout.size[0], layout.size[1])?;
    if layout.size[0] > limits.max_extent.width
        || layout.size[1] > limits.max_extent.height
        || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
        || limits.max_mip_levels == 0
        || limits.max_array_layers == 0
        || u64::from(layout.size[0]) * u64::from(layout.size[1]) * 4 > limits.max_resource_size
    {
        return Err("Sampled Vulkan DMA-BUF dimensions exceed format limits".into());
    }
    Ok(limits)
}

impl ExternalImageDevice {
    pub fn supports_dmabuf_sampling(&self, layout: DmaBufLayout) -> bool {
        self.dmabuf_producer().map_or(false, |producer| {
            sampled_limits(&producer.owner, layout).is_ok()
        })
    }

    /// Imports an initialized Vulkan allocation without a materialization copy.
    /// # Safety
    /// The producer must release GENERAL layout to EXTERNAL before `ready` signals,
    /// and prohibit writes until `release` reports Unused or Complete. The FD and
    /// layout must identify a live single-memory-plane allocation on this device
    /// and driver. Reuse this publication for repeated leases; do not concurrently
    /// import aliases. Abandoned allocations must not be recycled.
    pub unsafe fn import_vulkan_dmabuf(
        &self,
        plane: &DmaBufPlane,
        ready: &SyncFile,
        generation: u64,
        release: impl FnOnce(ExternalImageRelease) + 'static,
    ) -> Result<VulkanDmaBufImage> {
        let mut guard = ReleaseGuard {
            callback: Some(Box::new(release)),
            access: None,
            status: Cell::new(ExternalImageRelease::Unused),
        };
        if generation == 0 {
            return Err("Vulkan DMA-BUF publication needs a generation".into());
        }
        let owner = &self.dmabuf_producer()?.owner;
        let layout = plane.layout;
        let limits = sampled_limits(owner, layout)?;
        let bytes = File::from(plane.fd.try_clone().map_err(|error| error.to_string())?)
            .metadata()
            .map_err(|error| error.to_string())?
            .len();
        let end = layout.offset
            + layout.stride * u64::from(layout.size[1] - 1)
            + u64::from(layout.size[0]) * 4;
        if end > bytes || bytes > limits.max_resource_size {
            return Err("Sampled Vulkan DMA-BUF allocation bounds differ".into());
        }
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let desc = texture_descriptor(
            wgt::Extent3d {
                width: layout.size[0],
                height: layout.size[1],
                depth_or_array_layers: 1,
            },
            plane_format(layout.format)?,
            wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC,
        );
        let raw = import_texture(owner, plane, &desc, bytes)?;
        let texture = Texture::from_raw(
            owner,
            raw,
            &desc,
            crate::device::TextureFilter::Linear,
            false,
            wgt::TextureUses::RESOURCE,
        )?;
        guard.access = Some(ForeignAccess {
            device: self.clone(),
            owner: owner.clone(),
            texture: texture.clone(),
            lifetime: ForeignRgbLifetime::new(),
            external_family: vk::QUEUE_FAMILY_EXTERNAL,
            releases: self.dmabuf_producer()?.releases.clone(),
        });
        guard.access.as_mut().unwrap().acquire(ready)?;
        Ok(VulkanDmaBufImage(ForeignRgbImage(Rc::new(
            ForeignPublication {
                image: ExternalNativeImage::new(texture, layout.descriptor()),
                generation,
                release: guard,
            },
        ))))
    }
}

#[cfg(test)]
#[path = "vulkan_rgb_gpu.rs"]
mod tests;
