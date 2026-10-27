mod progress;
mod submit;
pub(super) use progress::{wait_for_idle, QueueProgress};
pub(super) use submit::submit_single;

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
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
    pub(super) uncertain: AtomicBool,
    pub(super) presentation_fences_enabled: bool,
    pub(super) orphaned_presentation: AtomicBool,
    pub(super) presentations: Mutex<Vec<(Weak<super::swapchain::Generation>, Arc<AtomicBool>)>>,
    pub(super) texture_ids: Arc<ResourceIdentityFactory<vk::Image>>,
    pub(super) view_ids: Arc<ResourceIdentityFactory<vk::ImageView>>,
    identity: SharedDeviceIdentity,
    #[cfg(test)]
    pub(super) generation_execution_error: AtomicBool,
    #[cfg(test)]
    pub(super) presentation_wait_error: AtomicBool,
    #[cfg(test)]
    pub(super) present_after_native_error: AtomicBool,
    #[cfg(test)]
    pub(super) surface_submit_waiters: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(super) surface_present_waiters: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    native_device_drops: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    compute_wrapper_attempts: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    compute_open_waiters: core::sync::atomic::AtomicUsize,
    #[cfg(test)]
    graphics_idle_waiters: core::sync::atomic::AtomicUsize,
}

impl QueueGroup {
    fn new(presentation_fences_enabled: bool) -> Self {
        Self {
            host: [Mutex::new(()), Mutex::new(())],
            closed: AtomicBool::new(false),
            uncertain: AtomicBool::new(false),
            presentation_fences_enabled,
            orphaned_presentation: AtomicBool::new(false),
            presentations: Mutex::new(Vec::new()),
            texture_ids: Arc::new(ResourceIdentityFactory::new()),
            view_ids: Arc::new(ResourceIdentityFactory::new()),
            identity: SharedDeviceIdentity::new(),
            #[cfg(test)]
            generation_execution_error: AtomicBool::new(false),
            #[cfg(test)]
            presentation_wait_error: AtomicBool::new(false),
            #[cfg(test)]
            present_after_native_error: AtomicBool::new(false),
            #[cfg(test)]
            surface_submit_waiters: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            surface_present_waiters: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            native_device_drops: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            compute_wrapper_attempts: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            compute_open_waiters: core::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            graphics_idle_waiters: core::sync::atomic::AtomicUsize::new(0),
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
            self.close_uncertain();
        }
        result
    }

    pub(super) fn close_uncertain(&self) {
        self.uncertain.store(true, Ordering::Release);
        self.closed.store(true, Ordering::Release);
    }

    pub(super) fn snapshot_generations(
        &self,
    ) -> Result<Vec<Arc<super::swapchain::Generation>>, crate::DeviceError> {
        if self.orphaned_presentation.load(Ordering::Acquire) {
            return Err(crate::DeviceError::Unexpected);
        }
        let mut generations = Vec::new();
        let mut unconfirmed = false;
        self.presentations.lock().retain(|(weak, retired)| {
            if let Some(generation) = weak.upgrade() {
                generations.push(generation);
                true
            } else if retired.load(Ordering::Acquire) {
                false
            } else {
                unconfirmed = true;
                true
            }
        });
        if unconfirmed {
            self.orphaned_presentation.store(true, Ordering::Release);
            self.close_uncertain();
            return Err(crate::DeviceError::Unexpected);
        }
        Ok(generations)
    }

