/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::bindings::WrExternalImageHandler;
use std::os::raw::c_void;
use webrender::{api::units::*, api::*, render_api::MemoryReport};
use webrender::{AsyncScreenshotHandle, PipelineInfo, RecordedFrameHandle, RenderResults};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrHalSurface {
    pub display: *mut c_void,
    pub window: u64,
    pub screen: i32,
    pub wayland: bool,
    pub transparent: bool,
}

#[repr(C)]
pub struct WrHalBuffer {
    pub data: *const u8,
    pub length: usize,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: ImageFormat,
    pub opaque: bool,
}

pub enum Renderer {
    Gl(webrender::Renderer),
    #[cfg(target_os = "linux")]
    Vulkan(VulkanRenderer),
}

#[cfg(target_os = "linux")]
pub struct VulkanRenderer {
    pub renderer: webrender::hal::Renderer,
    pub document: Option<DocumentId>,
    error: Option<String>,
    screenshots: std::collections::HashMap<usize, webrender::hal::ScreenshotHandle>,
    recordings: std::collections::HashMap<usize, webrender::hal::RecordedFrameHandle>,
    next_capture: usize,
    frames: std::collections::VecDeque<(u64, webrender::hal::FrameCompletion)>,
    completed_frame: u64,
    max_texture_size: i32,
}

#[cfg(target_os = "linux")]
struct Window(WrHalSurface);

#[cfg(target_os = "linux")]
impl raw_window_handle::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        use raw_window_handle::*;
        let raw = if self.0.wayland {
            let surface = std::ptr::NonNull::new(self.0.window as *mut c_void).ok_or(HandleError::Unavailable)?;
            RawWindowHandle::Wayland(WaylandWindowHandle::new(surface))
        } else {
            RawWindowHandle::Xlib(XlibWindowHandle::new(self.0.window as _))
        };
        // The C++ compositor retains the widget until after the Rust renderer is deleted.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

#[cfg(target_os = "linux")]
impl raw_window_handle::HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        use raw_window_handle::*;
        let display = std::ptr::NonNull::new(self.0.display).ok_or(HandleError::Unavailable)?;
        let raw = if self.0.wayland {
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display))
        } else {
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(display), self.0.screen))
        };
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

#[cfg(target_os = "linux")]
struct BufferImages(WrExternalImageHandler);

#[cfg(target_os = "linux")]
impl webrender::hal::ExternalImageProvider for BufferImages {
    fn acquire(
        &mut self,
        id: ExternalImageId,
        channel: u8,
        _: bool,
    ) -> Result<webrender::hal::ExternalImageLease, String> {
        extern "C" {
            fn wr_renderer_lock_hal_buffer(
                obj: *mut c_void,
                id: ExternalImageId,
                channel: u8,
                data: *mut WrHalBuffer,
            ) -> bool;
            fn wr_renderer_unlock_hal_buffer(obj: *mut c_void, id: ExternalImageId, channel: u8);
        }
        let mut data = std::mem::MaybeUninit::<WrHalBuffer>::uninit();
        let obj = self.0.object();
        if !unsafe { wr_renderer_lock_hal_buffer(obj, id, channel, data.as_mut_ptr()) } {
            return Err(format!(
                "HAL external image {:?}/{} has no supported buffer representation",
                id, channel
            ));
        }
        let data = unsafe { data.assume_init() };
        let result = (|| {
            if data.data.is_null() || data.width <= 0 || data.height <= 0 || data.stride <= 0 {
                return Err("Invalid HAL external buffer".into());
            }
            let row = (data.width as usize)
                .checked_mul(data.format.bytes_per_pixel() as usize)
                .ok_or("HAL external buffer row overflow")?;
            let needed = (data.stride as usize)
                .checked_mul(data.height as usize - 1)
                .and_then(|n| n.checked_add(row))
                .ok_or("HAL external buffer size overflow")?;
            if row > data.stride as usize || needed > data.length {
                return Err("HAL external buffer layout exceeds its allocation".into());
            }
            let mut bytes = unsafe { std::slice::from_raw_parts(data.data, needed) }.to_vec();
            if data.opaque && matches!(data.format, ImageFormat::BGRA8 | ImageFormat::RGBA8) {
                for y in 0..data.height as usize {
                    for x in 0..data.width as usize {
                        bytes[y * data.stride as usize + x * 4 + 3] = 255;
                    }
                }
            }
            let mut desc = ImageDescriptor::new(data.width, data.height, data.format, ImageDescriptorFlags::empty());
            desc.stride = Some(data.stride);
            webrender::hal::ExternalImageLease::new(
                desc,
                TexelRect::new(0.0, 0.0, data.width as f32, data.height as f32),
                0,
                webrender::hal::ExternalImageSource::Buffer(std::sync::Arc::new(bytes)),
                |_| {},
            )
        })();
        unsafe {
            wr_renderer_unlock_hal_buffer(obj, id, channel);
        }
        result
    }
}

