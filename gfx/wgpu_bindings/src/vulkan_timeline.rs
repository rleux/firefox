/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::{khr, vk};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::fd::{BorrowedFd, IntoRawFd};
use std::ptr;
use std::sync::{Arc, Mutex};

use crate::server::Global;
use crate::FfiSlice;
use wgc::resource::ParentDevice;
use wgpu_core_remote_types::id;

#[repr(C)]
pub struct VulkanTimelineDescriptor {
    pub fd: i32,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

pub struct VulkanTimeline {
    inner: Arc<TimelineSemaphore>,
    last_signal: Cell<u64>,
    last_submitted: Cell<u64>,
}

struct TimelineSemaphore {
    device: Arc<wgc::device::Device>,
    semaphore: vk::Semaphore,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    exportable: bool,
    max_difference: u64,
    pending: Mutex<BTreeMap<u64, usize>>,
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
            let mut limits = vk::PhysicalDeviceTimelineSemaphoreProperties::default();
            let mut properties = vk::PhysicalDeviceProperties2::default()
                .push_next(&mut ids)
                .push_next(&mut limits);
            instance.get_physical_device_properties2(physical, &mut properties);
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
            let mut info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
            if exportable {
                info = info.push_next(&mut export);
            }
            let semaphore = hal.raw_device().create_semaphore(&info, None).ok()?;
            Some(Self {
                inner: Arc::new(TimelineSemaphore {
                    device,
                    semaphore,
                    device_uuid: ids.device_uuid,
                    driver_uuid: ids.driver_uuid,
                    exportable,
                    max_difference: limits.max_timeline_semaphore_value_difference,
                    pending: Mutex::new(BTreeMap::new()),
                }),
                last_signal: Cell::new(0),
                last_submitted: Cell::new(0),
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
        if descriptor.device_uuid != timeline.inner.device_uuid
            || descriptor.driver_uuid != timeline.inner.driver_uuid
        {
            return None;
        }
        let fd = BorrowedFd::borrow_raw(descriptor.fd)
            .try_clone_to_owned()
            .ok()?;
        let hal = timeline.inner.device.clone().as_hal::<wgc::api::Vulkan>()?;
        let extension = khr::external_semaphore_fd::Device::new(
            hal.shared_instance().raw_instance(),
            hal.raw_device(),
        );
        use std::os::fd::AsRawFd;
        extension
            .import_semaphore_fd(
                &vk::ImportSemaphoreFdInfoKHR::default()
                    .semaphore(timeline.inner.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                    .fd(fd.as_raw_fd()),
            )
            .ok()?;
        let _ = fd.into_raw_fd();
        Some(timeline)
    }

    pub fn current_value(&self) -> Option<u64> {
        let hal = unsafe { self.inner.device.clone().as_hal::<wgc::api::Vulkan>() }?;
        unsafe {
            if hal
                .enabled_device_extensions()
                .contains(&khr::timeline_semaphore::NAME)
            {
                khr::timeline_semaphore::Device::new(
                    hal.shared_instance().raw_instance(),
                    hal.raw_device(),
                )
                .get_semaphore_counter_value(self.inner.semaphore)
            } else {
                hal.raw_device()
                    .get_semaphore_counter_value(self.inner.semaphore)
            }
        }
        .ok()
    }

    /// The returned descriptor transfers ownership of its FD.
    pub fn export(&self) -> Option<VulkanTimelineDescriptor> {
        if !self.inner.exportable || self.inner.device.check_is_valid().is_err() {
            return None;
        }
        let hal = unsafe { self.inner.device.clone().as_hal::<wgc::api::Vulkan>() }?;
        let extension = khr::external_semaphore_fd::Device::new(
            hal.shared_instance().raw_instance(),
            hal.raw_device(),
        );
        let fd = unsafe {
            extension
                .get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default()
                        .semaphore(self.inner.semaphore)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
                )
                .ok()?
        };
        Some(VulkanTimelineDescriptor {
            fd,
            device_uuid: self.inner.device_uuid,
            driver_uuid: self.inner.driver_uuid,
        })
    }
}

impl Drop for TimelineSemaphore {
    fn drop(&mut self) {
        let hal = unsafe { self.device.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
        unsafe { hal.raw_device().destroy_semaphore(self.semaphore, None) };
    }
}

#[repr(C)]
pub struct VulkanTimelinePoint<'a> {
    pub timeline: Option<&'a VulkanTimeline>,
    pub value: u64,
}

struct PendingPoint {
    semaphore: Arc<TimelineSemaphore>,
    value: u64,
}

impl PendingPoint {
    fn new(timeline: &VulkanTimeline, value: u64) -> Option<Self> {
        let mut pending = timeline.inner.pending.lock().unwrap();
        if pending
            .first_key_value()
            .into_iter()
            .chain(pending.last_key_value())
            .any(|(other, _)| value.abs_diff(*other) > timeline.inner.max_difference)
        {
            return None;
        }
        *pending.entry(value).or_default() += 1;
        Some(Self {
            semaphore: timeline.inner.clone(),
            value,
        })
    }
}

impl Drop for PendingPoint {
    fn drop(&mut self) {
        let mut pending = self.semaphore.pending.lock().unwrap();
        let count = pending.get_mut(&self.value).unwrap();
        *count -= 1;
        if *count == 0 {
            pending.remove(&self.value);
        }
    }
}

fn coalesce<'a>(points: &[VulkanTimelinePoint<'a>]) -> Option<Vec<(&'a VulkanTimeline, u64)>> {
    let mut result: Vec<(&VulkanTimeline, u64)> = Vec::new();
    for point in points {
        let timeline = point.timeline?;
        if point.value == 0 {
            return None;
        }
        if let Some((_, value)) = result
            .iter_mut()
            .find(|(other, _)| Arc::ptr_eq(&other.inner, &timeline.inner))
        {
            *value = (*value).max(point.value);
        } else {
            result.push((timeline, point.value));
        }
    }
    Some(result)
}

