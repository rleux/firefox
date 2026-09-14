/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BackendKind {
    Vulkan,
    Metal,
}
impl std::str::FromStr for BackendKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "vulkan" => Ok(Self::Vulkan),
            "metal" => Ok(Self::Metal),
            _ => Err(format!("Unknown HAL backend {value:?}")),
        }
    }
}
impl BackendKind {
    pub fn select(requested: Option<Self>, permitted: &[Self]) -> Result<Self, String> {
        let available = |kind: Self| kind.is_available() && permitted.contains(&kind);
        if let Some(kind) = requested {
            return if available(kind) {
                Ok(kind)
            } else {
                Err(format!(
                    "HAL backend {kind:?} is not compiled for this target/entry point"
                ))
            };
        }
        let order = if cfg!(target_os = "macos") {
            [Self::Metal, Self::Vulkan]
        } else {
            [Self::Vulkan, Self::Metal]
        };
        order
            .iter()
            .copied()
            .find(|kind| available(*kind))
            .ok_or("No HAL backend is compiled for this target/entry point".into())
    }
    pub fn default_available() -> Option<Self> {
        Self::select(None, &[Self::Vulkan, Self::Metal]).ok()
    }
    pub fn is_available(self) -> bool {
        match self {
            Self::Vulkan => cfg!(wr_hal_vulkan),
            Self::Metal => cfg!(wr_hal_metal),
        }
    }
}

enum Inner {
    #[cfg(wr_hal_vulkan)]
    Vulkan(super::vulkan::Renderer),
    #[cfg(wr_hal_metal)]
    Metal(super::metal::MetalRenderer),
    #[cfg(not(any(wr_hal_vulkan, wr_hal_metal)))]
    Unavailable(std::convert::Infallible),
}

pub struct SelectedRenderer {
    inner: Inner,
}

macro_rules! dispatch {
    ($renderer:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match $renderer {
            #[cfg(wr_hal_vulkan)]
            Inner::Vulkan(renderer) => renderer.$method($($arg),*),
            #[cfg(wr_hal_metal)]
            Inner::Metal(renderer) => renderer.$method($($arg),*),
            #[cfg(not(any(wr_hal_vulkan, wr_hal_metal)))]
            Inner::Unavailable(never) => match *never {},
        }
    };
}