impl Renderer {
    #[cfg(target_os = "linux")]
    pub fn new_vulkan(
        surface: WrHalSurface,
        size: DeviceIntSize,
        options: webrender::WebRenderOptions,
        notifier: Box<dyn RenderNotifier>,
    ) -> Result<(Self, webrender::render_api::RenderApiSender), String> {
        if surface.display.is_null() || surface.window == 0 || size.width < 0 || size.height < 0 {
            return Err("Invalid Vulkan window descriptor".into());
        }
        let hal_options = webrender::hal::Options {
            validation: std::env::var_os("MOZ_WR_VULKAN_VALIDATION").is_some(),
            adapter_name: std::env::var("MOZ_WR_VULKAN_ADAPTER").ok(),
        };
        let max_texture_size = options.max_internal_texture_size.unwrap_or(i32::MAX);
        let (renderer, sender) = webrender::hal::create_vulkan_renderer_for_window(
            &hal_options,
            options,
            notifier,
            webrender::hal::CompositorConfig::Draw,
            std::rc::Rc::new(Window(surface)),
            [size.width as u32, size.height as u32],
            webrender::hal::SurfaceOptions {
                vsync: true,
                transparent: surface.transparent,
            },
        )?;
        eprintln!(
            "WebRender backend: Vulkan (wgpu-hal), adapter: {}",
            renderer.info().name
        );
        let max_texture_size = max_texture_size.min(renderer.capabilities().max_texture_size);
        Ok((
            Self::Vulkan(VulkanRenderer {
                renderer,
                document: None,
                error: None,
                screenshots: Default::default(),
                recordings: Default::default(),
                next_capture: 1,
                frames: Default::default(),
                completed_frame: 1,
                max_texture_size,
            }),
            sender,
        ))
    }

    pub fn set_document(&mut self, _id: DocumentId) {
        #[cfg(target_os = "linux")]
        if let Self::Vulkan(r) = self {
            r.document = Some(_id);
        }
    }

    pub fn get_max_texture_size(&self) -> i32 {
        match self {
            Self::Gl(r) => r.get_max_texture_size(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.max_texture_size,
        }
    }

    pub fn set_clear_color(&mut self, color: ColorF) {
        match self {
            Self::Gl(r) => r.set_clear_color(color),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.renderer.set_clear_color(color),
        }
    }

    pub fn set_external_image_handler(&mut self, handler: WrExternalImageHandler) {
        match self {
            Self::Gl(r) => r.set_external_image_handler(Box::new(handler)),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                if let Err(e) = r.renderer.set_external_image_provider(Box::new(BufferImages(handler))) {
                    r.error = Some(e);
                }
            },
        }
    }

    pub fn update(&mut self) {
        match self {
            Self::Gl(r) => r.update(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                if let Err(e) = r.renderer.update() {
                    eprintln!("WebRender Vulkan update failed: {e}");
                    r.error = Some(e);
                }
            },
        }
    }

    pub fn trim_transient_resources(&mut self, buffers: bool) {
        match self {
            Self::Gl(r) => r.trim_transient_resources(buffers),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.renderer.trim_transient_resources(buffers),
        }
    }

    pub fn set_target_frame_publish_id(&mut self, id: FramePublishId) {
        match self {
            Self::Gl(r) => r.set_target_frame_publish_id(id),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.renderer.set_target_frame_publish_id(id),
        }
    }

    pub fn force_redraw(&mut self) {
        match self {
            Self::Gl(r) => r.force_redraw(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.renderer.force_redraw(),
        }
    }

    pub fn render(&mut self, size: DeviceIntSize, age: usize) -> Result<RenderResults, Vec<String>> {
        match self {
            Self::Gl(r) => r
                .render(size, age)
                .map_err(|v| v.iter().map(|e| format!("{e:?}")).collect()),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                let result = (|| {
                    if let Some(e) = &r.error {
                        return Err(e.clone());
                    }
                    r.renderer
                        .select_document(r.document.ok_or("Missing Vulkan document")?)?;
                    if !r.renderer.has_frame() {
                        return Ok(RenderResults::default());
                    }
                    r.renderer.render()
                })();
                result.map_err(|e: String| {
                    r.error = Some(e.clone());
                    vec![e]
                })
            },
        }
    }

