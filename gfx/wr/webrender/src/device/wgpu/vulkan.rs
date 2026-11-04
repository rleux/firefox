/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use ash::vk;

pub(super) fn open(
    options: &Options,
) -> Result<
    (
        Arc<core::instance::Adapter>,
        Arc<core::device::Device>,
        Arc<core::device::queue::Queue>,
    ),
    String,
> {
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
    let instance = core::instance::Instance::new(
        "WebRender Vulkan",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: if options.validation {
                wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION
            } else {
                wgt::InstanceFlags::empty()
            },
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    );
    let raw_instance = unsafe { instance.as_hal::<hal::api::Vulkan>() }
        .ok_or_else(|| "Initializing Vulkan failed".to_string())?;
    if raw_instance.shared_instance().instance_api_version() < vk::API_VERSION_1_1 {
        return Err("WebRender shaders require a Vulkan 1.1 instance".into());
    }
    let mut adapters = instance.enumerate_adapters(wgt::Backends::VULKAN, false);
    adapters.retain(|adapter| {
        let raw = unsafe { adapter.clone().as_hal::<hal::api::Vulkan>() }.unwrap();
        raw.physical_device_capabilities().properties().api_version >= vk::API_VERSION_1_1
    });
    let info: Vec<_> = adapters
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            (info.name, info.device_type)
        })
        .collect();
    let adapter = adapters.swap_remove(select_adapter(&info, options.adapter_name.as_deref())?);
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
    let raw_adapter = unsafe { adapter.clone().as_hal::<hal::api::Vulkan>() }.unwrap();
    #[cfg(target_os = "linux")]
    let callback = external_extensions(&raw_adapter);
    #[cfg(not(target_os = "linux"))]
    let callback = None;
    // Validate first, then use the same features, limits and physical device for HAL and core.
    let open = unsafe {
        raw_adapter.open_with_callback(
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
            callback,
        )
    }
    .map_err(|error| format!("Opening Vulkan device: {error}"))?;
    let (device, queue) = unsafe { adapter.create_device_and_queue_from_hal(open.into(), &desc) }
        .map_err(|error| format!("Creating core Vulkan device: {error}"))?;
    drop(raw_adapter);
    Ok((adapter, device, queue))
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