impl SelectedRenderer {
    pub fn capabilities(&self) -> crate::device::hal::RendererCapabilities {
        dispatch!(&self.inner, capabilities())
    }
    pub fn external_image_device(&self) -> crate::device::hal::ExternalImageDevice {
        dispatch!(&self.inner, external_image_device())
    }
    pub fn set_external_image_provider(
        &mut self,
        provider: Box<dyn crate::device::hal::ExternalImageProvider>,
    ) -> Result<(), String> {
        dispatch!(&mut self.inner, set_external_image_provider(provider))
    }
    #[cfg(any(test, feature = "hal-testing"))]
    pub fn inject_failure(&self, point: crate::device::hal::FailurePoint) {
        dispatch!(&self.inner, inject_failure(point))
    }
    pub fn is_failed(&self) -> bool {
        dispatch!(&self.inner, is_failed())
    }
    pub fn surface_info(&self) -> Option<crate::device::hal::SurfaceInfo> {
        dispatch!(&self.inner, surface_info())
    }
    pub fn resize_surface(&mut self, size: [u32; 2]) -> Result<(), String> {
        dispatch!(&mut self.inner, resize_surface(size))
    }
    pub fn acquire_surface(&mut self) -> Result<crate::device::hal::PresentationStatus, String> {
        dispatch!(&mut self.inner, acquire_surface())
    }
    pub fn discard_surface(&mut self) -> Result<(), String> {
        dispatch!(&mut self.inner, discard_surface())
    }
    pub fn present(&mut self) -> Result<crate::device::hal::PresentationStatus, String> {
        dispatch!(&mut self.inner, present())
    }
    pub fn poll(&self) -> Result<(), String> {
        dispatch!(&self.inner, poll())
    }
    pub fn enable_gpu_profiling(&self, enabled: bool) -> bool {
        dispatch!(&self.inner, enable_gpu_profiling(enabled))
    }
    pub fn take_gpu_timings(&self) -> Result<Vec<GpuTiming>, String> {
        dispatch!(&self.inner, take_gpu_timings())
    }
    pub fn take_cpu_timings(&mut self) -> Vec<CpuTiming> {
        dispatch!(&mut self.inner, take_cpu_timings())
    }
    pub fn report_memory(&self) -> RendererMemoryReport {
        dispatch!(&self.inner, report_memory())
    }
    pub fn trim_transient_resources(&self, trim_upload_buffers: bool) {
        dispatch!(&self.inner, trim_transient_resources(trim_upload_buffers))
    }
    pub fn memory_stats(&self) -> crate::device::hal::MemoryStats {
        dispatch!(&self.inner, memory_stats())
    }
    pub fn info(&self) -> &wgpu_types::AdapterInfo {
        dispatch!(&self.inner, info())
    }
    pub fn flush_pipeline_info(&mut self) -> PipelineInfo {
        dispatch!(&mut self.inner, flush_pipeline_info())
    }
    pub fn update(&mut self) -> Result<(), String> {
        dispatch!(&mut self.inner, update())
    }
    pub fn has_frame(&self) -> bool {
        dispatch!(&self.inner, has_frame())
    }
    pub fn configure_filtering(
        &mut self,
        filtering: crate::device::hal::Filtering,
    ) -> Result<(), String> {
        dispatch!(&mut self.inner, configure_filtering(filtering))
    }
    pub fn filtering(&self) -> crate::device::hal::Filtering {
        dispatch!(&self.inner, filtering())
    }
    pub fn has_presentable_output(&self) -> bool {
        dispatch!(&self.inner, has_presentable_output())
    }
    pub fn prepare_frame_if_ready(
        &mut self,
        document_id: DocumentId,
    ) -> Result<Option<PreparedFrameInfo>, String> {
        dispatch!(&mut self.inner, prepare_frame_if_ready(document_id))
    }
    pub fn prepare_frame(&mut self, document_id: DocumentId) -> Result<PreparedFrameInfo, String> {
        dispatch!(&mut self.inner, prepare_frame(document_id))
    }
    pub fn render_frame(&mut self) -> Result<FrameOutput, String> {
        dispatch!(&mut self.inner, render_frame())
    }
    pub fn render(&mut self) -> Result<crate::renderer::RenderResults, String> {
        dispatch!(&mut self.inner, render())
    }
    pub fn read_pixels_rgba8(
        &self,
        rect: api::units::FramebufferIntRect,
    ) -> Result<Vec<u8>, String> {
        dispatch!(&self.inner, read_pixels_rgba8(rect))
    }
    pub fn frame_completion(&self) -> Option<FrameCompletion> {
        dispatch!(&self.inner, frame_completion())
    }
    pub fn poll_completion(&self, completion: FrameCompletion) -> Result<bool, String> {
        dispatch!(&self.inner, poll_completion(completion))
    }
    pub fn request_readback(
        &self,
        rect: api::units::FramebufferIntRect,
    ) -> Result<ReadbackHandle, String> {
        dispatch!(&self.inner, request_readback(rect))
    }
    pub fn poll_readback(&self, handle: ReadbackHandle) -> Result<Option<Vec<u8>>, String> {
        dispatch!(&self.inner, poll_readback(handle))
    }
    pub fn wait_readback(&self, handle: ReadbackHandle) -> Result<Vec<u8>, String> {
        dispatch!(&self.inner, wait_readback(handle))
    }
    pub fn cancel_readback(&self, handle: ReadbackHandle) -> Result<(), String> {
        dispatch!(&self.inner, cancel_readback(handle))
    }
    pub fn get_screenshot_async(
        &mut self,
        window_rect: api::units::DeviceIntRect,
        buffer_size: api::units::DeviceIntSize,
        format: ImageFormat,
    ) -> Result<(ScreenshotHandle, api::units::DeviceIntSize), String> {
        dispatch!(
            &mut self.inner,
            get_screenshot_async(window_rect, buffer_size, format)
        )
    }
    pub fn map_and_recycle_screenshot(
        &self,
        handle: ScreenshotHandle,
        destination: &mut [u8],
        stride: usize,
        format: ImageFormat,
    ) -> Result<bool, String> {
        dispatch!(
            &self.inner,
            map_and_recycle_screenshot(handle, destination, stride, format)
        )
    }
    pub fn record_frame(
        &self,
        format: ImageFormat,
    ) -> Result<(RecordedFrameHandle, api::units::DeviceIntSize), String> {
        dispatch!(&self.inner, record_frame(format))
    }
    pub fn map_recorded_frame(
        &self,
        handle: RecordedFrameHandle,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<bool, String> {
        dispatch!(&self.inner, map_recorded_frame(handle, destination, stride))
    }
    pub fn release_profiler_structures(&mut self) {
        dispatch!(&mut self.inner, release_profiler_structures())
    }
    pub fn release_composition_recorder_structures(&mut self) {
        dispatch!(&mut self.inner, release_composition_recorder_structures())
    }
    pub fn supports_bgra_readback(&self) -> bool {
        dispatch!(&self.inner, supports_bgra_readback())
    }
}

pub fn create_renderer_for_backend(
    kind: BackendKind,
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
    window: Option<(
        std::rc::Rc<dyn crate::device::hal::SurfaceWindow>,
        [u32; 2],
        crate::device::hal::SurfaceOptions,
    )>,
) -> Result<(SelectedRenderer, RenderApiSender), String> {
    match kind {
        #[cfg(wr_hal_vulkan)]
        BackendKind::Vulkan => {
            let (core, sender) = create_renderer::<wgpu_hal::api::Vulkan>(
                hal_options,
                options,
                notifier,
                compositor,
                window,
            )?;
            Ok((
                SelectedRenderer {
                    inner: Inner::Vulkan(super::vulkan::Renderer { core }),
                },
                sender,
            ))
        }
        #[cfg(wr_hal_metal)]
        BackendKind::Metal => {
            let (core, sender) = create_renderer::<wgpu_hal::api::Metal>(
                hal_options,
                options,
                notifier,
                compositor,
                window,
            )?;
            Ok((
                SelectedRenderer {
                    inner: Inner::Metal(super::metal::MetalRenderer { core }),
                },
                sender,
            ))
        }
        #[allow(unreachable_patterns)]
        unavailable => Err(format!(
            "HAL backend {unavailable:?} is not compiled for this target"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_respects_entry_point_features_and_never_substitutes() {
        assert!(BackendKind::select(None, &[]).is_err());
        assert!(BackendKind::select(Some(BackendKind::Vulkan), &[BackendKind::Metal]).is_err());
        assert!(BackendKind::select(Some(BackendKind::Metal), &[BackendKind::Vulkan]).is_err());
        assert!("dx12".parse::<BackendKind>().is_err());
        for kind in [BackendKind::Vulkan, BackendKind::Metal] {
            let result = BackendKind::select(None, &[kind]);
            assert_eq!(result.is_ok(), kind.is_available());
            if kind.is_available() {
                assert_eq!(result.unwrap(), kind);
            }
        }
    }
    #[test]
    fn backend_availability_matches_compiled_target() {
        assert_eq!(BackendKind::Vulkan.is_available(), cfg!(wr_hal_vulkan));
        assert_eq!(BackendKind::Metal.is_available(), cfg!(wr_hal_metal));
    }
}
