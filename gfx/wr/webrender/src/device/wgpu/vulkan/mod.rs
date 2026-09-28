use super::*;
use std::ffi::CStr;
use wgpu_hal::{Adapter as _, Instance as _};
impl Device {
    pub fn new(options: &Options) -> Result<Self, String> {
        if let Some(name) = &options.adapter_name {
            if name.trim().is_empty() {
                return Err("Adapter name must not be empty".into());
            }
        }
        if options.validation {
            let entry = unsafe { ash::Entry::load() }
                .map_err(|error| format!("Loading Vulkan: {error}"))?;
            let layers = unsafe { entry.enumerate_instance_layer_properties() }
                .map_err(|error| format!("Enumerating layers: {error:?}"))?;
            if !layers.iter().any(|layer| {
                unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) }.to_bytes()
                    == b"VK_LAYER_KHRONOS_validation"
            }) {
                return Err("Requested Vulkan validation layer is unavailable".into());
            }
        }

        let instance = unsafe {
            hal::vulkan::Instance::init(&hal::InstanceDescriptor {
                name: "WebRender Vulkan",
                flags: if options.validation {
                    wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION
                } else {
                    wgt::InstanceFlags::empty()
                },
                memory_budget_thresholds: Default::default(),
                backend_options: Default::default(),
                telemetry: None,
                display: None,
            })
        }
        .map_err(|error| format!("Initializing Vulkan: {error:?}"))?;
        if !supports_shader_api_version(instance.shared_instance().instance_api_version()) {
            return Err("WebRender shaders require a Vulkan 1.1 instance".into());
        }
        let mut adapters = unsafe { instance.enumerate_adapters(None) };
        adapters.retain(|adapter| {
            supports_shader_api_version(adapter.adapter.physical_device_capabilities().properties().api_version)
        });
        if adapters.is_empty() {
            return Err("No Vulkan 1.1 adapters available for WebRender shaders".into());
        }
        let requested = options.adapter_name.as_deref().map(str::to_lowercase);
        let index = select_adapter(
            adapters.iter().map(|adapter| (adapter.info.name.as_str(), adapter.info.device_type)),
            requested.as_deref(),
        )?;
        let exposed = adapters.swap_remove(index);

        for (format, usage) in [
            (
                wgt::TextureFormat::Rgba8Unorm,
                hal::TextureFormatCapabilities::COLOR_ATTACHMENT
                    | hal::TextureFormatCapabilities::COPY_SRC,
            ),
            (
                wgt::TextureFormat::Depth32Float,
                hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
                    | hal::TextureFormatCapabilities::COPY_SRC,
            ),
        ] {
            let capabilities = unsafe { exposed.adapter.texture_format_capabilities(format) };
            if !capabilities.contains(usage) {
                return Err(format!(
                    "Adapter does not support {format:?} with {usage:?}"
                ));
            }
        }

        let features = exposed.features
            & (wgt::Features::DUAL_SOURCE_BLENDING
                | wgt::Features::FLOAT32_FILTERABLE
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM
            );
        let open = unsafe {
            exposed.adapter.open(
                features,
                &exposed.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .map_err(|error| format!("Opening {}: {error:?}", exposed.info.name))?;

        let limits = exposed.adapter.physical_device_capabilities().properties().limits;
        Ok(Self {
            open: open.into(),
            #[cfg(test)]
            trace: Default::default(),
            shader_module: spirv_module,
            lost: std::cell::Cell::new(false),
            max_viewport_dimensions: limits.max_viewport_dimensions,
            viewport_bounds_range: limits.viewport_bounds_range,
            flip_y: true,
            depth_zero_to_one: true,
            info: exposed.info,
            capabilities: exposed.capabilities,
            features,
            adapter: Box::new(exposed.adapter),
            instance: Box::new(instance),
        })
    }
    pub fn raw_device(&self) -> &hal::vulkan::Device {
        self.open.device.as_ref().as_any().downcast_ref().expect("Vulkan device")
    }
    pub fn queue(&self) -> &hal::vulkan::Queue {
        self.open.queue.as_ref().as_any().downcast_ref().expect("Vulkan queue")
    }
    pub(super) fn raw_adapter(&self) -> &hal::vulkan::Adapter {
        self.adapter.as_any().downcast_ref().expect("Vulkan adapter")
    }
}

fn supports_shader_api_version(version: u32) -> bool {
    version >= ash::vk::API_VERSION_1_1
}

#[cfg(test)]
#[path = "device_tests.rs"]
mod tests;

fn spirv_module(device: &dyn hal::DynDevice, label: &str, words: &[u32]) -> Result<Box<dyn hal::DynShaderModule>, String> {
    unsafe {
        device.create_shader_module(&hal::ShaderModuleDescriptor {
            label: Some(label),
            runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
        }, hal::ShaderInput::SpirV(words))
    }.map_err(|error| format!("Creating shader module {label}: {error:?}"))
}
