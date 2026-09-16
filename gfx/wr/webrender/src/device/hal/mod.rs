/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::convert::{TryFrom, TryInto};
use std::ops::Deref;
use wgpu_hal as hal;
use wgpu_hal::{Adapter as _, CommandEncoder as _, Device as _, Instance as _, Queue as _};
use wgpu_types as wgt;

mod pool;
#[cfg(any(test, feature = "hal-testing"))]
mod fault;
#[cfg(any(test, feature = "hal-testing"))]
pub use self::fault::FailurePoint;
mod external;
mod compositor;
pub use self::compositor::{CompositorConfig, CompositorTarget, LayerCompositor, NativeCompositor};
pub use crate::composite::{CompositeDescriptor, CompositorInputLayer, NativeSurfaceOperation, NativeSurfaceOperationDetails};
pub use self::external::{ExternalImageDevice, ExternalImageLease, ExternalImageProvider, ExternalImageRelease, ExternalImageSource, NativeImage};
pub(crate) mod backend;
pub(crate) mod render;
mod resources;
mod submission;
pub(crate) mod surface;
pub use self::surface::{PresentationStatus, SurfaceInfo, SurfaceOptions, SurfaceWindow};
mod query;
#[cfg(wr_hal_metal)]
pub(crate) mod metal;
#[cfg(feature = "hal-metal")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod metal_layout;
#[cfg(wr_hal_metal)]
pub use self::metal::{create_metal_device, create_metal_image_device, MetalEvent, MetalCopy, MetalDeviceContext, MetalPlane};
pub use crate::renderer::hal::{BackendKind, SelectedRenderer, create_renderer_for_backend};
#[cfg(wr_hal_metal)]
pub use crate::renderer::hal::{MetalRenderer, create_metal_renderer, create_metal_renderer_for_window, create_metal_renderer_for_layer};
#[cfg(wr_hal_vulkan)]
pub(crate) mod vulkan;
#[cfg(wr_hal_vulkan)]
pub use self::vulkan::{create_vulkan_device, VulkanDeviceContext, VulkanImageDescriptor};
#[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
pub use self::vulkan::{DmaBufLayout, DmaBufPlane, DmaBufExport, DmaBufCopy, DmaBufCapabilities, ForeignRgbFormat, ForeignRgbLayout, ForeignRgbImage, WeakForeignRgbImage};
#[cfg(any(all(target_os = "linux", feature = "hal-linux-dmabuf"), all(target_os = "android", feature = "hal-android-ahb")))]
pub use self::vulkan::SyncFile;
#[cfg(wr_hal_vulkan)]
pub use self::vulkan::{VulkanQueueCoordinator, create_vulkan_image_device};
#[cfg(all(target_os = "windows", feature = "hal-win32"))]
pub use self::vulkan::{Win32Image, Win32ImageLayout, Win32Export, Win32Copy, Win32Semaphore};
#[cfg(all(target_os = "android", feature = "hal-android-ahb"))]
pub use self::vulkan::{AndroidBufferColor, AndroidBufferAlpha, HardwareBufferCopy};
#[cfg(wr_hal_vulkan)]
pub use crate::renderer::hal::{create_vulkan_renderer, create_vulkan_renderer_with_compositor, create_vulkan_renderer_for_window, Renderer};
pub use crate::renderer::hal::{CpuTiming, GpuTiming, PreparedFrameInfo, ReadbackHandle, RecordedFrameHandle, RendererMemoryReport, ScreenshotHandle};
pub use self::render::{DrawStats, FrameOutput};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameCompletion {
    pub(crate) owner: api::RenderBackendId,
    pub(crate) serial: u64,
}

pub use wgpu_types::Backend as NativeBackend;

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RendererCapabilities {
    pub backend: NativeBackend,
    pub shader_input: &'static str,
    pub frame_timestamps: bool,
    pub validation_request_supported: bool,
    pub validation_requested: bool,
    pub dual_source_blending: bool,
    pub max_texture_size: i32,
}

type Result<T> = std::result::Result<T, String>;

#[derive(Default)]
pub struct Options {
    pub adapter_name: Option<String>,
    pub validation: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Filtering {
    #[default]
    Standard,
    LegacyBrilinear,
}

impl Filtering {
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::LegacyBrilinear => "legacy-brilinear",
        }
    }
}

