//! Owned semaphore dependencies attached to one native submission.

use crate::{DeviceError, DynDevice, DynQueue, DynResource};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt;

pub trait Semaphore: DynResource + fmt::Debug {}

#[derive(Clone, Debug)]
pub struct SemaphoreValue {
    pub semaphore: Arc<dyn Semaphore>,
    pub value: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct SubmissionSync {
    pub waits: Vec<SemaphoreValue>,
    pub signals: Vec<SemaphoreValue>,
}

impl SubmissionSync {
    pub fn is_empty(&self) -> bool {
        self.waits.is_empty() && self.signals.is_empty()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceProperties {
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    pub max_timeline_value_difference: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Semaphore operation is unavailable for this device")]
    Unsupported,
    #[error("Semaphore belongs to another native device")]
    DeviceMismatch,
    #[error("Invalid semaphore value or producer/consumer operation")]
    InvalidValue,
    #[error("Timeline device or driver identity mismatch")]
    IdentityMismatch,
    #[error("{0}")]
    Operation(String),
}

#[cfg(all(unix, vulkan))]
#[derive(Debug)]
pub struct TimelineHandle {
    pub(crate) fd: std::os::fd::OwnedFd,
    pub(crate) device_uuid: [u8; 16],
    pub(crate) driver_uuid: [u8; 16],
}

#[cfg(all(unix, vulkan))]
impl TimelineHandle {
    /// # Safety
    /// The FD must be an OPAQUE_FD timeline export with zero creation flags.
    /// Supplied identity metadata is checked on import. The original producer
    /// remains the sole signaller.
    pub unsafe fn from_fd(
        fd: std::os::fd::OwnedFd,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
    ) -> Self {
        Self {
            fd,
            device_uuid,
            driver_uuid,
        }
    }
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }
    pub fn into_parts(self) -> (std::os::fd::OwnedFd, [u8; 16], [u8; 16]) {
        (self.fd, self.device_uuid, self.driver_uuid)
    }
}

pub fn properties(device: &dyn DynDevice) -> Result<DeviceProperties, SyncError> {
    #[cfg(vulkan)]
    if let Some(device) = device.as_any().downcast_ref::<crate::vulkan::Device>() {
        return crate::vulkan::external_sync::properties(device);
    }
    let _ = device;
    Err(SyncError::Unsupported)
}

pub fn create_timeline(
    device: &dyn DynDevice,
    exportable: bool,
) -> Result<Arc<dyn Semaphore>, SyncError> {
    #[cfg(vulkan)]
    if let Some(device) = device.as_any().downcast_ref::<crate::vulkan::Device>() {
        return crate::vulkan::external_sync::create_timeline(device, exportable);
    }
    let _ = (device, exportable);
    Err(SyncError::Unsupported)
}

pub fn current_value(semaphore: &dyn Semaphore) -> Result<u64, SyncError> {
    #[cfg(vulkan)]
    if let Some(semaphore) = semaphore
        .as_any()
        .downcast_ref::<crate::vulkan::external_sync::Semaphore>()
    {
        return semaphore.current_value();
    }
    let _ = semaphore;
    Err(SyncError::Unsupported)
}

#[cfg(all(unix, vulkan))]
pub fn export_timeline(semaphore: &dyn Semaphore) -> Result<TimelineHandle, SyncError> {
    let semaphore = semaphore
        .as_any()
        .downcast_ref::<crate::vulkan::external_sync::Semaphore>()
        .ok_or(SyncError::Unsupported)?;
    semaphore.export()
}

#[cfg(all(unix, vulkan))]
pub fn import_timeline(
    device: &dyn DynDevice,
    handle: &TimelineHandle,
) -> Result<Arc<dyn Semaphore>, SyncError> {
    let device = device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .ok_or(SyncError::Unsupported)?;
    crate::vulkan::external_sync::import_timeline(device, handle)
}

#[cfg(all(unix, vulkan))]
pub fn supports_sync_file_import(device: &dyn DynDevice) -> bool {
    device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .is_some_and(crate::vulkan::external_sync::supports_sync_file_import)
}

/// # Safety
/// The FD must be a genuine compatible SYNC_FD semaphore export or fence.
#[cfg(all(unix, vulkan))]
pub unsafe fn import_sync_file(
    device: &dyn DynDevice,
    fd: std::os::fd::BorrowedFd<'_>,
) -> Result<Arc<dyn Semaphore>, SyncError> {
    let device = device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .ok_or(SyncError::Unsupported)?;
    unsafe { crate::vulkan::external_sync::import_sync_file(device, fd) }
}

/// Submit legacy work with dependencies scoped to this call.
///
/// # Safety
/// Queue::submit requirements apply. Dependencies must obey their single-producer
/// and one-shot rules and remain alive until native retirement.
pub unsafe fn submit_legacy(
    queue: &dyn DynQueue,
    buffers: &[&dyn crate::DynCommandBuffer],
    surfaces: &[&dyn crate::DynSurfaceTexture],
    terminal: (&dyn crate::DynFence, crate::FenceValue),
    synchronization: &SubmissionSync,
) -> Result<(), DeviceError> {
    #[cfg(vulkan)]
    if let Some(queue) = queue.as_any().downcast_ref::<crate::vulkan::Queue>() {
        return unsafe {
            crate::vulkan::external_sync::submit_legacy(
                queue,
                buffers,
                surfaces,
                terminal,
                synchronization,
            )
        };
    }
    let _ = (queue, buffers, surfaces, terminal, synchronization);
    Err(DeviceError::Unexpected)
}
