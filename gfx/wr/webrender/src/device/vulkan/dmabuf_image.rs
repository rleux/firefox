/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{image_parameters, wgt, Device};
use ash::vk;
use std::fs::File;
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use std::rc::Rc;
use crate::device::vulkan::state::UsageState;
use crate::device::vulkan::textures::TextureState;

#[path = "dmabuf_access.rs"]
mod access;

#[derive(Clone, Copy, Debug)]
pub struct DmaBufImageDescriptor {
    pub size: [u32; 2],
    pub format: wgt::TextureFormat,
    pub usage: wgt::TextureUses,
    pub modifier: u64,
    pub offset: u64,
    pub row_pitch: u64,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

impl DmaBufImageDescriptor {
    pub(in crate::device::vulkan) fn validate_plane(&self, bytes: u64) -> Result<(), String> {
        if self.size.contains(&0)
            || self.size.iter().any(|&size| size > i32::MAX as u32)
            || self.row_pitch == 0
            || self.offset >= bytes
        {
            return Err("Invalid DMA-BUF dimensions, pitch or offset".into());
        }
        if self.modifier == 0 {
            let row = u64::from(self.size[0]) * 4;
            if self.row_pitch < row || self.row_pitch % 4 != 0 || self.offset % 4 != 0 {
                return Err("Invalid linear RGB DMA-BUF layout".into());
            }
            let end = self
                .row_pitch
                .checked_mul(u64::from(self.size[1] - 1))
                .and_then(|value| value.checked_add(row))
                .and_then(|value| value.checked_add(self.offset))
                .ok_or("Linear DMA-BUF layout overflow")?;
            if end > bytes {
                return Err("Linear DMA-BUF pixels exceed the allocation".into());
            }
        }
        Ok(())
    }
}

pub struct DmaBufImage {
    pub(in crate::device::vulkan) owner: Rc<Device>,
    pub(in crate::device::vulkan) image: vk::Image,
    memory: vk::DeviceMemory,
    descriptor: DmaBufImageDescriptor,
    pub(in crate::device::vulkan) states: Rc<Vec<UsageState<TextureState>>>,
}

impl DmaBufImage {
    pub fn descriptor(&self) -> &DmaBufImageDescriptor {
        &self.descriptor
    }
}

impl Drop for DmaBufImage {
    fn drop(&mut self) {
        unsafe {
            let raw = self.owner.open.device.raw_device();
            raw.destroy_image(self.image, None);
            raw.free_memory(self.memory, None);
        }
    }
}

pub(super) fn identity(owner: &Device) -> ([u8; 16], [u8; 16]) {
    let mut id = vk::PhysicalDeviceIDProperties::default();
    unsafe {
        owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_properties2(
                owner.open.device.raw_physical_device(),
                &mut vk::PhysicalDeviceProperties2::default().push_next(&mut id),
            );
    }
    (id.device_uuid, id.driver_uuid)
}

fn memory_type(bits: u32, properties: &vk::PhysicalDeviceMemoryProperties) -> Result<u32, String> {
    (0..properties.memory_type_count)
        .find(|&index| {
            bits & (1 << index) != 0
                && !properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::PROTECTED)
        })
        .ok_or_else(|| "No compatible unprotected DMA-BUF memory type".into())
}

impl Device {
    /// Bind a single-plane Vulkan DMA-BUF allocation without acquiring it for GPU use.
    /// # Safety
    /// The FD must be a real, unprotected DMA-BUF matching the descriptor and
    /// producer identities, exported from a compatible Vulkan image with zero
    /// creation flags, one mip/layer/sample, and memory bound at offset zero.
    /// Later GPU use must acquire external ownership and synchronize with the producer.
    pub unsafe fn import_dma_buf(
        self: &Rc<Self>,
        fd: BorrowedFd<'_>,
        descriptor: DmaBufImageDescriptor,
    ) -> Result<Rc<DmaBufImage>, String> {
        self.import_dma_buf_impl(fd, descriptor, true)
    }

    pub(in crate::device::vulkan) unsafe fn import_dma_buf_impl(
        self: &Rc<Self>,
        fd: BorrowedFd<'_>,
        descriptor: DmaBufImageDescriptor,
        check_vulkan_identity: bool,
    ) -> Result<Rc<DmaBufImage>, String> {
        if self.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let supported = self
            .dma_buf_formats(descriptor.format, descriptor.usage)?
            .into_iter()
            .find(|entry| entry.modifier() == descriptor.modifier)
            .ok_or("Unsupported DMA-BUF format, modifier or usage")?;
        if !supported.supports_extent(descriptor.size) {
            return Err("DMA-BUF dimensions exceed device limits".into());
        }
        if check_vulkan_identity
            && identity(self) != (descriptor.device_uuid, descriptor.driver_uuid)
        {
            return Err("DMA-BUF device or driver identity mismatch".into());
        }
        let file = File::from(
            fd.try_clone_to_owned()
                .map_err(|error| format!("Duplicating DMA-BUF: {error}"))?,
        );
        let bytes = file
            .metadata()
            .map_err(|error| format!("Querying DMA-BUF size: {error}"))?
            .len();
        descriptor.validate_plane(bytes)?;
        let (format, usage, _) = image_parameters(descriptor.format, descriptor.usage)?;
        let plane = [vk::SubresourceLayout::default()
            .offset(descriptor.offset)
            .row_pitch(descriptor.row_pitch)];
        let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(descriptor.modifier)
            .plane_layouts(&plane);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: descriptor.size[0],
                height: descriptor.size[1],
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut modifier);
        let raw = self.open.device.raw_device();
        let image = unsafe { raw.create_image(&info, None) }
            .map_err(|error| format!("Creating imported DMA-BUF image: {error:?}"))?;
        let mut imported = DmaBufImage {
            owner: self.clone(),
            image,
            memory: vk::DeviceMemory::null(),
            descriptor,
            states: Rc::new(vec![UsageState::new(TextureState {
                usage: wgt::TextureUses::UNINITIALIZED,
                initialized: false,
            })]),
        };
        let requirements = unsafe { raw.get_image_memory_requirements(image) };
        if requirements.size > bytes || requirements.size > supported.max_resource_size() {
            return Err("DMA-BUF image requirements exceed allocation or format limits".into());
        }
        let extension = ash::khr::external_memory_fd::Device::new(
            self.open.device.shared_instance().raw_instance(),
            raw,
        );
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            extension.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                file.as_raw_fd(),
                &mut fd_properties,
            )
        }
        .map_err(|error| format!("Querying DMA-BUF memory types: {error:?}"))?;
        let properties = unsafe {
            self.open
                .device
                .shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(self.open.device.raw_physical_device())
        };
        let index = memory_type(
            requirements.memory_type_bits & fd_properties.memory_type_bits,
            &properties,
        )?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(file.as_raw_fd());
        imported.memory = unsafe {
            raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index)
                    .push_next(&mut dedicated)
                    .push_next(&mut import),
                None,
            )
        }
        .map_err(|error| format!("Importing DMA-BUF memory: {error:?}"))?;
        let _ = file.into_raw_fd();
        unsafe { raw.bind_image_memory(image, imported.memory, 0) }
            .map_err(|error| format!("Binding DMA-BUF image memory: {error:?}"))?;
        Ok(Rc::new(imported))
    }
}

#[cfg(test)]
#[path = "dmabuf_image_tests.rs"]
mod tests;
