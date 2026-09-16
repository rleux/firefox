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

pub enum Renderer {
    Gl(webrender::Renderer),
    #[cfg(target_os = "linux")]
    Vulkan(VulkanRenderer),
}

#[no_mangle]
pub extern "C" fn wr_renderer_get_backend_info(
    renderer: &Renderer,
    backend: &mut nsstring::nsACString,
    adapter: &mut nsstring::nsACString,
    driver: &mut nsstring::nsACString,
) {
    match renderer {
        Renderer::Gl(renderer) => {
            let info = renderer.get_graphics_api_info();
            backend.assign("OpenGL");
            adapter.assign(&info.renderer);
            driver.assign(&info.version);
        },
        #[cfg(target_os = "linux")]
        Renderer::Vulkan(renderer) => {
            let info = renderer.renderer.info();
            backend.assign("Vulkan (wgpu-hal)");
            adapter.assign(&info.name);
            driver.assign(&format!("{} ({})", info.driver, info.driver_info));
        },
    }
}

#[cfg(target_os = "linux")]
pub struct VulkanRenderer {
    _dmabuf_registration: crate::hal_image::DeviceRegistration,
    pub renderer: webrender::hal::Renderer,
    pub document: Option<DocumentId>,
    error: Option<String>,
    screenshots: std::collections::HashMap<usize, webrender::hal::ScreenshotHandle>,
    recordings: std::collections::HashMap<usize, webrender::hal::RecordedFrameHandle>,
    next_capture: usize,
    frames: std::collections::VecDeque<(u64, webrender::hal::FrameCompletion)>,
    completed_frame: u64,
    max_texture_size: i32,
    notifier: Box<dyn RenderNotifier>,
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
        let retry_notifier = notifier.clone();
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
        let dmabuf_registration = crate::hal_image::DeviceRegistration::new(&renderer.external_image_device())?;
        Ok((
            Self::Vulkan(VulkanRenderer {
                _dmabuf_registration: dmabuf_registration,
                renderer,
                document: None,
                error: None,
                screenshots: Default::default(),
                recordings: Default::default(),
                next_capture: 1,
                frames: Default::default(),
                completed_frame: 1,
                max_texture_size,
                notifier: retry_notifier,
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
                if let Err(e) = r
                    .renderer
                    .set_external_image_provider(Box::new(crate::hal_image::ExternalImages::new(
                        handler,
                        r.renderer.external_image_device(),
                    )))
                {
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
                let Some(native) = r.screenshots.remove(&id) else {
                    return false;
                };
                match r.renderer.wait_and_recycle_screenshot(native, dst, stride, format) {
                    Ok(true) => true,
                    Ok(false) => {
                        let _ = r.renderer.cancel_screenshot(native);
                        false
                    },
                    Err(e) => {
                        warn!("Vulkan screenshot: {}", e);
                        let _ = r.renderer.cancel_screenshot(native);
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
                let Some(native) = r.recordings.remove(&id) else {
                    return false;
                };
                match r.renderer.wait_map_recorded_frame(native, dst, stride) {
                    Ok(true) => true,
                    Ok(false) => {
                        let _ = r.renderer.cancel_recorded_frame(native);
                        false
                    },
                    Err(e) => {
                        warn!("Vulkan recording: {}", e);
                        let _ = r.renderer.cancel_recorded_frame(native);
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
        let result = (|| {
            r.renderer.poll()?;
            if r.renderer.surface_info().map(|s| s.size) != Some([width, height]) {
                r.renderer.resize_surface([width, height])?;
            }
            use webrender::hal::PresentationStatus;
            for _ in 0..2 {
                match r.renderer.acquire_surface()? {
                    PresentationStatus::Acquired => return Ok(true),
                    PresentationStatus::Outdated | PresentationStatus::Lost => r.renderer.force_redraw(),
                    PresentationStatus::Timeout => {
                        r.notifier.wake_up(true);
                        return Ok(false);
                    },
                    _ => return Ok(false),
                }
            }
            r.notifier.wake_up(true);
            Ok(false)
        })();
        result.map_err(|e: String| {
            r.error = Some(e.clone());
            e
        })
    }

    #[cfg(target_os = "linux")]
    pub fn end_vulkan_frame(&mut self, frame: u64) -> Result<bool, String> {
        let Self::Vulkan(r) = self else {
            return Err("Not a Vulkan renderer".into());
        };
        if r.renderer.has_acquired_surface() {
            if r.renderer.has_presentable_output() {
                let status = r.renderer.present()?;
                if !matches!(
                    status,
                    webrender::hal::PresentationStatus::Presented { suboptimal: false }
                ) {
                    r.renderer.force_redraw();
                    r.notifier.wake_up(true);
                }
            } else {
                r.renderer.discard_surface()?;
            }
        }
        r.frames.push_back((frame, r.renderer.submit_work()?));
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
