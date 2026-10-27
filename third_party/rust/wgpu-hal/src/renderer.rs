//! Portable operations required by the embedding graphics renderer.

use crate::{DeviceError, DynAdapter, DynCommandEncoder, DynDevice, DynInstance, DynTexture};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct SurfaceLifetime(Arc<AtomicBool>);

impl SurfaceLifetime {
    pub fn is_destroyed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Default)]
pub(crate) struct SurfaceDropGuard(Arc<AtomicBool>);

impl SurfaceDropGuard {
    pub(crate) fn lifetime(&self) -> SurfaceLifetime {
        SurfaceLifetime(self.0.clone())
    }
}

impl Drop for SurfaceDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Observe actual native surface destruction, including backend-held generations.
pub fn surface_lifetime(
    surface: &dyn crate::DynSurface,
) -> Result<SurfaceLifetime, crate::SurfaceError> {
    #[cfg(vulkan)]
    if let Some(surface) = surface.as_any().downcast_ref::<crate::vulkan::Surface>() {
        return crate::vulkan::renderer::surface_lifetime(surface);
    }
    let _ = surface;
    Err(crate::SurfaceError::Other(
        "Native surface lifetime is unavailable",
    ))
}

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

/// Configure a shared renderer surface, or retire it when configuration is None.
/// Closing a generation invalidates its tokens but permits retaining their views.
///
/// # Safety
/// The configuration must be valid for this surface and graphics device. Existing
/// native-device ownership is checked; another device cannot retire or replace it.
/// Keep the OS window/display alive through surface/resource destruction and any
/// concurrent context-retirement operation retaining the native generation.
/// Retain submitted resource owners through retirement, including on error.
/// Closed-generation resources permit no new views, recording or execution;
/// metadata may only be checked for rejection and existing views later destroyed.
pub unsafe fn configure_surface(
    surface: &dyn crate::DynSurface,
    device: &dyn DynDevice,
    configuration: Option<&crate::SurfaceConfiguration>,
) -> Result<(), crate::SurfaceError> {
    #[cfg(vulkan)]
    if let (Some(surface), Some(device)) = (
        surface.as_any().downcast_ref::<crate::vulkan::Surface>(),
        device.as_any().downcast_ref::<crate::vulkan::Device>(),
    ) {
        return unsafe {
            crate::vulkan::renderer::configure_surface(surface, device, configuration)
        };
    }
    let _ = (surface, device, configuration);
    Err(crate::SurfaceError::Other(
        "Checked surface configuration is unavailable",
    ))
}

/// Open a renderer endpoint with optional ordinary external-image support.
/// The returned effective features apply the complete optional DMA-BUF policy,
/// including when the incoming feature set already contains that internal bit.
/// # Safety
/// DynAdapter::open requirements apply.
pub unsafe fn open_device(
    adapter: &dyn crate::DynAdapter,
    features: wgt::Features,
    limits: &wgt::Limits,
    hints: &wgt::MemoryHints,
) -> Result<(crate::DynOpenDevice, wgt::Features), crate::DeviceError> {
    #[cfg(all(vulkan, target_os = "linux"))]
    if let Some(adapter) = adapter.as_any().downcast_ref::<crate::vulkan::Adapter>() {
        return unsafe {
            crate::vulkan::renderer_imports::open_adapter(adapter, features, limits, hints)
        };
    }
    unsafe { adapter.open(features, limits, hints) }.map(|open| (open, features))
}

/// Optional renderer features shared by legacy and unified native creation.
pub fn shared_features(available: wgt::Features) -> wgt::Features {
    available
        & (wgt::Features::DUAL_SOURCE_BLENDING
            | wgt::Features::FLOAT32_FILTERABLE
            | wgt::Features::TEXTURE_FORMAT_16BIT_NORM)
}
pub(crate) fn shared_native_features(available: wgt::Features) -> wgt::Features {
    wgt::Features::from_internal_flags(wgt::FeaturesWGPU::empty(), available.features_webgpu)
        | shared_features(available)
        | (available
            & (wgt::Features::EXTERNAL_TEXTURE
                | wgt::Features::TEXTURE_FORMAT_NV12
                | wgt::Features::TEXTURE_FORMAT_P010
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM))
}

/// Create a window-independent native owner for the renderer and WebGPU.
/// # Safety
/// Instance::init requirements and shared::open_shared_device retirement apply.
pub unsafe fn bootstrap_shared_device(
    descriptor: &crate::InstanceDescriptor<'_>,
    require_hardware: bool,
) -> Result<crate::shared::SharedDeviceEndpoints, alloc::string::String> {
    #[cfg(vulkan)]
    {
        return unsafe {
            crate::vulkan::renderer::bootstrap_shared_device(descriptor, require_hardware)
        };
    }
    #[cfg(not(vulkan))]
    {
        let _ = (descriptor, require_hardware);
        Err(alloc::string::String::from(
            "Shared renderer bootstrap requires Vulkan",
        ))
    }
}
