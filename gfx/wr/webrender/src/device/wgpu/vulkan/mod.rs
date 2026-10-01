use super::*;
use std::ffi::CStr;
use wgpu_hal::{Adapter as _, Instance as _};
impl Device {
    pub(super) unsafe fn clear_color_copy_destination(
        &self,
        encoder: &mut dyn hal::DynCommandEncoder,
        texture: &dyn hal::DynTexture,
        mip_level: u32,
    ) {
        self.raw_device().raw_device().cmd_clear_color_image(
            encoder.as_any().downcast_ref::<hal::vulkan::CommandEncoder>().unwrap().raw_handle(),
            texture.as_any().downcast_ref::<hal::vulkan::Texture>().unwrap().raw_handle(),
            ash::vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &ash::vk::ClearColorValue { uint32: [0; 4] },
            &[ash::vk::ImageSubresourceRange {
                aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                base_mip_level: mip_level,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            }],
        );
    }

    pub fn new(options: &Options) -> Result<Self, String> {
        if let Some(name) = &options.adapter_name {
            if name.trim().is_empty() {
                return Err("Adapter name must not be empty".into());
            }
        }
        let display = options
            .window
            .as_ref()
            .map(|window| {
                window.display_handle()
                    .map_err(|error| format!("Getting Vulkan display handle: {error}"))
            })
            .transpose()?;
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
                display,
            })
        }
        .map_err(|error| format!("Initializing Vulkan: {error:?}"))?;
        let surface = options
            .window
            .as_ref()
            .map(|window| {
                window_surface::WindowSurface::new(
                    &instance,
                    window,
                    display.unwrap().as_raw(),
                    options.surface_options,
                )
            })
            .transpose()?;
        let mut adapters = unsafe { instance.enumerate_adapters(None) };
        if let Some(surface) = &surface {
            adapters.retain(|adapter| unsafe {
                hal::DynAdapter::surface_capabilities(&adapter.adapter, surface.raw.as_ref()).is_some()
            });
            if adapters.is_empty() {
                return Err("No Vulkan adapters can present to the window surface".into());
            }
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

        let limits = exposed
            .adapter
            .physical_device_capabilities()
            .properties()
            .limits;
        Ok(Self {
            open: open.into(),
            #[cfg(test)]
            trace: Default::default(),
            shader_module: spirv_module,
            prepared_shaders: Default::default(),
            shader_layouts: Default::default(),
            graphics_api: crate::device::GraphicsApi::Vulkan,
            flip_y: true,
            depth_zero_to_one: true,
            info: exposed.info,
            capabilities: exposed.capabilities,
            max_viewport_dimensions: limits.max_viewport_dimensions,
            viewport_bounds_range: limits.viewport_bounds_range,
            features,
            lost: Cell::new(false),
            adapter: Box::new(exposed.adapter),
            surface: Cell::new(surface),
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

impl Recording<'_> {
    pub fn vulkan_encoder(&mut self) -> Result<&mut hal::vulkan::CommandEncoder, String> {
        self.encoder().as_any_mut().downcast_mut()
            .ok_or_else(|| "Native Vulkan commands require a Vulkan encoder".into())
    }
}

fn spirv_module(device: &dyn hal::DynDevice, label: &str, words: &[u32]) -> Result<Box<dyn hal::DynShaderModule>, String> {
    unsafe {
        device.create_shader_module(&hal::ShaderModuleDescriptor {
            label: Some(label),
            runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
        }, hal::ShaderInput::SpirV(words))
    }.map_err(|error| format!("Creating shader module {label}: {error:?}"))
}
