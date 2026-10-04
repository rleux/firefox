/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::rc::Rc;
use raw_window_handle::HasDisplayHandle;
use wgpu_hal as hal;
use wgpu_types as wgt;

pub mod shaders {
    include!(concat!(env!("OUT_DIR"), "/vulkan_shaders.rs"));
}

mod bindings;
mod binding_cache;
#[cfg(target_os = "linux")]
#[path = "vulkan/external.rs"]
mod external;
#[cfg(target_os = "linux")]
#[path = "vulkan/dmabuf.rs"]
mod dmabuf;
#[cfg(target_os = "linux")]
pub use self::dmabuf::{DmaBufFormat, DmaBufImage, DmaBufImageDescriptor};
#[cfg(target_os = "linux")]
#[path = "vulkan/timeline.rs"]
mod timeline;
#[cfg(target_os = "linux")]
pub use self::timeline::{SharedTimeline, TimelineHandle};
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
mod clear;
mod render_pass;
mod render_device;
pub(super) use self::render_device::RenderDevice;
mod texture_blit;
mod pipeline;
mod program;
mod program_store;
mod shader;
mod vertex_layout;
mod vertex_array;
mod upload_buffers;
pub use self::vertex_layout::vertex_layouts;
mod samplers;
mod renderer_properties;
pub use self::samplers::Samplers;
mod resources;
#[path = "vulkan/mod.rs"]
mod native;
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
    open: hal::DynOpenDevice,
    #[cfg(test)]
    trace: std::cell::RefCell<Vec<tests::Command>>,
    shader_module: fn(&dyn hal::DynDevice, &str, &[u32]) -> Result<Box<dyn hal::DynShaderModule>, String>,
    prepared_shaders: std::cell::RefCell<crate::internal_types::FastHashMap<*const webrender_build::vulkan::ShaderArtifact, std::rc::Weak<shader::PreparedShader>>>,
    shader_layouts: std::cell::RefCell<crate::internal_types::FastHashMap<Vec<wgt::BindGroupLayoutEntry>, std::rc::Weak<shader::ShaderLayouts>>>,
    graphics_api: crate::device::GraphicsApi,
    flip_y: bool,
    depth_zero_to_one: bool,
    info: wgt::AdapterInfo,
    capabilities: hal::Capabilities,
    max_viewport_dimensions: [u32; 2],
    viewport_bounds_range: [f32; 2],
    features: wgt::Features,
    lost: Cell<bool>,
    adapter: Box<dyn hal::DynAdapter>,
    initial_surface: Cell<Option<InitialSurface>>,
    // The instance may retain display connections, so their owner must outlive it.
    instance: Box<dyn hal::DynInstance>,
    display_owner: Option<Rc<dyn HasDisplayHandle>>,
}

impl Device {
    pub(super) fn create_shader_module(&self, label: &str, words: &[u32]) -> Result<Box<dyn hal::DynShaderModule>, String> {
        (self.shader_module)(self.open.device.as_ref(), label, words)
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




}

fn select_adapter<'a>(
    adapters: impl Iterator<Item = (&'a str, wgt::DeviceType)>,
    requested: Option<&str>,
) -> Result<usize, String> {
    let mut best = None;
    for (index, (name, device_type)) in adapters.enumerate() {
        if let Some(requested) = requested {
            if name.to_lowercase().contains(requested) {
                return Ok(index);
            }
        }
        let rank = match device_type {
            wgt::DeviceType::DiscreteGpu => 0,
            wgt::DeviceType::IntegratedGpu => 1,
            wgt::DeviceType::VirtualGpu => 2,
            wgt::DeviceType::Cpu => 3,
            _ => 4,
        };
        let candidate = (rank, index);
        if best.map_or(true, |current| candidate < current) {
            best = Some(candidate);
        }
    }
    best.map(|(_, index)| index)
        .ok_or_else(|| "No Vulkan adapters available".into())
}

#[cfg(test)]
pub(crate) mod tests;
