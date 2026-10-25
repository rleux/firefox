/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::rc::Rc;
use raw_window_handle::HasDisplayHandle;
use wgpu_hal as hal;
use wgpu_types as wgt;
pub use wgpu_types::{TextureFormat, TextureUses};

pub mod shaders {
    include!(concat!(env!("OUT_DIR"), "/vulkan_shaders.rs"));
}

mod bindings;
mod binding_cache;
mod external_textures;
pub use self::external_textures::ExternalTextureRegistry;
#[cfg(target_os = "linux")]
pub use self::external_textures::{ExternalReleaseStatus, PendingExternalRelease};
#[cfg(target_os = "linux")]
#[path = "vulkan/external.rs"]
mod external;
#[cfg(target_os = "linux")]
#[path = "vulkan/dmabuf.rs"]
mod dmabuf;
#[cfg(target_os = "linux")]
pub use self::dmabuf::{DmaBufCapabilities, DmaBufFormat, DmaBufImage, DmaBufImageDescriptor};
#[cfg(target_os = "linux")]
#[path = "vulkan/foreign_rgb.rs"]
mod foreign_rgb;
#[cfg(target_os = "linux")]
pub use self::foreign_rgb::{
    ForeignReleaseStatus, ForeignRgbImage, ForeignRgbLayout, PendingForeignRelease,
};
#[cfg(target_os = "linux")]
#[path = "vulkan/timeline.rs"]
mod timeline;
#[cfg(target_os = "linux")]
pub use self::timeline::{SharedTimeline, TimelineHandle};
#[cfg(target_os = "linux")]
#[path = "vulkan/sync_file.rs"]
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
mod graphics;
pub use self::graphics::GraphicsRecorderFactory;
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

pub use hal::shared::NativeRetirement;

pub struct SharedDeviceOptions {
    pub context: std::sync::Arc<dyn hal::shared::SharedDeviceContext>,
    pub recover: Rc<dyn Fn(NativeRetirement)>,
    pub fatal_process: unsafe extern "C" fn() -> !,
}

struct SharedDeviceState {
    options: SharedDeviceOptions,
    notified: Cell<bool>,
}

enum InitialSurface {
    Detached,
    Attached(window_surface::WindowSurface),
}

/// An opened Vulkan adapter, device and queue.
pub struct Device {
    open: std::sync::Arc<hal::DynOpenDevice>,
    shared: Option<SharedDeviceState>,
    #[cfg(target_os = "linux")]
    native_cache: native::Cache,
    #[cfg(test)]
    trace: std::cell::RefCell<Vec<tests::Command>>,
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
    pub fn shared_features(adapter_features: wgt::Features) -> wgt::Features {
        adapter_features
            & (wgt::Features::DUAL_SOURCE_BLENDING
                | wgt::Features::FLOAT32_FILTERABLE
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM)
    }

    /// Construct an offscreen renderer from the shared owner's graphics endpoint.
    ///
    /// # Safety
    ///
    /// The open device/queue must be the graphics endpoint associated with the
    /// supplied instance and selected adapter. SharedDeviceOptions.context must
    /// own that same device and queue; submitted resources must outlive
    /// their confirmed native retirement. Use only on the renderer thread in an
    /// isolated graphics process. Recovery must dispatch asynchronously without
    /// reentering WR or strongly capturing Device/Submission/core owners. The
    /// fatal C hook must terminate that process without returning or unwinding.
    pub unsafe fn from_shared_endpoint(
        open: std::sync::Arc<hal::DynOpenDevice>,
        instance: Box<dyn hal::DynInstance>,
        exposed: hal::DynExposedAdapter,
        shared: SharedDeviceOptions,
    ) -> Result<Self, String> {
        let native =
            hal::renderer::graphics_capabilities(&*instance, &*exposed.adapter, &*open.device)
                .map_err(|error| error.to_string())?;
        if !native
            .features
            .contains(Self::shared_features(exposed.features))
        {
            return Err(
                "Shared device did not enable WebRender's supported optional features".into(),
            );
        }
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
            if !unsafe { exposed.adapter.texture_format_capabilities(format) }.contains(usage) {
                return Err(format!(
                    "Adapter does not support {format:?} with {usage:?}"
                ));
            }
        }
        Ok(Self {
            open,
            shared: Some(SharedDeviceState { options: shared, notified: Cell::new(false) }),
            #[cfg(target_os = "linux")]
            native_cache: Default::default(),
            #[cfg(test)]
            trace: Default::default(),
            prepared_shaders: Default::default(),
            shader_layouts: Default::default(),
            graphics_api: crate::device::GraphicsApi::Vulkan,
            flip_y: true,
            depth_zero_to_one: true,
            info: exposed.info,
            capabilities: exposed.capabilities,
            max_viewport_dimensions: native.max_viewport_dimensions,
            viewport_bounds_range: native.viewport_bounds_range,
            features: native.features,
            lost: Cell::new(false),
            adapter: exposed.adapter,
            initial_surface: Cell::new(None),
            instance,
            display_owner: None,
        })
    }

    pub(super) fn create_shader_module(
        &self,
        label: &str,
        words: &[u32],
    ) -> Result<Box<dyn hal::DynShaderModule>, String> {
        unsafe {
            self.open.device.create_shader_module(
                &hal::ShaderModuleDescriptor {
                    label: Some(label),
                    runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
                },
                hal::ShaderInput::SpirV(words),
            )
        }
        .map_err(|error| format!("Creating shader module {label}: {error:?}"))
    }
    pub fn info(&self) -> &wgt::AdapterInfo {
        &self.info
    }

    pub fn is_lost(&self) -> bool {
        self.lost.get()
    }

    fn mark_lost(&self) {
        if self.lost.get() { return; }
        if let Some(shared) = &self.shared {
            match shared.options.context.drain() {
                Ok(()) | Err(hal::DeviceError::Lost) => {}
                Err(_) => unsafe { (shared.options.fatal_process)() },
            }
        }
        self.mark_lost_retired();
    }

    fn mark_lost_retired(&self) {
        self.lost.set(true);
        if let Some(shared) = &self.shared {
            if !shared.notified.replace(true) {
                (shared.options.recover)(NativeRetirement::Confirmed);
            }
        }
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

#[cfg(test)]
mod shared_device_tests;
