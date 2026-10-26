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
