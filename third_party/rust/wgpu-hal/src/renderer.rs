//! Portable operations required by the embedding graphics renderer.

use crate::{DeviceError, DynAdapter, DynCommandEncoder, DynDevice, DynInstance, DynTexture};

#[derive(Clone, Debug)]
pub struct GraphicsCapabilities {
    pub features: wgt::Features,
    pub validation_enabled: bool,
    pub max_viewport_dimensions: [u32; 2],
    pub viewport_bounds_range: [f32; 2],
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum GraphicsCapabilitiesError {
    #[error("The renderer backend is unsupported")]
    UnsupportedBackend,
    #[error("The instance, adapter and device do not belong together")]
    IncompatibleObjects,
    #[error("The instance and adapter cannot execute the renderer's precompiled shaders")]
    UnsupportedShaderVersion,
}

pub fn graphics_capabilities(
    instance: &dyn DynInstance,
    adapter: &dyn DynAdapter,
    device: &dyn DynDevice,
) -> Result<GraphicsCapabilities, GraphicsCapabilitiesError> {
    #[cfg(vulkan)]
    if let (Some(instance), Some(adapter), Some(device)) = (
        instance.as_any().downcast_ref::<crate::vulkan::Instance>(),
        adapter.as_any().downcast_ref::<crate::vulkan::Adapter>(),
        device.as_any().downcast_ref::<crate::vulkan::Device>(),
    ) {
        return crate::vulkan::renderer::graphics_capabilities(instance, adapter, device);
    }
    let _ = (instance, adapter, device);
    Err(GraphicsCapabilitiesError::UnsupportedBackend)
}

/// Zero a color subresource without requiring render-attachment usage.
///
/// # Safety
///
/// The encoder must be recording outside a pass, and the non-compressed color
/// texture must belong to its device, cover the range and be in COPY_DST state.
pub unsafe fn clear_texture_zero(
    encoder: &mut dyn DynCommandEncoder,
    texture: &dyn DynTexture,
    range: wgt::ImageSubresourceRange,
) -> Result<(), DeviceError> {
    #[cfg(vulkan)]
    if let (Some(encoder), Some(texture)) = (
        encoder
            .as_any_mut()
            .downcast_mut::<crate::vulkan::CommandEncoder>(),
        texture.as_any().downcast_ref::<crate::vulkan::Texture>(),
    ) {
        return unsafe { crate::vulkan::renderer::clear_texture_zero(encoder, texture, range) };
    }
    let _ = (encoder, texture, range);
    Err(DeviceError::Unexpected)
}
