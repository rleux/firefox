/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, wgt, Device};
use ash::vk;
use std::cell::Cell;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;

pub struct TimelineHandle {
    fd: OwnedFd,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}

impl TimelineHandle {
    /// # Safety
    /// The FD must be an OPAQUE_FD timeline semaphore export with zero creation
    /// flags and the supplied device/driver identities. Its original producer
    /// must remain the sole signaller.
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

/// One producer signals increasing values; imported timelines can only be waited on.
pub struct SharedTimeline {
    semaphore: Owned<vk::Semaphore>,
    last_signal: Option<Cell<u64>>,
    max_difference: u64,
}

fn properties(owner: &Device) -> ([u8; 16], [u8; 16], u64) {
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut timeline = vk::PhysicalDeviceTimelineSemaphoreProperties::default();
    unsafe {
        owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_properties2(
                owner.open.device.raw_physical_device(),
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
}

fn extension(owner: &Device) -> ash::khr::external_semaphore_fd::Device {
    ash::khr::external_semaphore_fd::Device::new(
        owner.open.device.shared_instance().raw_instance(),
        owner.open.device.raw_device(),
    )
}

impl SharedTimeline {
    fn create(owner: &Rc<Device>, producer: bool) -> Result<Self, String> {
        if owner.is_lost()
            || !owner
                .features
                .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Err("External Vulkan timelines are unavailable".into());
        }
        let mut timeline =
            vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
        let mut export = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        let mut info = vk::SemaphoreCreateInfo::default().push_next(&mut timeline);
        if producer {
            info = info.push_next(&mut export);
        }
        let raw = unsafe { owner.open.device.raw_device().create_semaphore(&info, None) }
            .map_err(|error| format!("Creating shared timeline: {error:?}"))?;
        Ok(Self {
            semaphore: Owned::new(owner, raw, |device, semaphore| unsafe {
                device.raw_device().destroy_semaphore(semaphore, None);
            }),
            last_signal: producer.then(|| Cell::new(0)),
            max_difference: properties(owner).2,
        })
    }

    pub fn new(owner: &Rc<Device>) -> Result<Rc<Self>, String> {
        Self::create(owner, true).map(Rc::new)
    }

    pub fn export(&self) -> Result<TimelineHandle, String> {
        if self.last_signal.is_none() || self.semaphore.owner.is_lost() {
            return Err("Only a live timeline producer can export its handle".into());
        }
        let fd = unsafe {
            extension(&self.semaphore.owner).get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(*self.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
            )
        }
        .map_err(|error| format!("Exporting timeline: {error:?}"))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let (device_uuid, driver_uuid, _) = properties(&self.semaphore.owner);
        Ok(TimelineHandle {
            fd,
            device_uuid,
            driver_uuid,
        })
    }

    pub fn import(owner: &Rc<Device>, handle: &TimelineHandle) -> Result<Rc<Self>, String> {
        if !owner
            .features
            .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Err("External Vulkan timelines are unavailable".into());
        }
        let (device_uuid, driver_uuid, _) = properties(owner);
        if device_uuid != handle.device_uuid || driver_uuid != handle.driver_uuid {
            return Err("Timeline device or driver identity mismatch".into());
        }
        let timeline = Self::create(owner, false)?;
        let fd = handle
            .fd
            .try_clone()
            .map_err(|error| format!("Duplicating timeline FD: {error}"))?;
        unsafe {
            extension(owner).import_semaphore_fd(
                &vk::ImportSemaphoreFdInfoKHR::default()
                    .semaphore(*timeline.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                    .fd(fd.as_raw_fd()),
            )
        }
        .map_err(|error| format!("Importing timeline: {error:?}"))?;
        let _ = fd.into_raw_fd();
        Ok(Rc::new(timeline))
    }

    fn validate_value(&self, value: u64) -> Result<(), String> {
        if self.max_difference == u64::MAX {
            return Ok(());
        }
        let owner = &self.semaphore.owner;
        let current = unsafe {
            if owner
                .open
                .device
                .enabled_device_extensions()
                .contains(&ash::khr::timeline_semaphore::NAME)
            {
                ash::khr::timeline_semaphore::Device::new(
                    owner.open.device.shared_instance().raw_instance(),
                    owner.open.device.raw_device(),
                )
                .get_semaphore_counter_value(*self.semaphore)
            } else {
                owner
                    .open
                    .device
                    .raw_device()
                    .get_semaphore_counter_value(*self.semaphore)
            }
        }
        .map_err(|error| format!("Querying shared timeline: {error:?}"))?;
        if value.saturating_sub(current) > self.max_difference {
            return Err("Timeline value exceeds the device's outstanding range".into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct SubmissionSync {
    sync_files: Vec<super::SyncFileWait>,
    waits: Vec<(Rc<SharedTimeline>, u64)>,
    signals: Vec<(Rc<SharedTimeline>, u64)>,
}

fn add(entries: &mut Vec<(Rc<SharedTimeline>, u64)>, timeline: &Rc<SharedTimeline>, value: u64) {
    if let Some((_, previous)) = entries
        .iter_mut()
        .find(|(entry, _)| Rc::ptr_eq(entry, timeline))
    {
        *previous = (*previous).max(value);
    } else {
        entries.push((timeline.clone(), value));
    }
}

impl SubmissionSync {
    pub fn wait_sync_file(
        &mut self,
        owner: &Rc<Device>,
        wait: super::SyncFileWait,
    ) -> Result<(), String> {
        if !Rc::ptr_eq(owner, &wait.semaphore.owner) {
            return Err("Sync-file wait belongs to another Vulkan device".into());
        }
        self.sync_files.push(wait);
        Ok(())
    }

    pub fn wait(
        &mut self,
        owner: &Rc<Device>,
        timeline: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        if !Rc::ptr_eq(owner, &timeline.semaphore.owner) {
            return Err("Timeline must be imported into the recording's device".into());
        }
        if value != 0 {
            add(&mut self.waits, timeline, value);
        }
        Ok(())
    }

    pub fn signal(
        &mut self,
        owner: &Rc<Device>,
        timeline: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let last = timeline
            .last_signal
            .as_ref()
            .ok_or("An imported timeline cannot signal")?;
        if !Rc::ptr_eq(owner, &timeline.semaphore.owner) || value <= last.get() {
            return Err("Timeline signal requires its producer and an increasing value".into());
        }
        add(&mut self.signals, timeline, value);
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        for (timeline, value) in &self.waits {
            if timeline
                .last_signal
                .as_ref()
                .is_some_and(|last| *value > last.get())
            {
                return Err("Cannot wait for a future signal on the same Vulkan queue".into());
            }
            timeline.validate_value(*value)?;
        }
        for (timeline, value) in &self.signals {
            if *value <= timeline.last_signal.as_ref().unwrap().get() {
                return Err("Timeline signals were submitted out of order".into());
            }
            timeline.validate_value(*value)?;
        }
        Ok(())
    }

    pub fn stage<'a>(&'a self, queue: &'a hal::vulkan::Queue) -> StagedSync<'a> {
        for wait in &self.sync_files {
            queue.add_wait_semaphore(*wait.semaphore, None, vk::PipelineStageFlags::ALL_COMMANDS);
        }
        for (timeline, value) in &self.waits {
            queue.add_wait_semaphore(
                *timeline.semaphore,
                Some(*value),
                vk::PipelineStageFlags::ALL_COMMANDS,
            );
        }
        for (timeline, value) in &self.signals {
            queue.add_signal_semaphore(*timeline.semaphore, Some(*value));
        }
        StagedSync { sync: self, queue }
    }

    pub fn submitted(&self) {
        for (timeline, value) in &self.signals {
            timeline.last_signal.as_ref().unwrap().set(*value);
        }
    }

    pub fn clear(&mut self) {
        self.sync_files.clear();
        self.waits.clear();
        self.signals.clear();
    }
}

pub(super) struct StagedSync<'a> {
    sync: &'a SubmissionSync,
    queue: &'a hal::vulkan::Queue,
}

impl Drop for StagedSync<'_> {
    fn drop(&mut self) {
        for wait in &self.sync.sync_files {
            self.queue.remove_wait_semaphore(*wait.semaphore);
        }
        for (timeline, _) in &self.sync.waits {
            self.queue.remove_wait_semaphore(*timeline.semaphore);
        }
        for (timeline, _) in &self.sync.signals {
            self.queue.remove_signal_semaphore(*timeline.semaphore);
        }
    }
}

#[cfg(test)]
#[path = "timeline_tests.rs"]
mod tests;
