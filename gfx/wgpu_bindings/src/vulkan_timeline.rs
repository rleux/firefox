/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::{khr, vk};
use std::os::fd::{BorrowedFd, IntoRawFd};
use std::ptr;
use std::sync::Arc;

use crate::server::Global;
use wgpu_core_remote_types::id;

#[repr(C)]
pub struct VulkanTimelineDescriptor {
    pub fd: i32,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

pub struct VulkanTimeline {
    device: Arc<wgc::device::Device>,
    semaphore: vk::Semaphore,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    exportable: bool,
}

impl VulkanTimeline {
    fn create(device: Arc<wgc::device::Device>, exportable: bool) -> Option<Self> {
        device.check_is_valid().ok()?;
        let hal = unsafe { device.clone().as_hal::<wgc::api::Vulkan>() }?;
        let fence = unsafe { device.clone().fence_as_hal::<wgc::api::Vulkan>() }?;
        if !matches!(&*fence, wgh::vulkan::Fence::TimelineSemaphore(_))
            || !hal
                .enabled_device_extensions()
                .contains(&khr::external_semaphore_fd::NAME)
        {
            return None;
        }
        let instance = hal.shared_instance().raw_instance();
        let physical = hal.raw_physical_device();
        unsafe {
            if instance
                .get_physical_device_properties(physical)
                .api_version
                < vk::API_VERSION_1_1
            {
                return None;
            }
            let mut kind =
                vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                .push_next(&mut kind);
            let mut properties = vk::ExternalSemaphoreProperties::default();
            instance.get_physical_device_external_semaphore_properties(
                physical,
                &info,
                &mut properties,
            );
            if !properties.external_semaphore_features.contains(
                vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                    | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
            ) || !properties
                .compatible_handle_types
                .contains(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
            {
                return None;
            }
            let mut ids = vk::PhysicalDeviceIDProperties::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
            instance.get_physical_device_properties2(physical, &mut properties);
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
            let mut info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
            if exportable {
                info = info.push_next(&mut export);
            }
            let semaphore = hal.raw_device().create_semaphore(&info, None).ok()?;
            Some(Self {
                device,
                semaphore,
                device_uuid: ids.device_uuid,
                driver_uuid: ids.driver_uuid,
                exportable,
            })
        }
    }

    pub fn new(device: Arc<wgc::device::Device>) -> Option<Self> {
        Self::create(device, true)
    }

    /// # Safety
    /// A nonnegative descriptor FD must remain open throughout this call and
    /// refer to an OPAQUE_FD timeline semaphore.
    pub unsafe fn import(
        device: Arc<wgc::device::Device>,
        descriptor: &VulkanTimelineDescriptor,
    ) -> Option<Self> {
        if descriptor.fd < 0 {
            return None;
        }
        let timeline = Self::create(device, false)?;
        if descriptor.device_uuid != timeline.device_uuid
            || descriptor.driver_uuid != timeline.driver_uuid
        {
            return None;
        }
        let fd = BorrowedFd::borrow_raw(descriptor.fd)
            .try_clone_to_owned()
            .ok()?;
        let hal = timeline.device.clone().as_hal::<wgc::api::Vulkan>()?;
        let extension = khr::external_semaphore_fd::Device::new(
            hal.shared_instance().raw_instance(),
            hal.raw_device(),
        );
        use std::os::fd::AsRawFd;
        extension
            .import_semaphore_fd(
                &vk::ImportSemaphoreFdInfoKHR::default()
                    .semaphore(timeline.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                    .fd(fd.as_raw_fd()),
            )
            .ok()?;
        let _ = fd.into_raw_fd();
        Some(timeline)
    }

    /// The returned descriptor transfers ownership of its FD.
    pub fn export(&self) -> Option<VulkanTimelineDescriptor> {
        if !self.exportable || self.device.check_is_valid().is_err() {
            return None;
        }
        let hal = unsafe { self.device.clone().as_hal::<wgc::api::Vulkan>() }?;
        let extension = khr::external_semaphore_fd::Device::new(
            hal.shared_instance().raw_instance(),
            hal.raw_device(),
        );
        let fd = unsafe {
            extension
                .get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default()
                        .semaphore(self.semaphore)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
                )
                .ok()?
        };
        Some(VulkanTimelineDescriptor {
            fd,
            device_uuid: self.device_uuid,
            driver_uuid: self.driver_uuid,
        })
    }
}

impl Drop for VulkanTimeline {
    fn drop(&mut self) {
        let hal = unsafe { self.device.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
        unsafe { hal.raw_device().destroy_semaphore(self.semaphore, None) };
    }
}

#[no_mangle]
pub extern "C" fn wgpu_vulkan_timeline_new(
    global: &Global,
    device_id: id::DeviceId,
) -> *mut VulkanTimeline {
    VulkanTimeline::new(global.resolve_device_id(device_id))
        .map_or(ptr::null_mut(), |value| Box::into_raw(Box::new(value)))
}

/// # Safety
/// A nonnegative descriptor FD must remain open throughout this call and
/// refer to an OPAQUE_FD timeline semaphore.
#[no_mangle]
pub unsafe extern "C" fn wgpu_vulkan_timeline_import(
    global: &Global,
    device_id: id::DeviceId,
    descriptor: &VulkanTimelineDescriptor,
) -> *mut VulkanTimeline {
    VulkanTimeline::import(global.resolve_device_id(device_id), descriptor)
        .map_or(ptr::null_mut(), |value| Box::into_raw(Box::new(value)))
}

// Successful export transfers the FD; failure leaves the output unchanged.
#[no_mangle]
pub extern "C" fn wgpu_vulkan_timeline_export(
    timeline: Option<&VulkanTimeline>,
    output: &mut VulkanTimelineDescriptor,
) -> bool {
    let Some(descriptor) = timeline.and_then(VulkanTimeline::export) else {
        return false;
    };
    *output = descriptor;
    true
}

/// # Safety
/// The pointer must be null or an owned handle returned by new/import.
#[no_mangle]
pub unsafe extern "C" fn wgpu_vulkan_timeline_delete(timeline: *mut VulkanTimeline) {
    if !timeline.is_null() {
        drop(Box::from_raw(timeline));
    }
}
