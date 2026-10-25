use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};

use ash::vk;
use wgpu_sync::Mutex;

use super::{InstanceShared, ResourceIdentityFactory};
use crate::shared::{
    SharedDeviceContext, SharedDeviceEndpoints, SharedDeviceError, SharedDeviceIdentity,
};

pub(super) struct QueueGroup {
    pub(super) host: [Mutex<()>; 2],
    pub(super) closed: AtomicBool,
    pub(super) texture_ids: Arc<ResourceIdentityFactory<vk::Image>>,
    pub(super) view_ids: Arc<ResourceIdentityFactory<vk::ImageView>>,
    identity: SharedDeviceIdentity,
    #[cfg(test)]
    native_device_drops: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    compute_wrapper_attempts: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    compute_open_waiters: core::sync::atomic::AtomicUsize,
}

impl QueueGroup {
    fn new() -> Self {
        Self {
            host: [Mutex::new(()), Mutex::new(())],
            closed: AtomicBool::new(false),
            texture_ids: Arc::new(ResourceIdentityFactory::new()),
            view_ids: Arc::new(ResourceIdentityFactory::new()),
            identity: SharedDeviceIdentity::new(),
            #[cfg(test)]
            native_device_drops: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            compute_wrapper_attempts: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            compute_open_waiters: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn submit<T>(
        &self,
        index: usize,
        operation: impl FnOnce() -> Result<T, crate::DeviceError>,
    ) -> Result<T, crate::DeviceError> {
        let _host = self.host[index].lock();
        if self.closed.load(Ordering::Acquire) {
            return Err(crate::DeviceError::Unexpected);
        }
        let result = operation();
        if result.is_err() {
            self.closed.store(true, Ordering::Release);
        }
        result
    }

    fn wait_for_idle(&self, device: &ash::Device) -> Result<(), crate::DeviceError> {
        let _graphics = self.host[0].lock();
        let _compute = self.host[1].lock();
        unsafe { device.device_wait_idle() }.map_err(super::map_host_device_oom_and_lost_err)
    }
}

struct NativeDevice {
    raw: ash::Device,
    _instance: Arc<InstanceShared>,
    physical_device: vk::PhysicalDevice,
    extensions: Vec<&'static core::ffi::CStr>,
    features: wgt::Features,
    limits: wgt::Limits,
    queues: Arc<QueueGroup>,
}

impl fmt::Debug for NativeDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedVulkanDevice").finish_non_exhaustive()
    }
}

impl Drop for NativeDevice {
    fn drop(&mut self) {
        unsafe { self.raw.destroy_device(None) };
        #[cfg(test)]
        self.queues
            .native_device_drops
            .fetch_add(1, Ordering::SeqCst);
    }
}

impl SharedDeviceContext for NativeDevice {
    fn identity(&self) -> SharedDeviceIdentity {
        self.queues.identity.clone()
    }

    fn clone_instance(&self) -> (wgt::Backend, Box<dyn crate::DynInstance>) {
        (
            wgt::Backend::Vulkan,
            Box::new(super::Instance {
                shared: self._instance.clone(),
                selected_physical_device: Some(self.physical_device),
            }),
        )
    }

    unsafe fn open_compute_device(
        self: Arc<Self>,
        adapter: &dyn crate::DynAdapter,
        features: wgt::Features,
        limits: &wgt::Limits,
        memory_hints: &wgt::MemoryHints,
    ) -> Result<crate::DynOpenDevice, SharedDeviceError> {
        let adapter = adapter
            .as_any()
            .downcast_ref::<super::Adapter>()
            .ok_or(SharedDeviceError::IncompatibleAdapter)?;
        if !Arc::ptr_eq(&adapter.instance, &self._instance) || adapter.raw != self.physical_device {
            return Err(SharedDeviceError::IncompatibleAdapter);
        }
        if !self.features.contains(features) {
            return Err(SharedDeviceError::UnsupportedFeatures(
                features - self.features,
            ));
        }
        if !limits.check_limits(&self.limits) {
            return Err(SharedDeviceError::LimitsExceeded);
        }
        #[cfg(test)]
        self.queues
            .compute_open_waiters
            .fetch_add(1, Ordering::SeqCst);
        let _host = self.queues.host[1].lock();
        #[cfg(test)]
        self.queues
            .compute_open_waiters
            .fetch_sub(1, Ordering::SeqCst);
        if self.queues.closed.load(Ordering::Acquire) {
            return Err(SharedDeviceError::Closed);
        }
        #[cfg(test)]
        self.queues
            .compute_wrapper_attempts
            .fetch_add(1, Ordering::SeqCst);
        let lease = self.clone();
        let open = unsafe {
            adapter.device_from_raw_shared(
                self.raw.clone(),
                Some(Box::new(move || drop(lease))),
                &self.extensions,
                features,
                limits,
                memory_hints,
                0,
                1,
                Some(self.queues.clone()),
            )
        }?;
        if self.queues.closed.load(Ordering::Acquire) {
            return Err(SharedDeviceError::Closed);
        }
        Ok(open.into())
    }

    fn drain(&self) -> Result<(), crate::DeviceError> {
        self.queues.closed.store(true, Ordering::Release);
        self.queues.wait_for_idle(&self.raw)
    }
}

fn supports_independent_queues(families: &[vk::QueueFamilyProperties]) -> bool {
    families.first().is_some_and(|family| {
        family.queue_count >= 2
            && family
                .queue_flags
                .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
    })
}

pub(crate) unsafe fn open_shared_device(
    adapter: &super::Adapter,
    features: wgt::Features,
    limits: &wgt::Limits,
    memory_hints: &wgt::MemoryHints,
) -> Result<SharedDeviceEndpoints, SharedDeviceError> {
    let families = unsafe {
        adapter
            .instance
            .raw
            .get_physical_device_queue_family_properties(adapter.raw)
    };
    if !supports_independent_queues(&families) {
        return Err(SharedDeviceError::IndependentQueuesUnavailable);
    }
    let extensions = adapter.required_device_extensions(features);
    let mut device_features = adapter.physical_device_features(&extensions, features);
    let priorities = [1.0, 1.0];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&priorities)];
    let extension_names: Vec<_> = extensions.iter().map(|name| name.as_ptr()).collect();
    let info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&extension_names);
    let info = device_features.add_to_device_create(info);
    let raw = unsafe { adapter.instance.raw.create_device(adapter.raw, &info, None) }
        .map_err(super::map_host_device_oom_and_lost_err)?;
    let owner = Arc::new(NativeDevice {
        raw,
        _instance: adapter.instance.clone(),
        physical_device: adapter.raw,
        extensions: extensions.clone(),
        features,
        limits: limits.clone(),
        queues: Arc::new(QueueGroup::new()),
    });
    let open_endpoint = |index| {
        let lease = owner.clone();
        unsafe {
            adapter.device_from_raw_shared(
                owner.raw.clone(),
                Some(Box::new(move || drop(lease))),
                &extensions,
                features,
                limits,
                memory_hints,
                0,
                index,
                Some(owner.queues.clone()),
            )
        }
    };
    let graphics = open_endpoint(0)?;
    #[cfg(test)]
    tests::after_graphics(&graphics.device)?;
    let compute = open_endpoint(1)?;
    Ok(SharedDeviceEndpoints {
        context: owner,
        graphics: crate::DynOpenDevice {
            device: Box::new(graphics.device),
            queue: Box::new(graphics.queue),
        },
        compute: crate::DynOpenDevice {
            device: Box::new(compute.device),
            queue: Box::new(compute.queue),
        },
    })
}

#[cfg(test)]
mod tests;