#[derive(Clone, Default)]
struct DeviceLost(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl DeviceLost {
    fn get(&self) -> bool { self.0.load(std::sync::atomic::Ordering::Acquire) }
    fn set(&self, lost: bool) { self.0.store(lost, std::sync::atomic::Ordering::Release); }
}

static NEXT_DEVICE_CACHE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Offscreen bootstrap device. Rendering WR display lists is a separate integration step.
pub struct Device<A: hal::Api> {
    cache_id: u64,
    open: hal::OpenDevice<A>,
    queue_gate: std::sync::Arc<std::sync::Mutex<()>>,
    lost: DeviceLost,
    completion_probe: Option<submission::CompletionProbe<A>>,
    validation_requested: bool,
    #[cfg(any(test, feature = "hal-testing"))]
    fault: std::cell::Cell<Option<FailurePoint>>,
    info: wgt::AdapterInfo,
    capabilities: hal::Capabilities,
    features: wgt::Features,
    next_texture_id: std::cell::Cell<u64>,
    memory: std::cell::Cell<MemoryStats>,
    formats: Vec<(wgt::TextureFormat, hal::TextureFormatCapabilities)>,
    adapter: A::Adapter,
    _instance: A::Instance,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryStats {
    pub buffers: usize,
    pub buffer_bytes: u64,
    pub textures: usize,
    pub texture_bytes: u64,
    pub cached_buffer_bytes: u64,
    pub cached_texture_bytes: u64,
    pub pipelines: usize,
    pub descriptors: usize,
    pub in_flight: usize,
    pub retained_references: usize,
    pub pending_notifications: usize,
    pub pipeline_epochs: usize,
    pub query_slots: usize,
    pub pending_queries: usize,
}

pub struct Readback {
    pub size: [u32; 2],
    /// Packed RGBA8 rows, starting at the texture's top edge.
    pub color: Vec<u8>,
    pub depth: Vec<f32>,
}

struct Resource<'a, D, T> {
    device: &'a D,
    value: Option<T>,
    destroy: unsafe fn(&D, T),
}

impl<'a, D, T> Resource<'a, D, T> {
    fn new(device: &'a D, value: T, destroy: unsafe fn(&D, T)) -> Self {
        Self {
            device,
            value: Some(value),
            destroy,
        }
    }
}

impl<D, T> Deref for Resource<'_, D, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().unwrap()
    }
}

impl<D, T> Drop for Resource<'_, D, T> {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.device, self.value.take().unwrap()) }
    }
}

struct Commands<'a, A: hal::Api> {
    device: &'a A::Device,
    queue: &'a A::Queue,
    encoder: Option<A::CommandEncoder>,
    buffer: Option<A::CommandBuffer>,
    fence: Option<A::Fence>,
    recording: bool,
    submitted: bool,
}

impl<'a, A: hal::Api> Commands<'a, A> {
    fn new(open: &'a hal::OpenDevice<A>) -> Result<Self> {
        let mut commands = Self {
            device: &open.device,
            queue: &open.queue,
            encoder: None,
            buffer: None,
            fence: None,
            recording: false,
            submitted: false,
        };
        unsafe {
            commands.fence = Some(
                open.device
                    .create_fence()
                    .map_err(|e| format!("Creating fence: {e:?}"))?,
            );
            commands.encoder = Some(
                open.device
                    .create_command_encoder(&hal::CommandEncoderDescriptor {
                        label: Some("WR HAL bootstrap"),
                        queue: &open.queue,
                    })
                    .map_err(|e| format!("Creating encoder: {e:?}"))?,
            );
            commands
                .encoder()
                .begin_encoding(Some("WR HAL bootstrap"))
                .map_err(|e| format!("Beginning commands: {e:?}"))?;
            commands.recording = true;
        }
        Ok(commands)
    }

    fn encoder(&mut self) -> &mut A::CommandEncoder {
        self.encoder.as_mut().unwrap()
    }

