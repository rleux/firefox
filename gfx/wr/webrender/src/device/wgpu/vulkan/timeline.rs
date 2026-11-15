/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::Device;
use ash::vk;
use std::{
    cell::Cell,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd},
    rc::Rc,
    sync::Arc,
};

pub struct TimelineHandle {
    fd: OwnedFd,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}
impl TimelineHandle {
    /// The FD is an OPAQUE_FD timeline export with matching identities and one signaller.
    pub unsafe fn from_fd(fd: OwnedFd, device_uuid: [u8; 16], driver_uuid: [u8; 16]) -> Self {
        Self {
            fd,
            device_uuid,
            driver_uuid,
        }
    }
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
    pub fn into_parts(self) -> (OwnedFd, [u8; 16], [u8; 16]) {
        (self.fd, self.device_uuid, self.driver_uuid)
    }
}
pub struct SharedTimeline {
    pub(in crate::device::wgpu) owner: Arc<Device>,
    pub(in crate::device::wgpu) semaphore: vk::Semaphore,
    last_signal: Option<Cell<u64>>,
    max_difference: u64,
}
pub(in crate::device::wgpu) fn properties(owner: &Device) -> ([u8; 16], [u8; 16], u64) {
    *owner.native_cache.properties.get_or_init(|| {
        let hal = owner.raw_device();
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut timeline = vk::PhysicalDeviceTimelineSemaphoreProperties::default();
        unsafe {
            hal.shared_instance()
                .raw_instance()
                .get_physical_device_properties2(
                    hal.raw_physical_device(),
                    &mut vk::PhysicalDeviceProperties2::default()
                        .push_next(&mut id)
                        .push_next(&mut timeline),
                );
        }
        (
            id.device_uuid,
            id.driver_uuid,
            timeline.max_timeline_semaphore_value_difference,
        )
    })
}
impl Drop for SharedTimeline {
    fn drop(&mut self) {
        unsafe {
            self.owner
                .raw_device()
                .raw_device()
                .destroy_semaphore(self.semaphore, None)
        }
    }
}
impl SharedTimeline {
    fn create(owner: &Arc<Device>, producer: bool) -> Result<Rc<Self>, String> {
        owner.check().map_err(|e| e.to_string())?;
        if !owner.supports_dma_buf() {
            return Err("External timelines unavailable".into());
        }
        let mut ty =
            vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
        let mut export = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        let mut info = vk::SemaphoreCreateInfo::default().push_next(&mut ty);
        if producer {
            info = info.push_next(&mut export);
        }
        let semaphore = unsafe {
            owner
                .raw_device()
                .raw_device()
                .create_semaphore(&info, None)
        }
        .map_err(|e| owner.native_error("Creating timeline", e))?;
        Ok(Rc::new(Self {
            owner: owner.clone(),
            semaphore,
            last_signal: producer.then(|| Cell::new(0)),
            max_difference: properties(owner).2,
        }))
    }
    pub fn new(owner: &Arc<Device>) -> Result<Rc<Self>, String> {
        Self::create(owner, true)
    }
    pub fn export(&self) -> Result<TimelineHandle, String> {
        self.owner.check().map_err(|e| e.to_string())?;
        if self.last_signal.is_none() {
            return Err("Imported timeline cannot export".into());
        }
        let fd = unsafe {
            self.owner.external_semaphore_fd().get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(self.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
            )
        }
        .map_err(|e| self.owner.native_error("Exporting timeline", e))?;
        let (device_uuid, driver_uuid, _) = properties(&self.owner);
        Ok(TimelineHandle {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            device_uuid,
            driver_uuid,
        })
    }
    pub fn import(owner: &Arc<Device>, handle: &TimelineHandle) -> Result<Rc<Self>, String> {
        let (device, driver, _) = properties(owner);
        if (device, driver) != (handle.device_uuid, handle.driver_uuid) {
            return Err("Timeline identity mismatch".into());
        }
        let result = Self::create(owner, false)?;
        let fd = handle.fd.try_clone().map_err(|e| e.to_string())?;
        unsafe {
            owner.external_semaphore_fd().import_semaphore_fd(
                &vk::ImportSemaphoreFdInfoKHR::default()
                    .semaphore(result.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                    .fd(fd.as_raw_fd()),
            )
        }
        .map_err(|e| owner.native_error("Importing timeline", e))?;
        let _ = fd.into_raw_fd();
        Ok(result)
    }
    pub(in crate::device::wgpu) fn current_value(&self) -> Result<u64, String> {
        let hal = self.owner.raw_device();
        let raw = hal.raw_device();
        let result = unsafe {
            if hal
                .enabled_device_extensions()
                .contains(&ash::khr::timeline_semaphore::NAME)
            {
                ash::khr::timeline_semaphore::Device::new(hal.shared_instance().raw_instance(), raw)
                    .get_semaphore_counter_value(self.semaphore)
            } else {
                raw.get_semaphore_counter_value(self.semaphore)
            }
        };
        result.map_err(|e| self.owner.native_error("Querying native timeline", e))
    }
    pub(in crate::device::wgpu) fn validate_value(&self, value: u64) -> Result<(), String> {
        if self.max_difference != u64::MAX
            && value.saturating_sub(self.current_value()?) > self.max_difference
        {
            return Err("Timeline value exceeds device range".into());
        }
        Ok(())
    }
    pub(in crate::device::wgpu) fn validate_wait(&self, value: u64) -> Result<(), String> {
        if self
            .last_signal
            .as_ref()
            .is_some_and(|last| value > last.get())
        {
            return Err("Cannot wait for a future signal on the same queue".into());
        }
        self.validate_value(value)
    }
    pub(in crate::device::wgpu) fn validate_signal(
        &self,
        owner: &Arc<Device>,
        value: u64,
    ) -> Result<(), String> {
        if !Arc::ptr_eq(owner, &self.owner)
            || self
                .last_signal
                .as_ref()
                .is_none_or(|last| value <= last.get())
        {
            return Err("Timeline signal requires its producer and an increasing value".into());
        }
        self.validate_value(value)
    }
    pub(in crate::device::wgpu) fn signal_submitted(&self, value: u64) {
        self.last_signal.as_ref().unwrap().set(value);
    }
}