fn valid_point(
    device: &Arc<wgc::device::Device>,
    timeline: &VulkanTimeline,
    value: u64,
    signal: bool,
) -> bool {
    if !Arc::ptr_eq(device, &timeline.inner.device)
        || (signal && (!timeline.inner.exportable || value <= timeline.last_signal.get()))
        || (!signal && timeline.inner.exportable && value > timeline.last_submitted.get())
    {
        return false;
    }
    timeline.current_value().is_some_and(|current| {
        (!signal || value > current) && value.abs_diff(current) <= timeline.inner.max_difference
    })
}

/// # Safety
/// The device and its queue must not be accessed concurrently or reentrantly
/// during this call. Peers must respect the shared timeline's outstanding-value
/// limit and eventually signal imported waits.
pub unsafe fn submit_with_timelines(
    queue: &Arc<wgc::device::queue::Queue>,
    commands: &[Arc<wgc::command::CommandBuffer>],
    waits: &[VulkanTimelinePoint<'_>],
    signals: &[VulkanTimelinePoint<'_>],
) -> Option<u64> {
    let device = queue.device();
    device.check_is_valid().ok()?;
    let waits = coalesce(waits)?;
    let signals = coalesce(signals)?;
    if waits
        .iter()
        .any(|(timeline, value)| !valid_point(device, timeline, *value, false))
        || signals
            .iter()
            .any(|(timeline, value)| !valid_point(device, timeline, *value, true))
    {
        return None;
    }
    let hal = queue.clone().as_hal::<wgc::api::Vulkan>()?;
    let retained: Vec<_> = waits
        .iter()
        .chain(&signals)
        .map(|(timeline, value)| PendingPoint::new(timeline, *value))
        .collect::<Option<_>>()?;
    for (timeline, value) in &waits {
        hal.add_wait_semaphore(
            timeline.inner.semaphore,
            Some(*value),
            vk::PipelineStageFlags::TOP_OF_PIPE,
        );
    }
    for (timeline, value) in &signals {
        timeline.last_signal.set(*value);
        hal.add_signal_semaphore(timeline.inner.semaphore, Some(*value));
    }
    for filter in [
        wgt::error::ErrorFilter::Validation,
        wgt::error::ErrorFilter::OutOfMemory,
        wgt::error::ErrorFilter::Internal,
    ] {
        device.push_error_scope(filter);
    }
    let index = queue.submit(commands);
    let mut success = device.is_valid();
    for _ in 0..3 {
        success &= device.pop_error_scope().unwrap().is_none();
    }
    for (timeline, _) in &waits {
        success &= !hal.remove_wait_semaphore(timeline.inner.semaphore);
    }
    for (timeline, _) in &signals {
        success &= !hal.remove_signal_semaphore(timeline.inner.semaphore);
    }
    // The queue owns this closure; retain the semaphores, never the queue itself.
    queue.on_submitted_work_done(Box::new(move || drop(retained)));
    if success {
        for (timeline, value) in signals {
            timeline.last_submitted.set(value);
        }
        Some(index)
    } else {
        None
    }
}

/// # Safety
/// The device and queue must not be accessed concurrently or reentrantly during
/// this call, and referenced handles must remain valid. Peers must respect the
/// shared timeline's outstanding-value limit and eventually signal imported waits.
#[no_mangle]
pub unsafe extern "C" fn wgpu_vulkan_queue_submit(
    global: &Global,
    queue_id: id::QueueId,
    commands: FfiSlice<'_, id::CommandBufferId>,
    waits: FfiSlice<'_, VulkanTimelinePoint<'_>>,
    signals: FfiSlice<'_, VulkanTimelinePoint<'_>>,
) -> u64 {
    let commands: Vec<_> = commands
        .as_slice()
        .iter()
        .map(|id| global.resolve_command_buffer_id(*id))
        .collect();
    submit_with_timelines(
        &global.resolve_queue_id(queue_id),
        &commands,
        waits.as_slice(),
        signals.as_slice(),
    )
    .unwrap_or(0)
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
