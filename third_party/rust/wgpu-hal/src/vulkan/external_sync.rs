use super::semaphore_list::SemaphoreType;
use crate::external_sync::{DeviceProperties, SemaphoreValue, SubmissionSync, SyncError};
use alloc::{format, sync::Arc, vec::Vec};
use ash::{khr, vk};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use wgpu_sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Timeline,
    SyncFile,
}

pub(crate) struct Semaphore {
    pub(super) device: Arc<super::DeviceShared>,
    pub(super) raw: vk::Semaphore,
    kind: Kind,
    producer: bool,
    exportable: bool,
    claimed: AtomicBool,
    last_signal: Mutex<u64>,
    max_difference: u64,
}

impl fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Semaphore")
            .field("kind", &self.kind)
            .field("producer", &self.producer)
            .finish()
    }
}
crate::impl_dyn_resource!(Semaphore);
impl crate::external_sync::Semaphore for Semaphore {}
impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe { self.device.raw.destroy_semaphore(self.raw, None) };
    }
}

pub(crate) fn properties(device: &super::Device) -> Result<DeviceProperties, SyncError> {
    properties_shared(&device.shared)
}
fn properties_shared(device: &super::DeviceShared) -> Result<DeviceProperties, SyncError> {
    if device.instance.instance_api_version < vk::API_VERSION_1_1 {
        return Err(SyncError::Unsupported);
    }
    let mut ids = vk::PhysicalDeviceIDProperties::default();
    let mut timeline = vk::PhysicalDeviceTimelineSemaphoreProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
    if device.private_caps.timeline_semaphores {
        properties = properties.push_next(&mut timeline);
    }
    unsafe {
        device
            .instance
            .raw
            .get_physical_device_properties2(device.physical_device, &mut properties);
    }
    Ok(DeviceProperties {
        device_uuid: ids.device_uuid,
        driver_uuid: ids.driver_uuid,
        max_timeline_value_difference: timeline.max_timeline_semaphore_value_difference,
    })
}

fn fd_capability(
    device: &super::DeviceShared,
    kind: vk::ExternalSemaphoreHandleTypeFlags,
    timeline: bool,
    required: vk::ExternalSemaphoreFeatureFlags,
) -> bool {
    if device.instance.instance_api_version < vk::API_VERSION_1_1
        || !device
            .enabled_extensions
            .contains(&khr::external_semaphore_fd::NAME)
    {
        return false;
    }
    let mut semaphore_type =
        vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
    let info = vk::PhysicalDeviceExternalSemaphoreInfo::default().handle_type(kind);
    let info = if timeline {
        info.push_next(&mut semaphore_type)
    } else {
        info
    };
    let mut properties = vk::ExternalSemaphoreProperties::default();
    unsafe {
        device
            .instance
            .raw
            .get_physical_device_external_semaphore_properties(
                device.physical_device,
                &info,
                &mut properties,
            );
    }
    properties.external_semaphore_features.contains(required)
        && properties.compatible_handle_types.contains(kind)
}

pub(super) fn create(
    device: &super::Device,
    producer: bool,
    exportable: bool,
) -> Result<Arc<Semaphore>, SyncError> {
    if !device.shared.private_caps.timeline_semaphores {
        return Err(SyncError::Unsupported);
    }
    if exportable
        && !fd_capability(
            &device.shared,
            vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD,
            true,
            vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
        )
    {
        return Err(SyncError::Unsupported);
    }
    let max_difference = properties(device)?.max_timeline_value_difference;
    let mut timeline =
        vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
    let mut export = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
    let info = vk::SemaphoreCreateInfo::default().push_next(&mut timeline);
    let info = if exportable {
        info.push_next(&mut export)
    } else {
        info
    };
    let raw = unsafe { device.shared.raw.create_semaphore(&info, None) }
        .map_err(|error| SyncError::Operation(format!("Creating timeline: {error:?}")))?;
    Ok(Arc::new(Semaphore {
        device: device.shared.clone(),
        raw,
        kind: Kind::Timeline,
        producer,
        exportable,
        claimed: AtomicBool::new(false),
        last_signal: Mutex::new(0),
        max_difference,
    }))
}