    fn submit_and_wait(&mut self) -> Result<()> {
        unsafe {
            self.buffer = Some(
                self.encoder()
                    .end_encoding()
                    .map_err(|e| format!("Finishing commands: {e:?}"))?,
            );
            self.recording = false;
            self.submitted = true;
            self.queue
                .submit(
                    &[self.buffer.as_ref().unwrap()],
                    &[],
                    (self.fence.as_ref().unwrap(), 1),
                )
                .map_err(|e| format!("Submitting commands: {e:?}"))?;
            if !self
                .device
                .wait(self.fence.as_ref().unwrap(), 1, None)
                .map_err(|e| format!("Waiting for readback: {e:?}"))?
            {
                return Err("Readback fence did not complete".into());
            }
        }
        Ok(())
    }
}

impl<A: hal::Api> Drop for Commands<'_, A> {
    fn drop(&mut self) {
        unsafe {
            if self.submitted {
                let _ = self.queue.wait_for_idle();
            }
            if let Some(mut encoder) = self.encoder.take() {
                if self.recording {
                    encoder.discard_encoding();
                }
                encoder.reset_all(self.buffer.take().into_iter());
            }
            if let Some(fence) = self.fence.take() {
                self.device.destroy_fence(fence);
            }
        }
    }
}

#[derive(Debug)]
struct ReadbackLayout {
    row_bytes: u32,
    pitch: u32,
    size: u64,
}

impl ReadbackLayout {
    fn new(width: u32, height: u32, alignment: u64) -> Result<Self> {
        Self::with_pixel_size(width, height, alignment, 4)
    }

    fn with_pixel_size(width: u32, height: u32, alignment: u64, bytes_per_pixel: u32) -> Result<Self> {
        if width == 0 || height == 0 || alignment == 0 {
            return Err("Invalid readback dimensions/alignment".into());
        }
        let row_bytes = width.checked_mul(bytes_per_pixel).ok_or("Readback row overflow")?;
        let pitch = u64::from(row_bytes)
            .checked_add(alignment - 1)
            .ok_or("Readback pitch overflow")?
            / alignment
            * alignment;
        let pitch = u32::try_from(pitch).map_err(|_| "Readback pitch exceeds u32")?;
        let size = u64::from(pitch)
            .checked_mul(u64::from(height))
            .ok_or("Readback size overflow")?;
        isize::try_from(size).map_err(|_| "Readback exceeds address space")?;
        Ok(Self {
            row_bytes,
            pitch,
            size,
        })
    }
}

impl<A: backend::BackendApi> Device<A> {
    fn new(options: &Options) -> Result<Self> {
        Self::new_with_window(options, None).map(|(device, _)| device)
    }

    fn new_with_window(options: &Options, window: Option<std::rc::Rc<dyn SurfaceWindow>>)
        -> Result<(Self, Option<surface::SurfaceSetup<A>>)>
    {
        Self::new_with_owner(options, window.map(surface::platform::WindowOwner::new).transpose()?)
    }

