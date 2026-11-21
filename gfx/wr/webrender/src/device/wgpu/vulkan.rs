/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use ash::vk;

pub(super) struct Opened {
    pub instance: Arc<core::instance::Instance>,
    pub surface: Option<surface::WindowSurface>,
    pub adapter: Arc<core::instance::Adapter>,
    pub device: Arc<core::device::Device>,
    pub queue: Arc<core::device::queue::Queue>,
    #[cfg(feature = "shared-webgpu")]
    pub shared: Option<Arc<wgpu_shared::NativeContext>>,
}

fn new_instance(options: &Options) -> Arc<core::instance::Instance> {
    core::instance::Instance::new(
        "WebRender Vulkan",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: if options.validation {
                wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION
            } else {
                wgt::InstanceFlags::empty()
            },
            memory_budget_thresholds: if options.shared_webgpu && cfg!(feature = "shared-webgpu") {
                wgt::MemoryBudgetThresholds {
                    for_resource_creation: Some(95),
                    for_device_loss: None,
                }
            } else {
                wgt::MemoryBudgetThresholds::default()
            },
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    )
}

pub(super) fn open(options: &Options) -> Result<Opened, String> {
    if options.validation {
        let entry =
            unsafe { ash::Entry::load() }.map_err(|error| format!("Loading Vulkan: {error}"))?;
        let layers = unsafe { entry.enumerate_instance_layer_properties() }
            .map_err(|error| format!("Enumerating Vulkan layers: {error:?}"))?;
        if !layers.iter().any(|layer| unsafe {
            std::ffi::CStr::from_ptr(layer.layer_name.as_ptr()).to_bytes()
                == b"VK_LAYER_KHRONOS_validation"
        }) {
            return Err("Requested Vulkan validation layer is unavailable".into());
        }
    }
    #[cfg(feature = "shared-webgpu")]
    let shared = options
        .shared_webgpu
        .then(wgpu_shared::NativeContext::active)
        .flatten();
    #[cfg(feature = "shared-webgpu")]
    let instance = shared
        .as_ref()
        .map(|context| context.instance.clone())
        .unwrap_or_else(|| new_instance(options));
    #[cfg(not(feature = "shared-webgpu"))]
    let instance = new_instance(options);
    let raw_instance = unsafe { instance.as_hal::<hal::api::Vulkan>() }
        .ok_or_else(|| "Initializing Vulkan failed".to_string())?;
    if raw_instance.shared_instance().instance_api_version() < vk::API_VERSION_1_1 {
        return Err("WebRender shaders require a Vulkan 1.1 instance".into());
    }
    let surface = options
        .window
        .as_ref()
        .map(|window| surface::WindowSurface::new(&instance, window.clone()))
        .transpose()?;
    let mut adapters = instance.enumerate_adapters(wgt::Backends::VULKAN, false);
    adapters.retain(|adapter| {
        let raw = unsafe { adapter.clone().as_hal::<hal::api::Vulkan>() }.unwrap();
        raw.physical_device_capabilities().properties().api_version >= vk::API_VERSION_1_1
    });
    if let Some(surface) = &surface {
        adapters.retain(|adapter| surface.supports_adapter(adapter));
        if adapters.is_empty() {
            return Err("No Vulkan adapters can present to the window surface".into());
        }
    }
    let info: Vec<_> = adapters
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            (info.name, info.device_type)
        })
        .collect();
    let adapter = adapters.swap_remove(select_adapter(&info, options.adapter_name.as_deref())?);
    #[cfg(feature = "shared-webgpu")]
    let adapter = if let Some(shared) = &shared {
        if options.adapter_name.is_some() && !shared.matches_adapter(&adapter) {
            return Err("Shared Vulkan device does not match requested adapter".into());
        }
        if surface
            .as_ref()
            .is_some_and(|surface| !surface.supports_adapter(&shared.adapter))
        {
            return Err("Shared Vulkan device cannot present to this surface".into());
        }
        shared.adapter.clone()
    } else {
        adapter
    };
    for (format, usage) in [
        (
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
        ),
        (
            wgt::TextureFormat::Depth32Float,
            wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
        ),
    ] {
        if !adapter
            .get_texture_format_features(format)
            .allowed_usages
            .contains(usage)
        {
            return Err(format!(
                "Adapter does not support {format:?} with {usage:?}"
            ));
        }
    }
    if !adapter
        .features()
        .contains(wgt::Features::PASSTHROUGH_SHADERS)
    {
        return Err("WebRender requires Vulkan shader passthrough support".into());
    }
    let descriptor_heap = options.descriptor_heap;
    if descriptor_heap && !cfg!(target_os = "linux") {
        return Err("WR descriptor heap prototype currently supports Linux only".into());
    }
    if descriptor_heap && options.shared_webgpu {
        return Err(
            "WR descriptor heaps require a dedicated device; shared WebGPU is unsupported".into(),
        );
    }
    let mut desc = core::device::DeviceDescriptor {
        label: Some("WebRender Vulkan".into()),
        required_features: adapter.features()
            & (wgt::Features::DUAL_SOURCE_BLENDING
                | wgt::Features::FLOAT32_FILTERABLE
                | wgt::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM
                | wgt::Features::TEXTURE_COMPONENT_SWIZZLE
                | wgt::Features::PASSTHROUGH_SHADERS)
            | primary_buffer_features(adapter.features(), adapter.get_info().device_type),
        required_limits: adapter.limits(),
        ..Default::default()
    };
    #[cfg(test)]
    if let Some(enabled) = options.primary_mapping {
        desc.required_features
            .set(wgt::Features::MAPPABLE_PRIMARY_BUFFERS, enabled);
    }
    adapter
        .validate_device_descriptor(&mut desc)
        .map_err(|error| format!("Validating Vulkan device: {error}"))?;
    #[cfg(feature = "shared-webgpu")]
    if let Some(shared) = shared {
        let (device, queue) = shared.open(&adapter, &desc).map_err(|e| e.to_string())?;
        return Ok(Opened {
            instance,
            surface,
            adapter,
            device,
            queue,
            shared: Some(shared),
        });
    }
    let raw_adapter = unsafe { adapter.clone().as_hal::<hal::api::Vulkan>() }.unwrap();
    #[cfg(target_os = "linux")]
    let callback = external_extensions(&raw_adapter);
    #[cfg(not(target_os = "linux"))]
    let callback = None;
    let mut heap_features = if descriptor_heap {
        Some(
            hal::vulkan::descriptor_heap::DeviceFeatures::new(&raw_adapter)
                .map_err(|error| format!("Preparing WR descriptor heap features: {error}"))?,
        )
    } else {
        None
    };
    let callback = match heap_features.as_mut() {
        Some(features) => Some(features.callback(callback)),
        None => callback,
    };
    // Validate first, then use the same features, limits and physical device for HAL and core.
    let native_features = if options.shared_webgpu && cfg!(feature = "shared-webgpu") {
        desc.required_features
            | adapter.features()
                & (wgt::Features::all_webgpu_mask()
                    | wgt::Features::EXTERNAL_TEXTURE
                    | wgt::Features::TEXTURE_FORMAT_NV12
                    | wgt::Features::TEXTURE_FORMAT_P010
                    | wgt::Features::TEXTURE_FORMAT_16BIT_NORM)
    } else {
        desc.required_features
    };
    let open = unsafe {
        raw_adapter.open_with_callback(
            native_features,
            &desc.required_limits,
            &desc.memory_hints,
            callback,
        )
    }
    .map_err(|error| format!("Opening Vulkan device: {error}"))?;
    if descriptor_heap {
        unsafe { open.device.enable_wr_descriptor_heap() }
            .map_err(|error| format!("Enabling WR descriptor heap: {error}"))?;
        log::info!("Private WR descriptor heap prototype enabled");
    }
    let (device, queue) = unsafe { adapter.create_device_and_queue_from_hal(open.into(), &desc) }
        .map_err(|error| format!("Creating core Vulkan device: {error}"))?;
    drop(raw_adapter);
    #[cfg(feature = "shared-webgpu")]
    if options.shared_webgpu {
        let context = Arc::new(wgpu_shared::NativeContext::new(
            instance.clone(),
            adapter.clone(),
            device,
            queue,
        ));
        let (device, queue) = context.open(&adapter, &desc).map_err(|e| e.to_string())?;
        context.publish().map_err(str::to_owned)?;
        log::info!(
            "Shared Vulkan native context active on {}",
            adapter.get_info().name
        );
        return Ok(Opened {
            instance,
            surface,
            adapter,
            device,
            queue,
            shared: Some(context),
        });
    }
    Ok(Opened {
        instance,
        surface,
        adapter,
        device,
        queue,
        #[cfg(feature = "shared-webgpu")]
        shared: None,
    })
}