pub(crate) fn create_timeline(
    device: &super::Device,
    exportable: bool,
) -> Result<Arc<dyn crate::external_sync::Semaphore>, SyncError> {
    Ok(create(device, true, exportable)?)
}

impl Semaphore {
    pub(crate) fn current_value(&self) -> Result<u64, SyncError> {
        if self.kind != Kind::Timeline {
            return Err(SyncError::InvalidValue);
        }
        super::Fence::TimelineSemaphore(self.raw)
            .get_latest(
                &self.device.raw,
                self.device.extension_fns.timeline_semaphore.as_ref(),
            )
            .map_err(|error| SyncError::Operation(format!("Querying timeline: {error:?}")))
    }

    #[cfg(unix)]
    pub(crate) fn export(&self) -> Result<crate::external_sync::TimelineHandle, SyncError> {
        use std::os::fd::FromRawFd;
        if !self.producer || !self.exportable {
            return Err(SyncError::InvalidValue);
        }
        let properties = properties_shared(&self.device)?;
        let extension =
            khr::external_semaphore_fd::Device::new(&self.device.instance.raw, &self.device.raw);
        let fd = unsafe {
            extension.get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(self.raw)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
            )
        }
        .map_err(|error| SyncError::Operation(format!("Exporting timeline: {error:?}")))?;
        Ok(crate::external_sync::TimelineHandle {
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) },
            device_uuid: properties.device_uuid,
            driver_uuid: properties.driver_uuid,
        })
    }
}

#[cfg(unix)]
pub(crate) fn import_timeline(
    device: &super::Device,
    handle: &crate::external_sync::TimelineHandle,
) -> Result<Arc<dyn crate::external_sync::Semaphore>, SyncError> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let properties = properties(device)?;
    if properties.device_uuid != handle.device_uuid || properties.driver_uuid != handle.driver_uuid
    {
        return Err(SyncError::IdentityMismatch);
    }
    if !fd_capability(
        &device.shared,
        vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD,
        true,
        vk::ExternalSemaphoreFeatureFlags::IMPORTABLE,
    ) {
        return Err(SyncError::Unsupported);
    }
    let semaphore = create(device, false, false)?;
    let fd = handle
        .fd
        .try_clone()
        .map_err(|error| SyncError::Operation(format!("Duplicating timeline FD: {error}")))?;
    let extension =
        khr::external_semaphore_fd::Device::new(&device.shared.instance.raw, &device.shared.raw);
    unsafe {
        extension.import_semaphore_fd(
            &vk::ImportSemaphoreFdInfoKHR::default()
                .semaphore(semaphore.raw)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                .fd(fd.as_raw_fd()),
        )
    }
    .map_err(|error| SyncError::Operation(format!("Importing timeline: {error:?}")))?;
    let _ = fd.into_raw_fd();
    Ok(semaphore)
}

#[cfg(unix)]
pub(crate) fn supports_sync_file_import(device: &super::Device) -> bool {
    fd_capability(
        &device.shared,
        vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD,
        false,
        vk::ExternalSemaphoreFeatureFlags::IMPORTABLE,
    )
}

#[cfg(unix)]
pub(crate) unsafe fn import_sync_file(
    device: &super::Device,
    fd: std::os::fd::BorrowedFd<'_>,
) -> Result<Arc<dyn crate::external_sync::Semaphore>, SyncError> {
    if !supports_sync_file_import(device) {
        return Err(SyncError::Unsupported);
    }
    let fd = fd
        .try_clone_to_owned()
        .map_err(|error| SyncError::Operation(format!("Duplicating sync-file: {error}")))?;
    unsafe { import_sync_file_payload(device, Some(fd)) }
}

#[cfg(all(unix, test))]
pub(super) unsafe fn import_signaled_sync_file(
    device: &super::Device,
) -> Result<Arc<dyn crate::external_sync::Semaphore>, SyncError> {
    if !supports_sync_file_import(device) {
        return Err(SyncError::Unsupported);
    }
    unsafe { import_sync_file_payload(device, None) }
}

