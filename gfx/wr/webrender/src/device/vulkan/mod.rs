/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ffi::CStr;
use std::rc::Rc;
use raw_window_handle::HasDisplayHandle;
use wgpu_hal as hal;
use wgpu_hal::{Adapter as _, Instance as _};
use wgpu_types as wgt;
pub use wgpu_types::{TextureFormat, TextureUses};

#[cfg(wr_vulkan_shaders)]
pub mod shaders {
    include!(concat!(env!("OUT_DIR"), "/vulkan_shaders.rs"));
}

mod bindings;
mod external_textures;
pub use self::external_textures::ExternalTextureRegistry;
#[cfg(target_os = "linux")]
pub use self::external_textures::{ExternalReleaseStatus, PendingExternalRelease};
#[cfg(target_os = "linux")]
mod external;
#[cfg(target_os = "linux")]
mod dmabuf;
#[cfg(target_os = "linux")]
pub use self::dmabuf::{DmaBufCapabilities, DmaBufFormat, DmaBufImage, DmaBufImageDescriptor};
#[cfg(target_os = "linux")]
mod foreign_rgb;
#[cfg(target_os = "linux")]
pub use self::foreign_rgb::{ForeignRgbImage, ForeignRgbLayout};
#[cfg(target_os = "linux")]
mod timeline;
#[cfg(target_os = "linux")]
pub use self::timeline::{SharedTimeline, TimelineHandle};
#[cfg(target_os = "linux")]
mod sync_file;
#[cfg(target_os = "linux")]
pub use self::sync_file::SyncFileWait;
mod surface_config;
pub use self::surface_config::SurfaceOptions;
mod window_surface;
pub use self::window_surface::SurfaceWindow;
#[cfg(all(test, target_os = "linux", feature = "debugger"))]
pub(crate) use self::window_surface::tests::x11::{X11Display, X11Window};
mod swapchain;
#[cfg(test)]
pub(crate) use self::swapchain::testing as surface_testing;
mod draw;
mod render_pass;
#[cfg(wr_vulkan_shaders)]
mod render_device;
#[cfg(wr_vulkan_shaders)]
pub(super) use self::render_device::RenderDevice;
#[cfg(wr_vulkan_shaders)]
mod texture_blit;
mod pipeline;
mod program;
#[cfg(wr_vulkan_shaders)]
mod program_store;
mod shader;
mod vertex_layout;
mod vertex_array;
mod upload_buffers;
pub use self::vertex_layout::vertex_layouts;
mod samplers;
#[cfg(wr_vulkan_shaders)]
mod renderer_properties;
pub use self::samplers::Samplers;
mod resources;
pub use self::resources::Buffer;
mod submission;
pub use self::submission::{Recording, InstanceBuffers, Submission, SubmissionQueue};
mod texture_pool;
mod texture_store;
pub use self::texture_pool::TexturePool;
mod textures;
pub use self::textures::{PendingReadback, Texture};
pub use crate::device::TextureFilter;
mod state;
mod buffer_pool;
pub use self::buffer_pool::BufferPool;

#[derive(Default)]
pub struct Options {
    pub adapter_name: Option<String>,
    pub validation: bool,
    pub window: Option<Rc<dyn SurfaceWindow>>,
    /// A separate display owner allows old windows to be released during replacement.
    /// Otherwise the initial window keeps the display alive until device destruction.
    /// With no window, a display owner starts windowed rendering with no surface attached.
    pub display_owner: Option<Rc<dyn HasDisplayHandle>>,
    pub surface_options: SurfaceOptions,
}

enum InitialSurface {
    Detached,
    Attached(window_surface::WindowSurface),
}

/// An opened Vulkan adapter, device and queue.
pub struct Device {
    open: hal::OpenDevice<hal::api::Vulkan>,
    info: wgt::AdapterInfo,
    capabilities: hal::Capabilities,
    max_viewport_dimensions: [u32; 2],
    viewport_bounds_range: [f32; 2],
    features: wgt::Features,
    lost: Cell<bool>,
    adapter: hal::vulkan::Adapter,
    initial_surface: Cell<Option<InitialSurface>>,
    // The instance may retain display connections, so their owner must outlive it.
    instance: hal::vulkan::Instance,
    display_owner: Option<Rc<dyn HasDisplayHandle>>,
}

impl Device {
    pub fn new(options: &Options) -> Result<Self, String> {
        let display_owner = options.display_owner.clone().or_else(|| {
            options.window.clone().map(|window| window as Rc<dyn HasDisplayHandle>)
        });
        let display = display_owner.as_ref()
            .map(|owner| owner.display_handle()
                .map_err(|error| format!("Getting Vulkan display handle: {error}")))
            .transpose()?;
        if let Some(window) = &options.window {
            let window_display = window.display_handle()
                .map_err(|error| format!("Getting Vulkan window display: {error}"))?;
            if display.map(|handle| handle.as_raw()) != Some(window_display.as_raw()) {
                return Err("Vulkan window and display owner must use the same display".into());
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
                adapter.adapter.surface_capabilities(&surface.raw).is_some()
            });
            if adapters.is_empty() {
                return Err("No Vulkan adapters can present to the window surface".into());
            }
        }
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
        #[cfg(target_os = "linux")]
        let (open, features) = external::open_adapter(&exposed, features)
            .map_err(|error| format!("Opening {}: {error:?}", exposed.info.name))?;
        #[cfg(not(target_os = "linux"))]
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
            open,
            info: exposed.info,
            capabilities: exposed.capabilities,
            max_viewport_dimensions: limits.max_viewport_dimensions,
            viewport_bounds_range: limits.viewport_bounds_range,
            features,
            lost: Cell::new(false),
            adapter: exposed.adapter,
            initial_surface: Cell::new(display_owner.as_ref().map(|_| match surface {
                Some(surface) => InitialSurface::Attached(surface),
                None => InitialSurface::Detached,
            })),
            instance,
            display_owner,
        })
    }

    pub fn info(&self) -> &wgt::AdapterInfo {
        &self.info
    }

    pub fn is_lost(&self) -> bool {
        self.lost.get()
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
pub(crate) mod tests;