pub(super) fn primary_buffer_features(
    available: wgt::Features,
    kind: wgt::DeviceType,
) -> wgt::Features {
    // Host-visible primary buffers can be much slower on discrete GPUs.
    if matches!(kind, wgt::DeviceType::IntegratedGpu | wgt::DeviceType::Cpu) {
        available & wgt::Features::MAPPABLE_PRIMARY_BUFFERS
    } else {
        wgt::Features::empty()
    }
}

#[cfg(target_os = "linux")]
fn external_extensions(
    adapter: &hal::vulkan::Adapter,
) -> Option<Box<hal::vulkan::CreateDeviceCallback<'_>>> {
    let caps = adapter.physical_device_capabilities();
    let instance = adapter.shared_instance();
    if ![
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
        ash::khr::external_semaphore_fd::NAME,
    ]
    .iter()
    .all(|extension| caps.supports_extension(extension))
        || (caps.properties().api_version < vk::API_VERSION_1_2
            && !caps.supports_extension(ash::khr::timeline_semaphore::NAME))
    {
        return None;
    }
    let mut features = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
    let mut semaphore = vk::ExternalSemaphoreProperties::default();
    unsafe {
        instance.raw_instance().get_physical_device_features2(
            adapter.raw_physical_device(),
            &mut vk::PhysicalDeviceFeatures2::default().push_next(&mut features),
        );
        if features.timeline_semaphore == vk::TRUE {
            let mut timeline =
                vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            instance
                .raw_instance()
                .get_physical_device_external_semaphore_properties(
                    adapter.raw_physical_device(),
                    &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                        .push_next(&mut timeline),
                    &mut semaphore,
                );
        }
    }
    if !semaphore.external_semaphore_features.contains(
        vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
            | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
    ) {
        return None;
    }
    Some(Box::new(move |args| {
        for extension in [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::khr::external_semaphore_fd::NAME,
            ash::ext::queue_family_foreign::NAME,
        ] {
            if caps.supports_extension(extension) && !args.extensions.contains(&extension) {
                args.extensions.push(extension);
            }
        }
    }))
}

#[cfg(target_os = "linux")]
#[path = "vulkan/native.rs"]
mod native;
#[cfg(target_os = "linux")]
pub(super) use native::NativeCache;
#[cfg(target_os = "linux")]
#[path = "vulkan/dmabuf.rs"]
mod dmabuf;
#[cfg(target_os = "linux")]
pub use dmabuf::{DmaBufCapabilities, DmaBufFormat, DmaBufImage, DmaBufImageDescriptor};
#[cfg(target_os = "linux")]
#[path = "vulkan/timeline.rs"]
pub(super) mod timeline;
#[cfg(target_os = "linux")]
pub use timeline::{SharedTimeline, TimelineHandle};

#[cfg(target_os = "linux")]
mod sync_file;
#[cfg(target_os = "linux")]
mod foreign_rgb;
#[cfg(target_os = "linux")]
pub use self::{sync_file::SyncFileWait, foreign_rgb::ForeignRgbLayout};