#[cfg(unix)]
unsafe fn import_sync_file_payload(
    device: &super::Device,
    fd: Option<std::os::fd::OwnedFd>,
) -> Result<Arc<dyn crate::external_sync::Semaphore>, SyncError> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let raw = unsafe {
        device
            .shared
            .raw
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
    }
    .map_err(|error| SyncError::Operation(format!("Creating sync-file semaphore: {error:?}")))?;
    let semaphore = Arc::new(Semaphore {
        device: device.shared.clone(),
        raw,
        kind: Kind::SyncFile,
        producer: false,
        exportable: false,
        claimed: AtomicBool::new(false),
        last_signal: Mutex::new(0),
        max_difference: 0,
    });
    let extension =
        khr::external_semaphore_fd::Device::new(&device.shared.instance.raw, &device.shared.raw);
    unsafe {
        extension.import_semaphore_fd(
            &vk::ImportSemaphoreFdInfoKHR::default()
                .semaphore(raw)
                .flags(vk::SemaphoreImportFlags::TEMPORARY)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                .fd(fd.as_ref().map_or(-1, AsRawFd::as_raw_fd)),
        )
    }
    .map_err(|error| SyncError::Operation(format!("Importing sync-file semaphore: {error:?}")))?;
    if let Some(fd) = fd {
        let _ = fd.into_raw_fd();
    }
    Ok(semaphore)
}

pub(super) struct Prepared {
    pub(super) waits: Vec<SemaphoreType>,
    pub(super) signals: Vec<SemaphoreType>,
    pub(super) leases: SubmissionSync,
    reserved: Vec<Arc<dyn crate::external_sync::Semaphore>>,
    attempted: bool,
}
impl Prepared {
    pub(super) fn reserve(&mut self) -> Result<(), SyncError> {
        for wait in &self.leases.waits {
            let semaphore = wait.semaphore.as_any().downcast_ref::<Semaphore>().unwrap();
            if semaphore.kind == Kind::SyncFile {
                if semaphore
                    .claimed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Err(SyncError::InvalidValue);
                }
                self.reserved.push(wait.semaphore.clone());
            }
        }
        Ok(())
    }
    pub(super) fn attempted(&mut self) {
        self.attempted = true;
    }
    pub(super) fn accepted(&self) {
        for signal in &self.leases.signals {
            let semaphore = signal
                .semaphore
                .as_any()
                .downcast_ref::<Semaphore>()
                .unwrap();
            *semaphore.last_signal.lock() = signal.value.unwrap();
        }
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.attempted {
            for reserved in &self.reserved {
                reserved
                    .as_any()
                    .downcast_ref::<Semaphore>()
                    .unwrap()
                    .claimed
                    .store(false, Ordering::Release);
            }
        }
    }
}