    pub fn flush_pipeline_info(&mut self) -> PipelineInfo {
        match self {
            Self::Gl(r) => r.flush_pipeline_info(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.renderer.flush_pipeline_info(),
        }
    }

    pub fn report_memory(&mut self, swgl: *mut c_void) -> MemoryReport {
        match self {
            Self::Gl(r) => r.report_memory(swgl),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                let report = r.renderer.report_memory();
                let mut cpu = report.cpu;
                cpu.render_target_textures += report.gpu.texture_bytes as usize;
                cpu.texture_upload_pbos += report.gpu.buffer_bytes as usize;
                cpu
            },
        }
    }

    pub fn set_profiler_ui(&mut self, ui: &str) {
        match self {
            Self::Gl(r) => r.set_profiler_ui(ui),
            #[cfg(target_os = "linux")]
            Self::Vulkan(_) => {},
        }
    }

    pub fn deinit(self) {
        match self {
            Self::Gl(r) => r.deinit(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(_) => {},
        }
    }

    pub fn read_pixels_into(&mut self, rect: FramebufferIntRect, format: ImageFormat, dst: &mut [u8]) -> bool {
        match self {
            Self::Gl(r) => {
                r.read_pixels_into(rect, format, dst);
                true
            },
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => match r.renderer.read_pixels_rgba8(rect) {
                Ok(mut pixels)
                    if matches!(format, ImageFormat::RGBA8 | ImageFormat::BGRA8) && pixels.len() == dst.len() =>
                {
                    if format == ImageFormat::BGRA8 {
                        for p in pixels.chunks_exact_mut(4) {
                            p.swap(0, 2);
                        }
                    }
                    let row = rect.size().width as usize * 4;
                    let length = pixels.len();
                    for y in 0..rect.size().height as usize / 2 {
                        for x in 0..row {
                            pixels.swap(y * row + x, length - (y + 1) * row + x);
                        }
                    }
                    dst.copy_from_slice(&pixels);
                    true
                },
                result => {
                    r.error = Some(format!("Vulkan readback failed: {:?}", result.err()));
                    false
                },
            },
        }
    }

    pub fn supports_bgra_readback(&self) -> bool {
        match self {
            Self::Gl(r) => r.supports_bgra_readback(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => r.renderer.supports_bgra_readback(),
        }
    }

    pub fn get_screenshot_async(
        &mut self,
        rect: DeviceIntRect,
        size: DeviceIntSize,
        format: ImageFormat,
    ) -> (AsyncScreenshotHandle, DeviceIntSize) {
        match self {
            Self::Gl(r) => r.get_screenshot_async(rect, size, format),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => match r.renderer.get_screenshot_async(rect, size, format) {
                Ok((handle, actual)) => {
                    let id = r.next_capture;
                    r.next_capture = id.checked_add(1).expect("Capture IDs exhausted");
                    r.screenshots.insert(id, handle);
                    (AsyncScreenshotHandle::from_raw(id), actual)
                },
                Err(e) => {
                    warn!("Vulkan screenshot: {}", e);
                    (AsyncScreenshotHandle::from_raw(0), DeviceIntSize::zero())
                },
            },
        }
    }

    pub fn map_and_recycle_screenshot(
        &mut self,
        handle: AsyncScreenshotHandle,
        dst: &mut [u8],
        stride: usize,
        format: ImageFormat,
    ) -> bool {
        match self {
            Self::Gl(r) => r.map_and_recycle_screenshot(handle, dst, stride, format),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                let id = handle.into_raw();
                let Some(&native) = r.screenshots.get(&id) else {
                    return false;
                };
                match r.renderer.map_and_recycle_screenshot(native, dst, stride, format) {
                    Ok(false) => false,
                    Ok(true) => {
                        r.screenshots.remove(&id);
                        true
                    },
                    Err(e) => {
                        warn!("Vulkan screenshot: {}", e);
                        let _ = r.renderer.cancel_screenshot(native);
                        r.screenshots.remove(&id);
                        false
                    },
                }
            },
        }
    }

    pub fn record_frame(&mut self, format: ImageFormat) -> Option<(RecordedFrameHandle, DeviceIntSize)> {
        match self {
            Self::Gl(r) => r.record_frame(format),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => match r.renderer.record_frame(format) {
                Ok((handle, size)) => {
                    let id = r.next_capture;
                    r.next_capture = id.checked_add(1).expect("Capture IDs exhausted");
                    r.recordings.insert(id, handle);
                    Some((RecordedFrameHandle::from_raw(id), size))
                },
                Err(e) => {
                    warn!("Vulkan recording: {}", e);
                    None
                },
            },
        }
    }

    pub fn map_recorded_frame(&mut self, handle: RecordedFrameHandle, dst: &mut [u8], stride: usize) -> bool {
        match self {
            Self::Gl(r) => r.map_recorded_frame(handle, dst, stride),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                let id = handle.into_raw();
                let Some(&native) = r.recordings.get(&id) else {
                    return false;
                };
                match r.renderer.map_recorded_frame(native, dst, stride) {
                    Ok(false) => false,
                    Ok(true) => {
                        r.recordings.remove(&id);
                        true
                    },
                    Err(e) => {
                        warn!("Vulkan recording: {}", e);
                        let _ = r.renderer.cancel_recorded_frame(native);
                        r.recordings.remove(&id);
                        false
                    },
                }
            },
        }
    }

