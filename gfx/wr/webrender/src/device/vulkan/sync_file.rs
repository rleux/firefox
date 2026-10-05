/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::Device;
use ash::{khr, vk};
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use std::rc::Rc;

/// A single-use GPU wait for a fence exported by EGL or another compatible API.
pub struct SyncFileWait {
    pub(super) semaphore: Owned<vk::Semaphore>,
}

impl SyncFileWait {
    /// # Safety
    /// The borrowed FD must be a genuine SYNC_FD semaphore export or compatible fence.
    pub unsafe fn import(owner: &Rc<Device>, fd: BorrowedFd<'_>) -> Result<Self, String> {
        if owner.is_lost()
            || owner.open.device.shared_instance().instance_api_version() < vk::API_VERSION_1_1
            || !owner
                .open
                .device
                .enabled_device_extensions()
                .contains(&khr::external_semaphore_fd::NAME)
        {
            return Err("Vulkan sync-file import is unavailable".into());
        }
        let instance = owner.open.device.shared_instance().raw_instance();
        let mut properties = vk::ExternalSemaphoreProperties::default();
        instance.get_physical_device_external_semaphore_properties(
            owner.open.device.raw_physical_device(),
            &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            &mut properties,
        );
        if !properties
            .external_semaphore_features
            .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)
            || !properties
                .compatible_handle_types
                .contains(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
        {
            return Err("Device cannot import sync-file fences".into());
        }
        let fd = fd
            .try_clone_to_owned()
            .map_err(|error| format!("Duplicating sync-file: {error}"))?;
        let raw = owner.open.device.raw_device();
        let semaphore = raw
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            .map_err(|error| format!("Creating sync-file semaphore: {error:?}"))?;
        let semaphore = Owned::new(owner, semaphore, |device, semaphore| unsafe {
            device.raw_device().destroy_semaphore(semaphore, None);
        });
        khr::external_semaphore_fd::Device::new(instance, raw)
            .import_semaphore_fd(
                &vk::ImportSemaphoreFdInfoKHR::default()
                    .semaphore(*semaphore)
                    .flags(vk::SemaphoreImportFlags::TEMPORARY)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                    .fd(fd.as_raw_fd()),
            )
            .map_err(|error| format!("Importing sync-file semaphore: {error:?}"))?;
        let _ = fd.into_raw_fd();
        Ok(Self { semaphore })
    }
}