pub(super) fn prepare(
    queue: &super::Queue,
    synchronization: &SubmissionSync,
    signal_floor: &mut Vec<(vk::Semaphore, u64)>,
    binary_waits: &mut Vec<vk::Semaphore>,
) -> Result<Prepared, SyncError> {
    let mut waits = Vec::new();
    let mut signals = Vec::new();
    let mut signal_dependencies = Vec::new();
    for (entries, output, signalling) in [
        (&synchronization.waits, &mut waits, false),
        (&synchronization.signals, &mut signals, true),
    ] {
        let mut seen = Vec::new();
        for SemaphoreValue { semaphore, value } in entries {
            let semaphore = semaphore
                .as_any()
                .downcast_ref::<Semaphore>()
                .ok_or(SyncError::Unsupported)?;
            if semaphore.device.raw.handle() != queue.device.raw.handle() {
                return Err(SyncError::DeviceMismatch);
            }
            if seen.contains(&semaphore.raw) {
                return Err(SyncError::InvalidValue);
            }
            seen.push(semaphore.raw);
            match (semaphore.kind, value) {
                (Kind::SyncFile, None) if !signalling => {
                    if semaphore.claimed.load(Ordering::Acquire)
                        || binary_waits.contains(&semaphore.raw)
                    {
                        return Err(SyncError::InvalidValue);
                    }
                    binary_waits.push(semaphore.raw);
                    output.push(SemaphoreType::Binary(semaphore.raw));
                }
                (Kind::Timeline, Some(value)) => {
                    let current = semaphore.current_value()?;
                    if value.saturating_sub(current) > semaphore.max_difference {
                        return Err(SyncError::InvalidValue);
                    }
                    if !signalling && semaphore.producer {
                        let prior = signal_floor
                            .iter()
                            .find(|(raw, _)| *raw == semaphore.raw)
                            .map_or(*semaphore.last_signal.lock(), |(_, value)| *value);
                        if *value > prior {
                            return Err(SyncError::InvalidValue);
                        }
                    }
                    if signalling {
                        if !semaphore.producer {
                            return Err(SyncError::InvalidValue);
                        }
                        let previous = signal_floor
                            .iter()
                            .find(|(raw, _)| *raw == semaphore.raw)
                            .map_or_else(
                                || (*semaphore.last_signal.lock()).max(current),
                                |(_, value)| *value,
                            );
                        if previous != 0 {
                            signal_dependencies.push((semaphore.raw, previous));
                        }
                        let floor = signal_floor
                            .iter_mut()
                            .find(|(raw, _)| *raw == semaphore.raw);
                        if let Some((_, previous)) = floor {
                            if *value <= *previous {
                                return Err(SyncError::InvalidValue);
                            }
                            *previous = *value;
                        } else {
                            if *value <= current || *value <= *semaphore.last_signal.lock() {
                                return Err(SyncError::InvalidValue);
                            }
                            signal_floor.push((semaphore.raw, *value));
                        }
                    }
                    output.push(SemaphoreType::Timeline(semaphore.raw, *value));
                }
                _ => return Err(SyncError::InvalidValue),
            }
        }
    }
    for (raw, value) in signal_dependencies {
        if let Some(SemaphoreType::Timeline(_, previous)) = waits
            .iter_mut()
            .find(|wait| matches!(wait, SemaphoreType::Timeline(existing, _) if *existing == raw))
        {
            *previous = (*previous).max(value);
        } else {
            waits.push(SemaphoreType::Timeline(raw, value));
        }
    }
    Ok(Prepared {
        waits,
        signals,
        leases: synchronization.clone(),
        reserved: Vec::new(),
        attempted: false,
    })
}

pub(crate) unsafe fn submit_legacy(
    queue: &super::Queue,
    buffers: &[&dyn crate::DynCommandBuffer],
    surfaces: &[&dyn crate::DynSurfaceTexture],
    terminal: (&dyn crate::DynFence, crate::FenceValue),
    synchronization: &SubmissionSync,
) -> Result<(), crate::DeviceError> {
    let bad = || crate::DeviceError::Unexpected;
    if queue.device.shared_queue_group.is_some() {
        return Err(bad());
    }
    let buffers = buffers
        .iter()
        .map(|buffer| {
            buffer
                .as_any()
                .downcast_ref::<super::CommandBuffer>()
                .ok_or_else(bad)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let surfaces = surfaces
        .iter()
        .map(|surface| {
            surface
                .as_any()
                .downcast_ref::<super::SurfaceTexture>()
                .ok_or_else(bad)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let fence = terminal
        .0
        .as_any()
        .downcast_ref::<super::Fence>()
        .ok_or_else(bad)?;
    let mut prepared =
        prepare(queue, synchronization, &mut Vec::new(), &mut Vec::new()).map_err(|_| bad())?;
    prepared.reserve().map_err(|_| bad())?;
    prepared.attempted();
    unsafe {
        queue.submit_impl(
            &buffers,
            &surfaces,
            Some((fence, terminal.1)),
            &prepared.waits,
            &prepared.signals,
            &[],
        )
    }?;
    prepared.accepted();
    Ok(())
}
