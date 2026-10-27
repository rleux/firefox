use alloc::sync::Arc;
use ash::vk;

use crate::renderer::{GraphicsCapabilities, GraphicsCapabilitiesError};

pub(crate) fn graphics_capabilities(
    instance: &super::Instance,
    adapter: &super::Adapter,
    device: &super::Device,
) -> Result<GraphicsCapabilities, GraphicsCapabilitiesError> {
    if !Arc::ptr_eq(&instance.shared, &adapter.instance)
        || !Arc::ptr_eq(&instance.shared, &device.shared.instance)
        || adapter.raw != device.shared.physical_device
    {
        return Err(GraphicsCapabilitiesError::IncompatibleObjects);
    }
    let properties = adapter.physical_device_capabilities().properties();
    if instance.shared.instance_api_version < vk::API_VERSION_1_1
        || properties.api_version < vk::API_VERSION_1_1
    {
        return Err(GraphicsCapabilitiesError::UnsupportedShaderVersion);
    }
    Ok(GraphicsCapabilities {
        features: device.shared.features,
        validation_enabled: instance
            .shared
            .debug_utils
            .as_ref()
            .is_some_and(|debug| debug.callback_data.validation_layer_properties.is_some()),

        max_viewport_dimensions: properties.limits.max_viewport_dimensions,
        viewport_bounds_range: properties.limits.viewport_bounds_range,
    })
}

pub(crate) unsafe fn clear_texture_zero(
    encoder: &mut super::CommandEncoder,
    texture: &super::Texture,
    range: wgt::ImageSubresourceRange,
) -> Result<(), crate::DeviceError> {
    if encoder.active == vk::CommandBuffer::null()
        || range.aspect != wgt::TextureAspect::All
        || range.mip_level_count == Some(0)
        || range.array_layer_count == Some(0)
    {
        return Err(crate::DeviceError::Unexpected);
    }
    unsafe {
        encoder.device.raw.cmd_clear_color_image(
            encoder.active,
            texture.raw,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue { uint32: [0; 4] },
            &[vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: range.base_mip_level,
                level_count: range.mip_level_count.unwrap_or(vk::REMAINING_MIP_LEVELS),
                base_array_layer: range.base_array_layer,
                layer_count: range
                    .array_layer_count
                    .unwrap_or(vk::REMAINING_ARRAY_LAYERS),
            }],
        );
    }
    Ok(())
}

pub(crate) fn surface_lifetime(
    surface: &super::Surface,
) -> Result<crate::renderer::SurfaceLifetime, crate::SurfaceError> {
    surface
        .inner
        .as_any()
        .downcast_ref::<super::swapchain::NativeSurface>()
        .map(|surface| surface.lifetime())
        .ok_or(crate::SurfaceError::Other(
            "Native surface lifetime is unavailable",
        ))
}

pub(crate) unsafe fn configure_surface(
    surface: &super::Surface,
    device: &super::Device,
    configuration: Option<&crate::SurfaceConfiguration>,
) -> Result<(), crate::SurfaceError> {
    if device.shared.shared_queue_group.is_none() {
        return Err(crate::SurfaceError::Other(
            "Checked presentation requires a shared device",
        ));
    }
    match configuration {
        Some(configuration) => unsafe { surface.configure_checked(device, configuration) },
        None => unsafe { surface.unconfigure_checked(device) },
    }
}

pub(crate) unsafe fn bootstrap_shared_device(
    descriptor: &crate::InstanceDescriptor<'_>,
    require_hardware: bool,
) -> Result<crate::shared::SharedDeviceEndpoints, alloc::string::String> {
    use crate::{Adapter as _, Instance as _};
    let instance = unsafe { super::Instance::init_shared_presentation(descriptor) }
        .map_err(|error| alloc::format!("Initializing shared Vulkan instance: {error:?}"))?;
    let mut adapters = unsafe { instance.enumerate_adapters(None) };
    adapters.retain(|adapter| {
        adapter
            .adapter
            .physical_device_capabilities()
            .properties()
            .api_version
            >= vk::API_VERSION_1_1
    });
    let selected = adapters
        .iter()
        .enumerate()
        .min_by_key(|(index, adapter)| {
            let rank = match adapter.info.device_type {
                wgt::DeviceType::DiscreteGpu => 0,
                wgt::DeviceType::IntegratedGpu => 1,
                wgt::DeviceType::VirtualGpu => 2,
                wgt::DeviceType::Cpu => 3,
                _ => 4,
            };
            (rank, *index)
        })
        .map(|(index, _)| index)
        .ok_or("No Vulkan1.1 adapter for the shared renderer")?;
    let adapter = adapters.swap_remove(selected);
    if require_hardware
        && !matches!(
            adapter.info.device_type,
            wgt::DeviceType::DiscreteGpu | wgt::DeviceType::IntegratedGpu
        )
    {
        return Err(alloc::string::String::from(
            "Shared renderer requires a hardware adapter",
        ));
    }
    for (format, required) in [
        (
            wgt::TextureFormat::Rgba8Unorm,
            crate::TextureFormatCapabilities::COLOR_ATTACHMENT
                | crate::TextureFormatCapabilities::COPY_SRC,
        ),
        (
            wgt::TextureFormat::Depth32Float,
            crate::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
                | crate::TextureFormatCapabilities::COPY_SRC,
        ),
    ] {
        if !unsafe { adapter.adapter.texture_format_capabilities(format) }.contains(required) {
            return Err(alloc::format!(
                "Shared renderer adapter lacks {format:?} with {required:?}"
            ));
        }
    }
    let features = crate::renderer::shared_native_features(adapter.features);
    unsafe {
        super::shared::open_shared_device(
            &adapter.adapter,
            features,
            &adapter.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .map_err(|error| alloc::format!("Opening shared renderer: {error}"))
}

#[cfg(test)]
mod bootstrap_tests {
    #[test]
    fn native_ceiling_keeps_public_features_and_only_known_internal_requirements() {
        let available = wgt::Features::TIMESTAMP_QUERY
            | wgt::Features::SHADER_F16
            | wgt::Features::DUAL_SOURCE_BLENDING
            | wgt::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
            | wgt::Features::EXTERNAL_TEXTURE;
        let native = crate::renderer::shared_native_features(available);
        assert!(native.contains(
            wgt::Features::TIMESTAMP_QUERY
                | wgt::Features::SHADER_F16
                | wgt::Features::DUAL_SOURCE_BLENDING
                | wgt::Features::EXTERNAL_TEXTURE
        ));
        assert!(!native.contains(wgt::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES));
        assert!(available.contains(native));
    }
}