    pub(super) fn wait_for_idle(&self, device: &ash::Device) -> Result<(), crate::DeviceError> {
        self.closed.store(true, Ordering::Release);
        let generations;
        let execution = {
            let _graphics = self.host[0].lock();
            let _compute = self.host[1].lock();
            generations = self.snapshot_generations()?;
            for generation in &generations {
                generation.close();
            }
            unsafe { device.device_wait_idle() }.map_err(super::map_host_device_oom_and_lost_err)
        };
        let result = execution.and_then(|()| {
            for generation in &generations {
                generation.wait_presentation()?;
            }
            Ok(())
        });
        if matches!(result, Ok(()) | Err(crate::DeviceError::Lost)) {
            for generation in &generations {
                generation.mark_retired();
            }
        }
        result
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

    fn is_closed(&self) -> bool {
        self.queues.closed.load(Ordering::Acquire)
    }

    fn clone_instance(&self) -> (wgt::Backend, Box<dyn crate::DynInstance>) {
        (
            wgt::Backend::Vulkan,
            Box::new(super::Instance {
                shared: self._instance.clone(),
                selected_physical_device: Some(self.physical_device),
                selected_features: Some(self.features),
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

    unsafe fn submit_ordered(
        &self,
        logical_compute_queue: &dyn crate::DynQueue,
        segments: &[crate::shared::SharedSubmitSegment<'_>],
        terminal: (&dyn crate::DynFence, crate::FenceValue),
    ) -> Result<Box<dyn crate::shared::SharedSubmissionReceipt>, crate::shared::SharedSubmitError>
    {
        unsafe { submit::submit_ordered(self, logical_compute_queue, segments, terminal) }
    }

    unsafe fn submit_graphics(
        &self,
        queue: &dyn crate::DynQueue,
        buffers: &[&dyn crate::DynCommandBuffer],
        surfaces: &[&dyn crate::DynSurfaceTexture],
        terminal: (&dyn crate::DynFence, crate::FenceValue),
        synchronization: &crate::external_sync::SubmissionSync,
    ) -> Result<Box<dyn crate::shared::SharedSubmissionReceipt>, crate::shared::SharedSubmitError>
    {
        unsafe {
            submit::submit_graphics(self, queue, buffers, surfaces, terminal, synchronization)
        }
    }

    fn drain(&self) -> Result<(), crate::DeviceError> {
        self.queues.closed.store(true, Ordering::Release);
        let result = self.queues.wait_for_idle(&self.raw);
        self.queues.uncertain.store(
            !matches!(result, Ok(()) | Err(crate::DeviceError::Lost)),
            Ordering::Release,
        );
        result
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

fn presentation_support(
    api_version: u32,
    extensions_present: bool,
    feature_enabled: bool,
) -> Result<(), SharedDeviceError> {
    if api_version >= vk::API_VERSION_1_1 && extensions_present && feature_enabled {
        Ok(())
    } else {
        Err(SharedDeviceError::PresentationUnavailable)
    }
}

fn presentation_features(
    adapter: &super::Adapter,
    extensions: &mut Vec<&'static core::ffi::CStr>,
) -> Result<Option<vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT<'static>>, SharedDeviceError>
{
    if !adapter
        .instance
        .extensions
        .contains(&ash::ext::surface_maintenance1::NAME)
    {
        return Ok(None);
    }
    let properties = adapter.physical_device_capabilities();
    let required = [
        ash::khr::swapchain::NAME,
        ash::ext::swapchain_maintenance1::NAME,
    ];
    let extensions_present = adapter
        .instance
        .extensions
        .contains(&ash::khr::surface::NAME)
        && adapter
            .instance
            .extensions
            .contains(&ash::khr::get_surface_capabilities2::NAME)
        && required
            .iter()
            .all(|extension| properties.supports_extension(extension));
    presentation_support(
        adapter
            .instance
            .instance_api_version
            .min(properties.properties().api_version),
        extensions_present,
        true,
    )?;
    let mut feature = vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default();
    unsafe {
        adapter.instance.raw.get_physical_device_features2(
            adapter.raw,
            &mut vk::PhysicalDeviceFeatures2::default().push_next(&mut feature),
        );
    }
    presentation_support(
        properties.properties().api_version,
        true,
        feature.swapchain_maintenance1 != 0,
    )?;
    for extension in required {
        if !extensions.contains(&extension) {
            extensions.push(extension);
        }
    }
    Ok(Some(feature))
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
    if !adapter.private_caps.timeline_semaphores {
        return Err(SharedDeviceError::TimelineSemaphoresUnavailable);
    }
    #[cfg(target_os = "linux")]
    let (features, extra_extensions) = {
        let mut extra = Vec::new();
        let features = super::renderer_imports::configure(adapter, features, &mut extra);
        (features, extra)
    };
    let mut extensions = adapter.required_device_extensions(features);
    #[cfg(target_os = "linux")]
    for extension in extra_extensions {
        if !extensions.contains(&extension) {
            extensions.push(extension);
        }
    }
    #[cfg(unix)]
    if adapter.instance.instance_api_version >= vk::API_VERSION_1_1
        && adapter
            .physical_device_capabilities()
            .properties()
            .api_version
            >= vk::API_VERSION_1_1
        && adapter
            .physical_device_capabilities()
            .supports_extension(ash::khr::external_semaphore_fd::NAME)
        && !extensions.contains(&ash::khr::external_semaphore_fd::NAME)
    {
        extensions.push(ash::khr::external_semaphore_fd::NAME);
    }
    let mut presentation = presentation_features(adapter, &mut extensions)?;
    let mut device_features = adapter.physical_device_features(&extensions, features);
    let priorities = [1.0, 1.0];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&priorities)];
    let extension_names: Vec<_> = extensions.iter().map(|name| name.as_ptr()).collect();
    let info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&extension_names);
    let mut info = device_features.add_to_device_create(info);
    if let Some(feature) = presentation.as_mut() {
        info = info.push_next(feature);
    }
    let raw = unsafe { adapter.instance.raw.create_device(adapter.raw, &info, None) }
        .map_err(super::map_host_device_oom_and_lost_err)?;
    let owner = Arc::new(NativeDevice {
        raw,
        _instance: adapter.instance.clone(),
        physical_device: adapter.raw,
        extensions: extensions.clone(),
        features,
        limits: limits.clone(),
        queues: Arc::new(QueueGroup::new(presentation.is_some())),
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
