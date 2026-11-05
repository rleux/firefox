/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::Device;
use ash::vk;
use std::{os::fd::{AsRawFd, BorrowedFd, IntoRawFd}, sync::Arc};

/// One temporary SYNC_FD payload, held until the native submission retires.
pub struct SyncFileWait {
    owner: Arc<Device>,
    pub(in crate::device::wgpu) semaphore: vk::Semaphore,
}

#[repr(C)]
#[derive(Default)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

impl SyncFileWait {
    /// Validates a Linux sync_file without consuming it or waiting on the CPU.
    pub fn ready(fd: BorrowedFd<'_>) -> Result<bool, String> {
        let mut info=SyncFileInfo::default();
        const FILE_INFO: libc::Ioctl=libc::_IOWR::<SyncFileInfo>(b'>' as u32,4);
        if unsafe{libc::ioctl(fd.as_raw_fd(),FILE_INFO,&mut info)}<0 {
            return Err("Ready FD is not a valid Linux sync_file".into());
        }
        match info.status {
            value if value<0=>Err("Producer sync_file reports failed GPU work".into()),
            0=>Ok(false),
            _=>Ok(true),
        }
    }

    /// The fence must cover all producer writes to the fully initialized image.
    pub unsafe fn import(owner:&Arc<Device>,fd:BorrowedFd<'_>)->Result<Arc<Self>,String> {
        owner.check().map_err(|error|error.to_string())?;
        if !owner.supports_sync_file_import(){return Err("SYNC_FD import unavailable".into());}
        if !Self::ready(fd)?{return Err("SYNC_FD is not ready for enqueue".into());}
        let fd=fd.try_clone_to_owned().map_err(|error|error.to_string())?;
        let semaphore=unsafe{owner.raw_device().raw_device().create_semaphore(&vk::SemaphoreCreateInfo::default(),None)}.map_err(|error|owner.native_error("Creating SYNC_FD semaphore",error))?;
        let wait=Arc::new(Self{owner:owner.clone(),semaphore});
        unsafe{owner.external_semaphore_fd().import_semaphore_fd(&vk::ImportSemaphoreFdInfoKHR::default().semaphore(semaphore).flags(vk::SemaphoreImportFlags::TEMPORARY).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD).fd(fd.as_raw_fd()))}.map_err(|error|owner.native_error("Importing SYNC_FD semaphore",error))?;
        let _=fd.into_raw_fd();Ok(wait)
    }
}
impl Drop for SyncFileWait {
    fn drop(&mut self){unsafe{self.owner.raw_device().raw_device().destroy_semaphore(self.semaphore,None);}}
}

impl Device {
    pub(in crate::device::wgpu) fn supports_sync_file_import(&self)->bool {
        *self.native_cache.sync_file.get_or_init(|| {
            let hal=self.raw_device();
            if hal.shared_instance().instance_api_version()<vk::API_VERSION_1_1 || !hal.enabled_device_extensions().contains(&ash::khr::external_semaphore_fd::NAME){return false;}
            let mut properties=vk::ExternalSemaphoreProperties::default();
            unsafe{hal.shared_instance().raw_instance().get_physical_device_external_semaphore_properties(hal.raw_physical_device(),&vk::PhysicalDeviceExternalSemaphoreInfo::default().handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),&mut properties);}
            properties.external_semaphore_features.contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)&&properties.compatible_handle_types.contains(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    #[test]
    fn regular_fd_is_rejected_before_vulkan_import() {
        let fd=std::fs::File::open("/dev/null").unwrap();
        assert!(SyncFileWait::ready(fd.as_fd()).is_err());
    }
}
