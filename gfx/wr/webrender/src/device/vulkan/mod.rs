/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::ffi::CStr;
use wgpu_hal as hal;
use wgpu_hal::{Adapter as _, Instance as _};
use wgpu_types as wgt;

mod resources;
pub use self::resources::Buffer;
mod submission;
pub use self::submission::{Recording, Submission};
mod textures;
pub use self::textures::Texture;
pub use crate::device::TextureFilter;

#[derive(Default)]
pub struct Options {
    pub adapter_name: Option<String>,
    pub validation: bool,
}

/// An opened Vulkan adapter, device and queue.
pub struct Device {
    open: hal::OpenDevice<hal::api::Vulkan>,
    info: wgt::AdapterInfo,
    capabilities: hal::Capabilities,
    features: wgt::Features,
    adapter: hal::vulkan::Adapter,
    _instance: hal::vulkan::Instance,
}

impl Device {
    pub fn new(options: &Options) -> Result<Self, String> {
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
        let mut adapters = unsafe { instance.enumerate_adapters(None) };
        let names: Vec<_> = adapters
            .iter()
            .map(|adapter| (adapter.info.name.as_str(), adapter.info.device_type))
            .collect();
        let index = select_adapter(&names, options.adapter_name.as_deref())?;
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
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM
                | wgt::Features::TIMESTAMP_QUERY
                | wgt::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        let open = unsafe {
            exposed.adapter.open(
                features,
                &exposed.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .map_err(|error| format!("Opening {}: {error:?}", exposed.info.name))?;

        Ok(Self {
            open,
            info: exposed.info,
            capabilities: exposed.capabilities,
            features,
            adapter: exposed.adapter,
            _instance: instance,
        })
    }

    pub fn info(&self) -> &wgt::AdapterInfo {
        &self.info
    }

    pub fn capabilities(&self) -> &hal::Capabilities {
        &self.capabilities
    }

    pub fn features(&self) -> wgt::Features {
        self.features
    }

    pub fn raw_device(&self) -> &hal::vulkan::Device {
        &self.open.device
    }

    pub fn queue(&self) -> &hal::vulkan::Queue {
        &self.open.queue
    }
}

fn select_adapter(
    adapters: &[(&str, wgt::DeviceType)],
    requested: Option<&str>,
) -> Result<usize, String> {
    let mut candidates: Vec<_> = (0..adapters.len()).collect();
    if let Some(name) = requested {
        if name.trim().is_empty() {
            return Err("Adapter name must not be empty".into());
        }
        let name_lowercase = name.to_lowercase();
        candidates.retain(|&index| adapters[index].0.to_lowercase().contains(&name_lowercase));
        if candidates.len() != 1 {
            return Err(format!(
                "Adapter filter {name:?} matched {} devices; expected exactly one",
                candidates.len()
            ));
        }
    }
    candidates.sort_by_key(|&index| {
        let (name, device_type) = adapters[index];
        let rank = match device_type {
            wgt::DeviceType::DiscreteGpu => 0,
            wgt::DeviceType::IntegratedGpu => 1,
            wgt::DeviceType::VirtualGpu => 2,
            wgt::DeviceType::Cpu => 3,
            _ => 4,
        };
        (rank, name)
    });
    candidates
        .first()
        .copied()
        .ok_or_else(|| "No Vulkan adapters available".into())
}

#[cfg(test)]
mod tests;
