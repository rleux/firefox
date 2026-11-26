/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, Weak,
};
use wgpu_core as core;
use wgpu_hal as hal;
use wgpu_types as wgt;

pub mod canvas;

static GENERATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE: Mutex<Weak<NativeContext>> = Mutex::new(Weak::new());

pub struct NativeContext {
    pub instance: Arc<core::instance::Instance>,
    pub adapter: Arc<core::instance::Adapter>,
    pub device: Arc<core::device::Device>,
    pub queue: Arc<core::device::queue::Queue>,
    failed: AtomicBool,
    generation: u64,
    wr_descriptor_heap: bool,
}

impl NativeContext {
    pub fn new(
        instance: Arc<core::instance::Instance>,
        adapter: Arc<core::instance::Adapter>,
        device: Arc<core::device::Device>,
        queue: Arc<core::device::queue::Queue>,
    ) -> Self {
        Self {
            instance,
            adapter,
            device,
            queue,
            failed: AtomicBool::new(false),
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
            wr_descriptor_heap: false,
        }
    }

    /// # Safety
    /// The native device must have been opened with the WR descriptor-heap feature callback.
    /// The source core device remains ordinary; only explicitly opened WR wrappers opt in.
    pub unsafe fn new_with_wr_descriptor_heap(
        instance: Arc<core::instance::Instance>,
        adapter: Arc<core::instance::Adapter>,
        device: Arc<core::device::Device>,
        queue: Arc<core::device::queue::Queue>,
    ) -> Self {
        let mut context = Self::new(instance, adapter, device, queue);
        context.wr_descriptor_heap = true;
        context
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_valid(&self) -> bool {
        !self.failed.load(Ordering::Acquire) && self.device.is_valid()
    }

    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    pub fn active() -> Option<Arc<Self>> {
        ACTIVE
            .lock()
            .unwrap()
            .upgrade()
            .filter(|context| context.is_valid())
    }

    pub fn publish(self: &Arc<Self>) -> Result<(), &'static str> {
        let mut active = ACTIVE.lock().unwrap();
        if active.upgrade().is_some() {
            return Err("A shared Vulkan context already exists");
        }
        *active = Arc::downgrade(self);
        Ok(())
    }

    pub fn instance_with_policy(
        &self,
        mut descriptor: wgt::InstanceDescriptor,
        telemetry: Option<hal::Telemetry>,
    ) -> Arc<core::instance::Instance> {
        descriptor.backends = wgt::Backends::VULKAN;
        let mut builder =
            core::instance::InstanceBuilder::new("Shared WebGPU", descriptor, telemetry);
        let raw = unsafe { self.instance.as_hal::<hal::api::Vulkan>() }
            .unwrap()
            .clone();
        builder.try_add_custom_hal(hal::api::Vulkan, |_| Ok(raw));
        builder.build()
    }

    pub fn matches_device(&self, device: &Arc<core::device::Device>) -> bool {
        unsafe {
            let Some(raw) = device.clone().as_hal::<hal::api::Vulkan>() else {
                return false;
            };
            let source = self.device.clone().as_hal::<hal::api::Vulkan>().unwrap();
            raw.raw_device().handle() == source.raw_device().handle()
        }
    }

    pub fn matches_adapter(&self, adapter: &Arc<core::instance::Adapter>) -> bool {
        unsafe {
            let Some(raw) = adapter.clone().as_hal::<hal::api::Vulkan>() else {
                return false;
            };
            let source = self.device.clone().as_hal::<hal::api::Vulkan>().unwrap();
            raw.raw_physical_device() == source.raw_physical_device()
                && std::ptr::eq(raw.shared_instance(), source.shared_instance())
        }
    }

    pub fn open(
        &self,
        adapter: &Arc<core::instance::Adapter>,
        descriptor: &core::device::DeviceDescriptor,
    ) -> Result<
        (Arc<core::device::Device>, Arc<core::device::queue::Queue>),
        core::instance::RequestDeviceError,
    > {
        if !self.is_valid() {
            return Err(core::device::DeviceError::Lost.into());
        }
        open_device(adapter, &self.device, &self.queue, descriptor)
    }

    /// Open a renderer wrapper, optionally using private descriptor-heap layouts.
    ///
    /// # Safety
    /// When requesting heaps, the caller must restrict heap-only bind groups to
    /// compatible single-group WR render pipelines with passthrough shaders,
    /// no dynamic offsets or immediates, and no compute or bundle use.
    pub unsafe fn open_for_webrender(
        &self,
        adapter: &Arc<core::instance::Adapter>,
        descriptor: &core::device::DeviceDescriptor,
        descriptor_heap: bool,
    ) -> Result<
        (Arc<core::device::Device>, Arc<core::device::queue::Queue>),
        core::instance::RequestDeviceError,
    > {
        if !self.is_valid() {
            return Err(core::device::DeviceError::Lost.into());
        }
        open_device_inner(
            adapter,
            &self.device,
            &self.queue,
            descriptor,
            descriptor_heap && self.wr_descriptor_heap,
        )
    }
}

pub fn open_device(
    adapter: &Arc<core::instance::Adapter>,
    source: &Arc<core::device::Device>,
    source_queue: &Arc<core::device::queue::Queue>,
    descriptor: &core::device::DeviceDescriptor,
) -> Result<
    (Arc<core::device::Device>, Arc<core::device::queue::Queue>),
    core::instance::RequestDeviceError,
> {
    open_device_inner(adapter, source, source_queue, descriptor, false)
}

fn open_device_inner(
    adapter: &Arc<core::instance::Adapter>,
    source: &Arc<core::device::Device>,
    source_queue: &Arc<core::device::queue::Queue>,
    descriptor: &core::device::DeviceDescriptor,
    descriptor_heap: bool,
) -> Result<
    (Arc<core::device::Device>, Arc<core::device::queue::Queue>),
    core::instance::RequestDeviceError,
> {
    use core::device::DeviceError;
    source.check_is_valid()?;
    let mut descriptor = descriptor.clone();
    adapter.validate_device_descriptor(&mut descriptor)?;
    let open = unsafe {
        let adapter = adapter
            .clone()
            .as_hal::<hal::api::Vulkan>()
            .ok_or(DeviceError::Lost)?;
        let device = source
            .clone()
            .as_hal::<hal::api::Vulkan>()
            .ok_or(DeviceError::Lost)?;
        let queue = source_queue
            .clone()
            .as_hal::<hal::api::Vulkan>()
            .ok_or(DeviceError::Lost)?;
        adapter.open_shared(
            &device,
            &queue,
            descriptor.required_features,
            &descriptor.required_limits,
            &descriptor.memory_hints,
        )
    }
    .map_err(DeviceError::from_hal)?;
    if descriptor_heap {
        // The private context capability proves feature initialization. Activate before core
        // creates Queue::PendingWrites, whose encoder is later recycled into user rendering.
        unsafe { open.device.enable_wr_descriptor_heap() }.map_err(DeviceError::from_hal)?;
    }
    // HAL checked native identity, enabled features and limits; core keeps an independent device identity.
    unsafe { adapter.create_device_and_queue_from_hal(open.into(), &descriptor) }
}