    fn new_with_owner(options: &Options, window: Option<surface::platform::WindowOwner>)
        -> Result<(Self, Option<surface::SurfaceSetup<A>>)>
    {

        let display = window.as_ref().map(|window| window.display_handle()).transpose()?.flatten();
        let instance = unsafe {
            A::Instance::init(&hal::InstanceDescriptor {
                name: "WebRender HAL",
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
        .map_err(|e| format!("Initializing HAL: {e:?}"))?;
        let surface = window.as_ref().map(|window| window.create_surface::<A>(&instance)).transpose()?;
        let mut adapters = unsafe { instance.enumerate_adapters(surface.as_ref()) };
        if let Some(name) = &options.adapter_name {
            if name.trim().is_empty() {
                return Err("Adapter name must not be empty".into());
            }
            adapters.retain(|a| a.info.name.to_lowercase().contains(&name.to_lowercase()));
            if adapters.len() != 1 {
                return Err(format!(
                    "Adapter filter {name:?} matched {} devices; expected exactly one",
                    adapters.len()
                ));
            }
        }
        if let Some(surface) = &surface {
            adapters.retain(|adapter| unsafe { adapter.adapter.surface_capabilities(surface) }.is_some());
        }
        adapters.sort_by_key(|a| {
            (
                match a.info.device_type {
                    wgt::DeviceType::DiscreteGpu => 0,
                    wgt::DeviceType::IntegratedGpu => 1,
                    wgt::DeviceType::VirtualGpu => 2,
                    wgt::DeviceType::Cpu => 3,
                    _ => 4,
                },
                a.info.name.clone(),
            )
        });
        let exposed = adapters
            .into_iter()
            .next()
            .ok_or("No HAL adapters available")?;
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
            let caps = unsafe { exposed.adapter.texture_format_capabilities(format) };
            if !caps.contains(usage) {
                return Err(format!(
                    "Adapter does not support {format:?} with {usage:?}"
                ));
            }
        }
        let formats = [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
            wgt::TextureFormat::R8Unorm,
            wgt::TextureFormat::Rg8Unorm,
            wgt::TextureFormat::R16Unorm,
            wgt::TextureFormat::Rg16Unorm,
            wgt::TextureFormat::Rgba32Float,
            wgt::TextureFormat::Rgba32Sint,
            wgt::TextureFormat::Depth32Float,
        ]
        .iter()
        .map(|&format| {
            (format, unsafe {
                exposed.adapter.texture_format_capabilities(format)
            })
        })
        .collect();
        let features = exposed.features
            & (wgt::Features::DUAL_SOURCE_BLENDING | wgt::Features::TEXTURE_FORMAT_16BIT_NORM
                | wgt::Features::TIMESTAMP_QUERY | wgt::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        let (open, features) = A::open_adapter(&exposed, features)?;
        let cache_id = NEXT_DEVICE_CACHE_ID.fetch_update(std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed, |value| value.checked_add(1))
            .map_err(|_| "HAL device cache identity exhausted")?;
        Ok((Self {
            cache_id,
            open,
            queue_gate: std::sync::Arc::new(std::sync::Mutex::new(())),
            lost: DeviceLost::default(),
            completion_probe: None,
            validation_requested: options.validation,
            #[cfg(any(test, feature = "hal-testing"))]
            fault: std::cell::Cell::new(None),
            info: exposed.info,
            capabilities: exposed.capabilities,
            features,
            next_texture_id: std::cell::Cell::new(1),
            memory: std::cell::Cell::new(MemoryStats::default()),
            formats,
            adapter: exposed.adapter,
            _instance: instance,
        }, surface.map(|raw| surface::SurfaceSetup { raw, window: window.unwrap() })))
    }

}

impl<A: hal::Api> Device<A> {
    pub(super) fn lock_queue(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.queue_gate.lock().map_err(|_| "HAL queue coordinator is poisoned".into())
    }

    pub fn info(&self) -> &wgt::AdapterInfo {
        &self.info
    }

    pub(crate) fn supports_dual_source_blending(&self) -> bool {
        self.features.contains(wgt::Features::DUAL_SOURCE_BLENDING)
    }

    pub(crate) fn max_texture_size(&self) -> i32 {
        self.capabilities
            .limits
            .max_texture_dimension_2d
            .min(i32::MAX as u32) as i32
    }

    fn layout(&self, width: u32, height: u32) -> Result<ReadbackLayout> {
        if width > self.capabilities.limits.max_texture_dimension_2d
            || height > self.capabilities.limits.max_texture_dimension_2d
        {
            return Err("Offscreen target exceeds adapter texture limits".into());
        }
        let layout = ReadbackLayout::new(
            width,
            height,
            self.capabilities.alignments.buffer_copy_pitch.get(),
        )?;
        if layout.size > self.capabilities.limits.max_buffer_size {
            return Err("Readback exceeds adapter buffer limit".into());
        }
        Ok(layout)
    }

    fn readback_buffer(
        &self,
        layout: &ReadbackLayout,
    ) -> Result<Resource<'_, A::Device, A::Buffer>> {
        let value = unsafe {
            self.open.device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR readback"),
                size: layout.size,
                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|e| format!("Creating readback: {e:?}"))?;
        Ok(Resource::new(
            &self.open.device,
            value,
            A::Device::destroy_buffer,
        ))
    }

    fn map_readback(&self, buffer: &A::Buffer, layout: &ReadbackLayout) -> Result<Vec<u8>> {
        #[cfg(any(test, feature = "hal-testing"))]
        self.check_fault(FailurePoint::Map)?;
        unsafe {
            let mapping = self
                .open
                .device
                .map_buffer(buffer, 0..layout.size)
                .map_err(|e| { self.lost.set(true); format!("Mapping readback: {e:?}") })?;
            if !mapping.is_coherent {
                self.open
                    .device
                    .invalidate_mapped_ranges(buffer, std::iter::once(0..layout.size));
            }
            let bytes = std::slice::from_raw_parts(mapping.ptr.as_ptr(), layout.size as usize);
            let mut pixels = Vec::with_capacity(
                layout.row_bytes as usize * (layout.size / u64::from(layout.pitch)) as usize,
            );
            for row in bytes.chunks(layout.pitch as usize) {
                pixels.extend_from_slice(&row[..layout.row_bytes as usize]);
            }
            self.open.device.unmap_buffer(buffer);
            Ok(pixels)
        }
    }

    /// Synchronous offscreen bootstrap check; does not replace Renderer or asynchronous capture.
    pub fn clear_and_readback(
        &mut self,
        width: u32,
        height: u32,
        color: [u8; 4],
        depth: f32,
    ) -> Result<Readback> {
        let layout = self.layout(width, height)?;
        if !depth.is_finite() || !(0.0..=1.0).contains(&depth) {
            return Err("Depth clear must be finite and between zero and one".into());
        }
        let extent = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let device = &self.open.device;
        let make_texture = |format, usage| -> Result<_> {
            let texture =
                unsafe { device.create_texture(&texture_descriptor(extent, format, usage)) }
                    .map_err(|e| format!("Creating target: {e:?}"))?;
            Ok(Resource::new(device, texture, A::Device::destroy_texture))
        };
        let color_texture = make_texture(
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::COPY_SRC,
        )?;
        let depth_texture = make_texture(
            wgt::TextureFormat::Depth32Float,
            wgt::TextureUses::DEPTH_WRITE | wgt::TextureUses::COPY_SRC,
        )?;
        let make_view = |texture: &A::Texture, format, usage| -> Result<_> {
            let view = unsafe {
                device.create_texture_view(
                    texture,
                    &hal::TextureViewDescriptor {
                        swizzle: Default::default(),
                        label: Some("WR offscreen view"),
                        format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange::default(),
                    },
                )
            }
            .map_err(|e| format!("Creating target view: {e:?}"))?;
            Ok(Resource::new(device, view, A::Device::destroy_texture_view))
        };
        let color_view = make_view(
            &color_texture,
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureUses::COLOR_TARGET,
        )?;
        let depth_view = make_view(
            &depth_texture,
            wgt::TextureFormat::Depth32Float,
            wgt::TextureUses::DEPTH_WRITE,
        )?;
        let color_buffer = self.readback_buffer(&layout)?;
        let depth_buffer = self.readback_buffer(&layout)?;
        // Drop command ownership before its resources, including on submission errors.
        let mut commands = Commands::<A>::new(&self.open)?;
        unsafe {
            commands
                .encoder()
                .transition_textures(IntoIterator::into_iter([
                    barrier::<A::Texture>(
                        &color_texture,
                        wgt::TextureUses::UNINITIALIZED,
                        wgt::TextureUses::COLOR_TARGET,
                    ),
                    barrier::<A::Texture>(
                        &depth_texture,
                        wgt::TextureUses::UNINITIALIZED,
                        wgt::TextureUses::DEPTH_WRITE,
                    ),
                ]));
            commands
                .encoder()
                .begin_render_pass(&hal::RenderPassDescriptor {
                    label: Some("WR clear"),
                    extent,
                    sample_count: 1,
                    color_attachments: &[Some(hal::ColorAttachment {
                        target: hal::Attachment {
                            view: &color_view,
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                        clear_value: wgt::Color {
                            r: f64::from(color[0]) / 255.0,
                            g: f64::from(color[1]) / 255.0,
                            b: f64::from(color[2]) / 255.0,
                            a: f64::from(color[3]) / 255.0,
                        },
                    })],
                    depth_stencil_attachment: Some(hal::DepthStencilAttachment {
                        depth_read_only: false,
                        stencil_read_only: true,
                        target: hal::Attachment {
                            view: &depth_view,
                            usage: wgt::TextureUses::DEPTH_WRITE,
                        },
                        depth_ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                        stencil_ops: hal::AttachmentOps::LOAD_DONT_CARE
                            | hal::AttachmentOps::STORE_DISCARD,
                        clear_value: (depth, 0),
                    }),
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .map_err(|e| format!("Beginning clear pass: {e:?}"))?;
            commands.encoder().end_render_pass();
            commands
                .encoder()
                .transition_textures(IntoIterator::into_iter([
                    barrier::<A::Texture>(
                        &color_texture,
                        wgt::TextureUses::COLOR_TARGET,
                        wgt::TextureUses::COPY_SRC,
                    ),
                    barrier::<A::Texture>(
                        &depth_texture,
                        wgt::TextureUses::DEPTH_WRITE,
                        wgt::TextureUses::COPY_SRC,
                    ),
                ]));
            copy_readback::<A>(
                commands.encoder(),
                &color_texture,
                &color_buffer,
                &layout,
                extent,
                hal::FormatAspects::COLOR,
            );
            copy_readback::<A>(
                commands.encoder(),
                &depth_texture,
                &depth_buffer,
                &layout,
                extent,
                hal::FormatAspects::DEPTH,
            );
        }
        unsafe {
            commands.encoder().transition_buffers(
                [&*color_buffer, &*depth_buffer]
                    .iter()
                    .copied()
                    .map(|buffer| hal::BufferBarrier {
                        buffer,
                        usage: hal::StateTransition {
                            from: wgt::BufferUses::COPY_DST,
                            to: wgt::BufferUses::MAP_READ,
                        },
                    }),
            );
        }
        commands.submit_and_wait()?;
        let color = self.map_readback(&color_buffer, &layout)?;
        let depth_bytes = self.map_readback(&depth_buffer, &layout)?;
        let depth = depth_bytes
            .chunks_exact(4)
            .map(|p| f32::from_ne_bytes(p.try_into().unwrap()))
            .collect();
        Ok(Readback {
            size: [width, height],
            color,
            depth,
        })
    }
}

fn texture_descriptor(
    size: wgt::Extent3d,
    format: wgt::TextureFormat,
    usage: wgt::TextureUses,
) -> hal::TextureDescriptor<'static> {
    hal::TextureDescriptor {
        label: Some("WR offscreen"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format,
        usage,
        memory_flags: hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    }
}

fn barrier<T: hal::DynTexture>(
    texture: &T,
    from: wgt::TextureUses,
    to: wgt::TextureUses,
) -> hal::TextureBarrier<'_, T> {
    hal::TextureBarrier {
        queue_family_ownership_transfer: None,
        texture,
        range: wgt::ImageSubresourceRange::default(),
        usage: hal::StateTransition { from, to },
    }
}

unsafe fn copy_readback<A: hal::Api>(
    encoder: &mut A::CommandEncoder,
    texture: &A::Texture,
    buffer: &A::Buffer,
    layout: &ReadbackLayout,
    extent: wgt::Extent3d,
    aspect: hal::FormatAspects,
) {
    encoder.copy_texture_to_buffer(
        texture,
        wgt::TextureUses::COPY_SRC,
        buffer,
        std::iter::once(hal::BufferTextureCopy {
            buffer_layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(layout.pitch),
                rows_per_image: Some(extent.height),
            },
            texture_base: hal::TextureCopyBase {
                mip_level: 0,
                array_layer: 0,
                origin: wgt::Origin3d::ZERO,
                aspect,
            },
            size: extent.into(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::ReadbackLayout;
    #[test]
    fn readback_rows_and_overflow() {
        let layout = ReadbackLayout::new(7, 5, 256).unwrap();
        assert_eq!(
            (layout.row_bytes, layout.pitch, layout.size),
            (28, 256, 1280)
        );
        assert!(ReadbackLayout::new(0, 5, 256).is_err());
        assert!(ReadbackLayout::new(u32::MAX, 2, 256).is_err());
        assert!(ReadbackLayout::new(1, 1, 0).is_err());
    }
}

#[cfg(test)]
fn validation_logging() {
    struct Logger;
    impl log::Log for Logger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.target().starts_with("wgpu_hal") && metadata.level() <= log::Level::Warn
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr(),
                    "{} {}: {}",
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: Logger = Logger;
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
    });
}