    pub fn release_profiler_structures(&mut self) {
        match self {
            Self::Gl(r) => r.release_profiler_structures(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                r.screenshots.clear();
                r.renderer.release_profiler_structures();
            },
        }
    }

    pub fn release_composition_recorder_structures(&mut self) {
        match self {
            Self::Gl(r) => r.release_composition_recorder_structures(),
            #[cfg(target_os = "linux")]
            Self::Vulkan(r) => {
                r.recordings.clear();
                r.renderer.release_composition_recorder_structures();
            },
        }
    }

    #[cfg(target_os = "linux")]
    pub fn begin_vulkan_frame(&mut self, width: u32, height: u32) -> Result<bool, String> {
        let Self::Vulkan(r) = self else {
            return Err("Not a Vulkan renderer".into());
        };
        r.renderer.poll()?;
        if r.renderer.surface_info().map(|s| s.size) != Some([width, height]) {
            r.renderer.resize_surface([width, height])?;
        }
        Ok(r.renderer.acquire_surface()? == webrender::hal::PresentationStatus::Acquired)
    }

    #[cfg(target_os = "linux")]
    pub fn end_vulkan_frame(&mut self, frame: u64) -> Result<bool, String> {
        let Self::Vulkan(r) = self else {
            return Err("Not a Vulkan renderer".into());
        };
        if !r.renderer.has_presentable_output() {
            r.renderer.discard_surface()?;
            return Ok(true);
        }
        r.renderer.present()?;
        if let Some(completion) = r.renderer.frame_completion() {
            r.frames.push_back((frame, completion));
        }
        Ok(true)
    }

    #[cfg(target_os = "linux")]
    pub fn poll_vulkan(&mut self) -> Result<u64, String> {
        let Self::Vulkan(r) = self else {
            return Err("Not a Vulkan renderer".into());
        };
        r.renderer.poll()?;
        while let Some(&(frame, completion)) = r.frames.front() {
            if !r.renderer.poll_completion(completion)? {
                break;
            }
            r.completed_frame = frame;
            r.frames.pop_front();
        }
        Ok(r.completed_frame)
    }

    #[cfg(target_os = "linux")]
    pub fn cancel_vulkan_frame(&mut self) -> Result<(), String> {
        let Self::Vulkan(r) = self else {
            return Err("Not a Vulkan renderer".into());
        };
        r.renderer.discard_surface()
    }

    #[cfg(target_os = "linux")]
    pub fn pause_vulkan(&mut self) -> Result<(), String> {
        let Self::Vulkan(r) = self else {
            return Err("Not a Vulkan renderer".into());
        };
        r.renderer.discard_surface()?;
        r.renderer.resize_surface([0, 0])
    }

    #[cfg(target_os = "linux")]
    pub fn vulkan_failed(&self) -> bool {
        match self {
            Self::Vulkan(r) => r.error.is_some() || r.renderer.is_failed(),
            _ => false,
        }
    }
}
