/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::resources::Owned;
use super::super::submission::SubmissionSync;
use super::*;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;
type V = hal::api::Vulkan;

pub struct SyncFile(Option<OwnedFd>);
impl SyncFile {
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self(Some(fd))
    }
    pub fn already_signaled() -> Self {
        Self(None)
    }
    pub fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.0.as_ref().map(AsFd::as_fd)
    }
    pub fn into_fd(self) -> Option<OwnedFd> {
        self.0
    }
}

fn semaphore(owner: &Rc<Device<V>>, export: bool) -> Result<Owned<V, vk::Semaphore>> {
    let mut external = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let mut info = vk::SemaphoreCreateInfo::default();
    if export {
        info = info.push_next(&mut external);
    }
    let raw = unsafe { owner.open.device.raw_device().create_semaphore(&info, None) }
        .map_err(|e| format!("Creating external semaphore: {e:?}"))?;
    Ok(Owned::new(owner, raw, |device, sem| unsafe {
        device.raw_device().destroy_semaphore(sem, None)
    }))
}

pub(super) struct TransferSync {
    wait: Option<Owned<V, vk::Semaphore>>,
    signal: Owned<V, vk::Semaphore>,
}
impl SubmissionSync<V> for TransferSync {
    fn stage(&self, queue: &hal::vulkan::Queue) {
        if let Some(wait) = &self.wait {
            queue.add_wait_semaphore(**wait, None, vk::PipelineStageFlags::ALL_COMMANDS);
        }
        queue.add_signal_semaphore(*self.signal, None);
    }
    fn unstage(&self, queue: &hal::vulkan::Queue) {
        if let Some(wait) = &self.wait {
            queue.remove_wait_semaphore(**wait);
        }
        queue.remove_signal_semaphore(*self.signal);
    }
}

impl TransferSync {
    pub(super) fn new(owner: &Rc<Device<V>>, ready: Option<&SyncFile>) -> Result<Rc<Self>> {
        let wait = if let Some(ready) = ready {
            let wait = semaphore(owner, false)?;
            let fd = ready
                .0
                .as_ref()
                .map(|fd| fd.try_clone())
                .transpose()
                .map_err(|e| e.to_string())?;
            let extension = ash::khr::external_semaphore_fd::Device::new(
                owner.open.device.shared_instance().raw_instance(),
                owner.open.device.raw_device(),
            );
            unsafe {
                extension.import_semaphore_fd(
                    &vk::ImportSemaphoreFdInfoKHR::default()
                        .semaphore(*wait)
                        .flags(vk::SemaphoreImportFlags::TEMPORARY)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                        .fd(fd.as_ref().map_or(-1, AsRawFd::as_raw_fd)),
                )
            }
            .map_err(|e| format!("Importing sync-file: {e:?}"))?;
            if let Some(fd) = fd {
                let _ = fd.into_raw_fd();
            }
            Some(wait)
        } else {
            None
        };
        Ok(Rc::new(Self {
            wait,
            signal: semaphore(owner, true)?,
        }))
    }
    pub(super) fn receipt(&self, owner: &Device<V>) -> Result<SyncFile> {
        let extension = ash::khr::external_semaphore_fd::Device::new(
            owner.open.device.shared_instance().raw_instance(),
            owner.open.device.raw_device(),
        );
        let fd = unsafe {
            extension.get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(*self.signal)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            )
        }
        .map_err(|e| {
            owner.lost.set(true);
            format!("Exporting release sync-file: {e:?}")
        })?;
        Ok(SyncFile(if fd == -1 {
            None
        } else {
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        }))
    }
}

impl ExternalImageDevice {
    pub(super) fn sync_file_producer(&self) -> Result<&Producer<V>> {
        let producer = self
            .0
            .as_any()
            .downcast_ref::<Producer<V>>()
            .ok_or("Sync-file sharing requires Vulkan")?;
        producer.ensure_healthy()?;
        Ok(producer)
    }
}
