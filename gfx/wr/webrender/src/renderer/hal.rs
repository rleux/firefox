/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::init::{self, BackendConnection, BackendResourceOptions};
use super::{PipelineInfo, WebRenderOptions};
use crate::api::{
    Checkpoint, DocumentId, FontRenderMode, ImageFormat, NotificationRequest, RenderBackendId,
    RenderNotifier,
};
use crate::api::channel::{unbounded_channel, Receiver, Sender};
use crate::composite::{CompositorConfig, CompositorKind};
use crate::device::hal::{Options, FrameOutput};
use crate::device::hal::backend::BackendApi;
use crate::device::hal::render::{FrameRenderer, RenderedFrame, PendingReadback};
use crate::frame_builder::FrameBuilderConfig;
use crate::internal_types::{RenderedDocument, ResourceUpdateList, ResultMsg};
use crate::render_api::{ApiMsg, RenderApiSender};
use crate::render_backend_pool::RenderBackendPool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

macro_rules! renderer_facade {
    ($name:ident, $api:ty) => {
        pub struct $name { pub(super) core: RendererCore<$api> }
        impl $name {
            pub fn capabilities(&self) -> crate::device::hal::RendererCapabilities { self.core.gpu.capabilities() }
            pub fn external_image_device(&self) -> crate::device::hal::ExternalImageDevice { self.core.external_image_device() }
            pub fn set_external_image_provider(&mut self, provider: Box<dyn crate::device::hal::ExternalImageProvider>) -> Result<(), String> { self.core.set_external_image_provider(provider) }
            #[cfg(any(test, feature = "hal-testing"))]
            pub fn inject_failure(&self, point: crate::device::hal::FailurePoint) { self.core.inject_failure(point) }
            pub fn is_failed(&self) -> bool { self.core.is_failed() }
            pub fn surface_info(&self) -> Option<crate::device::hal::SurfaceInfo> { self.core.surface_info() }
            pub fn resize_surface(&mut self, size: [u32; 2]) -> Result<(), String> { self.core.resize_surface(size) }
            pub fn acquire_surface(&mut self) -> Result<crate::device::hal::PresentationStatus, String> { self.core.acquire_surface() }
            pub fn discard_surface(&mut self) -> Result<(), String> { self.core.discard_surface() }
            pub fn present(&mut self) -> Result<crate::device::hal::PresentationStatus, String> { self.core.present() }
            pub fn poll(&self) -> Result<(), String> { self.core.poll() }
            pub fn poll_completed(&self) -> Result<FrameCompletion, String> { self.core.poll_completed() }
            pub fn enable_gpu_profiling(&self, enabled: bool) -> bool { self.core.enable_gpu_profiling(enabled) }
            pub fn take_gpu_timings(&self) -> Result<Vec<GpuTiming>, String> { self.core.take_gpu_timings() }
            pub fn take_cpu_timings(&mut self) -> Vec<CpuTiming> { self.core.take_cpu_timings() }
            pub fn report_memory(&self) -> RendererMemoryReport { self.core.report_memory() }
            pub fn trim_transient_resources(&self, trim_upload_buffers: bool) { self.core.trim_transient_resources(trim_upload_buffers) }
            pub fn memory_stats(&self) -> crate::device::hal::MemoryStats { self.core.memory_stats() }
            pub fn info(&self) -> &wgpu_types::AdapterInfo { self.core.info() }
            pub fn flush_pipeline_info(&mut self) -> PipelineInfo { self.core.flush_pipeline_info() }
            pub fn update(&mut self) -> Result<(), String> { self.core.update() }
            pub fn set_target_frame_publish_id(&mut self, id: api::FramePublishId) { self.core.target_frame_publish_id = Some(id); }
            pub fn set_clear_color(&mut self, color: api::ColorF) { self.core.clear_color = color; self.core.force_redraw = true; }
            pub fn force_redraw(&mut self) { self.core.force_redraw = true; }
            pub fn select_document(&mut self, id: DocumentId) -> Result<(), String> { self.core.check_document(id) }
            pub fn has_frame(&self) -> bool { self.core.has_frame() }
            pub fn configure_filtering(&mut self, filtering: crate::device::hal::Filtering) -> Result<(), String> { self.core.configure_filtering(filtering) }
            pub fn filtering(&self) -> crate::device::hal::Filtering { self.core.filtering() }
            pub fn has_presentable_output(&self) -> bool { self.core.has_presentable_output() }
            pub fn prepare_frame_if_ready(&mut self, document_id: DocumentId) -> Result<Option<PreparedFrameInfo>, String> { self.core.prepare_frame_if_ready(document_id) }
            pub fn prepare_frame(&mut self, document_id: DocumentId) -> Result<PreparedFrameInfo, String> { self.core.prepare_frame(document_id) }
            pub fn render_metrics(&self) -> Option<(crate::device::hal::diagnostics::RenderMetricsSnapshot, crate::device::hal::diagnostics::RenderMetricsSnapshot)> { self.core.gpu.render_metrics() }
            pub fn render_frame(&mut self) -> Result<FrameOutput, String> { self.core.render_frame() }
            pub fn render(&mut self) -> Result<crate::renderer::RenderResults, String> { self.core.render() }
            pub fn render_if_needed(&mut self) -> Result<RenderOutcome, String> { self.core.render_if_needed() }
            pub fn has_current_output(&self) -> bool { self.core.has_current_output() }
            pub fn service_hidden_frame(&mut self) -> Result<(), String> { self.core.service_hidden_frame() }
            pub fn has_pending_gpu_work(&self) -> bool { self.core.gpu.has_pending_gpu_work() }
            pub fn read_pixels_rgba8(&self, rect: api::units::FramebufferIntRect) -> Result<Vec<u8>, String> { self.core.read_pixels_rgba8(rect) }
            pub fn frame_completion(&self) -> Option<FrameCompletion> { self.core.frame_completion() }
            pub fn submit_work(&self) -> Result<FrameCompletion, String> { self.core.gpu.submit_work().map(|serial| FrameCompletion { owner: self.core.backend_id, serial }) }
            pub fn has_acquired_surface(&self) -> bool { self.core.gpu.has_acquired_surface() }
            pub fn poll_completion(&self, completion: FrameCompletion) -> Result<bool, String> { self.core.poll_completion(completion) }
            pub fn request_readback(&self, rect: api::units::FramebufferIntRect) -> Result<ReadbackHandle, String> { self.core.request_readback(rect) }
            pub fn poll_readback(&self, handle: ReadbackHandle) -> Result<Option<Vec<u8>>, String> { self.core.poll_readback(handle) }
            pub fn wait_readback(&self, handle: ReadbackHandle) -> Result<Vec<u8>, String> { self.core.wait_readback(handle) }
            pub fn cancel_readback(&self, handle: ReadbackHandle) -> Result<(), String> { self.core.cancel_readback(handle) }
            pub fn get_screenshot_async(&mut self, window_rect: api::units::DeviceIntRect, buffer_size: api::units::DeviceIntSize, format: ImageFormat) -> Result<(ScreenshotHandle, api::units::DeviceIntSize), String> { self.core.get_screenshot_async(window_rect, buffer_size, format) }
            pub fn map_and_recycle_screenshot(&self, handle: ScreenshotHandle, destination: &mut [u8], stride: usize, format: ImageFormat) -> Result<bool, String> { self.core.map_and_recycle_screenshot(handle, destination, stride, format) }
            pub fn wait_and_recycle_screenshot(&self, handle: ScreenshotHandle, destination: &mut [u8], stride: usize, format: ImageFormat) -> Result<bool, String> { self.core.map_capture(handle, destination, stride, format, true) }
            pub fn cancel_screenshot(&self, handle: ScreenshotHandle) -> Result<(), String> { self.core.cancel_readback(handle.readback) }
            pub fn record_frame(&self, format: ImageFormat) -> Result<(RecordedFrameHandle, api::units::DeviceIntSize), String> { self.core.record_frame(format) }
            pub fn map_recorded_frame(&self, handle: RecordedFrameHandle, destination: &mut [u8], stride: usize) -> Result<bool, String> { self.core.map_recorded_frame(handle, destination, stride) }
            pub fn wait_map_recorded_frame(&self, handle: RecordedFrameHandle, destination: &mut [u8], stride: usize) -> Result<bool, String> { self.core.map_capture(handle.0, destination, stride, handle.0.format, true) }
            pub fn cancel_recorded_frame(&self, handle: RecordedFrameHandle) -> Result<(), String> { self.core.cancel_readback(handle.0.readback) }
            pub fn release_profiler_structures(&mut self) { self.core.release_profiler_structures() }
            pub fn release_composition_recorder_structures(&mut self) { self.core.release_composition_recorder_structures() }
            pub fn supports_bgra_readback(&self) -> bool { self.core.supports_bgra_readback() }
        }
    };
}

#[cfg(wr_hal_vulkan)]
mod vulkan;
#[cfg(wr_hal_vulkan)]
pub use vulkan::{Renderer, create_vulkan_renderer, create_vulkan_renderer_with_compositor, create_vulkan_renderer_for_window};

mod selected;
pub use selected::{BackendKind, SelectedRenderer, create_renderer_for_backend};
#[cfg(wr_hal_metal)]
mod metal;
#[cfg(wr_hal_metal)]
pub use metal::{MetalRenderer, create_metal_renderer, create_metal_renderer_for_window, create_metal_renderer_for_layer};

#[cfg(all(test, wr_hal_vulkan))]
#[path = "hal_reuse_tests.rs"]
mod reuse_tests;
#[cfg(all(test, wr_hal_vulkan))]
#[path = "hal_partial_tests.rs"]
mod partial_tests;
#[cfg(all(test, wr_hal_vulkan))]
#[path = "hal_hidden_tests.rs"]
mod hidden_tests;

pub enum RenderOutcome {
    Rendered(super::RenderResults),
    Reused,
    Skipped,
}

#[derive(Default)]
struct PreparedRequest {
    generation: u64,
    render: Option<bool>,
}

#[derive(PartialEq)]
struct OutputIdentity {
    document: DocumentId,
    rect: api::units::DeviceIntRect,
    clear_color: api::ColorF,
    surface_generation: Option<u64>,
}

pub(crate) const MAX_DEPTH_IDS: i32 = 1 << 22;

#[derive(Debug)]
pub struct PreparedFrameInfo {
    pub render: bool,
    pub present: bool,
    pub passes: usize,
    pub picture_tiles: usize,
    pub primitive_instances: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReadbackHandle {
    owner: RenderBackendId,
    id: u64,
}

pub use crate::device::hal::FrameCompletion;

#[derive(Clone, Copy, Debug)]
pub struct GpuTiming {
    pub completion: FrameCompletion,
    pub nanoseconds: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct CpuTiming {
    pub completion: FrameCompletion,
    pub composite_time: Duration,
    pub resource_upload_time: Duration,
    pub is_slow: bool,
}

pub struct RendererMemoryReport {
    pub cpu: crate::render_api::MemoryReport,
    pub gpu: crate::device::hal::MemoryStats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenshotHandle {
    readback: ReadbackHandle,
    size: api::units::DeviceIntSize,
    format: ImageFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedFrameHandle(ScreenshotHandle);

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReadbackKind { Pixels, Screenshot, Recording }

struct ReadbackRequest<A: BackendApi> {
    ticket: PendingReadback<A>,
    flip_rows: bool,
    format: ImageFormat,
    kind: ReadbackKind,
}

pub(crate) struct RendererCore<A: BackendApi> {
    target_frame_publish_id: Option<api::FramePublishId>,
    filtering_locked: bool,
    gpu: FrameRenderer<A>,
    ready: Arc<FrameReady>,
    ready_generation: u64,
    pending_message: Option<ResultMsg>,
    document_id: Option<DocumentId>,
    parked_documents: HashMap<DocumentId, RenderedDocument>,
    prepared_requests: HashMap<DocumentId, PreparedRequest>,
    cpu_timings: std::collections::VecDeque<CpuTiming>,
    slow_cpu_frame_threshold: Duration,
    resource_upload_time: Duration,
    last_upload_time: Duration,
    last_upload_bytes: u64,
    last_compositor_surfaces: [usize; 2],
    clear_color: api::ColorF,
    last_output: Option<RenderedFrame<A>>,
    output_identity: Option<OutputIdentity>,
    partial_composition: bool,
    readbacks: RefCell<HashMap<ReadbackHandle, ReadbackRequest<A>>>,
    readback_bytes: Cell<u64>,
    next_readback: Cell<u64>,
    last_descriptor: Option<crate::composite::CompositeDescriptor>,
    last_device_rect: Option<api::units::DeviceIntRect>,
    damage: Vec<api::units::DeviceIntRect>,
    did_rasterize: bool,
    result_rx: Receiver<ResultMsg>,
    api_tx: Option<Sender<ApiMsg>>,
    backend_id: RenderBackendId,
    pub(crate) document: Option<RenderedDocument>,
    pub(crate) notifications: Vec<NotificationRequest>,
    pipeline_info: PipelineInfo,
    force_redraw: bool,
    debug_flags: api::DebugFlags,
    _pool: Arc<RenderBackendPool>,
}

pub(crate) fn create_renderer<A: BackendApi>(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
    window: Option<(std::rc::Rc<dyn crate::device::hal::SurfaceWindow>, [u32; 2], crate::device::hal::SurfaceOptions)>,
) -> Result<(RendererCore<A>, RenderApiSender), String> {
    create_renderer_with_factory(hal_options, options, notifier, compositor, || {
        match window {
        Some((window, size, options)) => {
            let (device, setup) = A::create_device(hal_options, Some(window))?;
            Ok((device, Some((setup.ok_or("HAL window initialization returned no surface")?, size, options))))
        }
        None => Ok((A::create_device(hal_options, None)?.0, None)),
        }
    })
}

pub(crate) fn create_renderer_with_factory<A: BackendApi>(
    _hal_options: &Options, mut options: WebRenderOptions, notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
    factory: impl FnOnce() -> Result<(crate::device::hal::Device<A>, Option<(crate::device::hal::surface::SurfaceSetup<A>, [u32; 2], crate::device::hal::SurfaceOptions)>), String>,
) -> Result<(RendererCore<A>, RenderApiSender), String> {
    if !matches!(
        options.compositor_config,
        CompositorConfig::Draw {
            max_partial_present_rects: 0,
            ..
        }
    ) {
        return Err("HAL currently requires the offscreen Draw compositor".into());
    }
    if options.cached_programs.is_some() || options.resource_override_path.is_some() {
        return Err("GL program caches and shader overrides are unavailable on HAL".into());
    }
    init::initialize_process();
    let compositor_kind = compositor.kind();
    if matches!(compositor_kind, CompositorKind::Layer { .. }) { options.surface_origin_is_top_left = true; }
    let (device, surface) = factory()?;
    if options.reject_software_rasterizer && device.info().device_type == wgpu_types::DeviceType::Cpu {
        return Err("Software Vulkan adapter rejected by the embedding policy".into());
    }
    let timestamp_bits = A::timestamp_valid_bits(&device);
    let max_internal_texture_size = options
        .max_internal_texture_size
        .unwrap_or(device.max_texture_size())
        .min(device.max_texture_size());
    if max_internal_texture_size < 2048 {
        return Err("HAL texture limit is below WebRender's minimum of 2048".into());
    }
    let dual_source = device.supports_dual_source_blending();
    let config = FrameBuilderConfig {
        default_font_render_mode: match (
            options.enable_aa,
            options.enable_subpixel_aa && dual_source,
        ) {
            (false, _) => FontRenderMode::Mono,
            (true, false) => FontRenderMode::Alpha,
            (true, true) => FontRenderMode::Subpixel,
        },
        dual_source_blending_is_supported: dual_source,
        testing: options.testing,
        gpu_supports_fast_clears: false,
        gpu_supports_advanced_blend: false,
        advanced_blend_is_coherent: false,
        gpu_supports_render_target_partial_update: true,
        external_images_require_copy: true,
        batch_lookback_count: WebRenderOptions::BATCH_LOOKBACK_COUNT,
        background_color: Some(options.clear_color),
        compositor_kind,
        tile_size_override: None,
        max_surface_override: None,
        max_depth_ids: MAX_DEPTH_IDS,
        max_target_size: max_internal_texture_size,
        force_invalidation: false,
        is_software: false,
        low_quality_pinch_zoom: options.low_quality_pinch_zoom,
        max_shared_surface_size: options.max_shared_surface_size,
        enable_dithering: options.enable_dithering,
    };
    let resources = BackendResourceOptions {
        max_internal_texture_size,
        image_tiling_threshold: options
            .image_tiling_threshold
            .min(max_internal_texture_size),
        color_cache_formats: ImageFormat::BGRA8.into(),
        swizzle_settings: None,
        supports_r8_texture_upload: true,
    };
    let mut gpu = FrameRenderer::new(device)?;
    gpu.configure_timestamps(timestamp_bits);
    gpu.enable_gpu_profiling(options.debug_flags.contains(api::DebugFlags::GPU_TIME_QUERIES));
    gpu.set_compositor(compositor);
    if let Some((setup, size, options)) = surface { gpu.attach_surface(setup, size, options)?; }
    if options.enable_dithering {
        gpu.enable_dithering()?;
    }
    let ready = Arc::new(FrameReady::default());
    let notifier = Box::new(FrameNotifier {
        inner: notifier,
        ready: ready.clone(),
        metrics: gpu.metrics(),
    });
    let (result_tx, result_rx) = unbounded_channel();
    let BackendConnection {
        api_tx,
        backend_id,
        pool,
        sender,
    } = init::create_render_backend(&mut options, notifier, result_tx, config, resources)
        .map_err(|error| format!("Starting render backend: {error:?}"))?;
    let partial_composition = gpu.info().backend == wgpu_types::Backend::Vulkan
        && std::env::var("WR_HAL_FORCE_FULL_COMPOSITION").as_deref() != Ok("1");
    Ok((
        RendererCore {
            target_frame_publish_id: None,
            filtering_locked: false,
            gpu,
            ready,
            ready_generation: 0,
            pending_message: None,
            document_id: None,
            parked_documents: HashMap::new(),
            prepared_requests: HashMap::new(),
            cpu_timings: std::collections::VecDeque::new(),
            slow_cpu_frame_threshold: Duration::from_millis(10),
            resource_upload_time: Duration::ZERO,
            last_upload_time: Duration::ZERO,
            last_upload_bytes: 0,
            last_compositor_surfaces: [0; 2],
            clear_color: options.clear_color,
            last_output: None,
            output_identity: None,
            partial_composition,
            readbacks: RefCell::new(HashMap::new()),
            readback_bytes: Cell::new(0),
            next_readback: Cell::new(1),
            last_descriptor: None,
            last_device_rect: None,
            damage: Vec::new(),
            did_rasterize: false,
            result_rx,
            api_tx: Some(api_tx),
            backend_id,
            document: None,
            notifications: Vec::new(),
            pipeline_info: PipelineInfo::default(),
            force_redraw: true,
            debug_flags: options.debug_flags,
            _pool: pool,
        },
        sender,
    ))
}

impl<A: BackendApi> RendererCore<A> {
    pub fn external_image_device(&self) -> crate::device::hal::ExternalImageDevice {
        self.gpu.external_image_device()
    }

    pub fn set_external_image_provider(&mut self, provider: Box<dyn crate::device::hal::ExternalImageProvider>) -> Result<(), String> {
        self.flush_required_frame()?;
        self.gpu.set_external_image_provider(provider);
        self.output_identity = None;
        Ok(())
    }

    #[cfg(any(test, feature = "hal-testing"))]
    pub fn inject_failure(&self, point: crate::device::hal::FailurePoint) { self.gpu.inject_failure(point); }

    pub fn is_failed(&self) -> bool { self.gpu.is_failed() }

    pub fn surface_info(&self) -> Option<crate::device::hal::SurfaceInfo> { self.gpu.surface_info() }
    pub fn resize_surface(&mut self, size: [u32; 2]) -> Result<(), String> {
        self.output_identity = None;
        self.gpu.resize_surface(size)
    }
    pub fn acquire_surface(&mut self) -> Result<crate::device::hal::PresentationStatus, String> {
        let status = self.gpu.acquire_surface();
        if !matches!(status, Ok(crate::device::hal::PresentationStatus::Acquired)) {
            self.output_identity = None;
        }
        status
    }
    pub fn discard_surface(&mut self) -> Result<(), String> {
        self.output_identity = None;
        self.gpu.discard_surface()
    }
    pub fn present(&mut self) -> Result<crate::device::hal::PresentationStatus, String> {
        self.gpu.present_output(self.last_output.as_ref().ok_or("No rendered frame to present")?)
    }

    pub fn poll(&self) -> Result<(), String> { self.gpu.poll() }
    pub fn poll_completed(&self) -> Result<FrameCompletion, String> {
        self.gpu.poll_completed().map(|serial| FrameCompletion { owner: self.backend_id, serial })
    }

    pub fn enable_gpu_profiling(&self, enabled: bool) -> bool { self.gpu.enable_gpu_profiling(enabled) }

    pub fn take_gpu_timings(&self) -> Result<Vec<GpuTiming>, String> {
        Ok(self.gpu.take_gpu_timings()?.into_iter().map(|(serial, nanoseconds)| GpuTiming {
            completion: FrameCompletion { owner: self.backend_id, serial }, nanoseconds,
        }).collect())
    }

    pub fn take_cpu_timings(&mut self) -> Vec<CpuTiming> { self.cpu_timings.drain(..).collect() }

    pub fn report_memory(&self) -> RendererMemoryReport {
        let mut cpu = crate::render_api::MemoryReport::default();
        for document in self.document.iter().chain(self.parked_documents.values()) {
            cpu.frame_allocator += document.frame.allocator_memory.get_stats().reserved_bytes;
            cpu.render_tasks += document.frame.render_tasks.report_memory();
        }
        RendererMemoryReport { cpu, gpu: self.memory_stats() }
    }

    pub fn trim_transient_resources(&self, trim_upload_buffers: bool) {
        if let Some(sender) = &self.api_tx {
            let _ = sender.send(ApiMsg::TrimTransientResources { backend_id: self.backend_id, trim_upload_buffers });
        }
    }

    pub fn memory_stats(&self) -> crate::device::hal::MemoryStats {
        let mut stats = self.gpu.memory_stats();
        stats.pending_notifications = self.notifications.len();
        stats.pipeline_epochs = self.pipeline_info.epochs.len();
        stats
    }

    pub fn info(&self) -> &wgpu_types::AdapterInfo {
        self.gpu.info()
    }

    pub fn flush_pipeline_info(&mut self) -> PipelineInfo {
        std::mem::take(&mut self.pipeline_info)
    }

    pub fn update(&mut self) -> Result<(), String> {
        if let Some(metrics) = self.gpu.metrics() {
            metrics.add(crate::device::hal::diagnostics::RenderCounter::Updates, 1);
        }
        self.gpu.poll()?;
        self.update_until(self.target_frame_publish_id)
    }

    fn check_document(&mut self, id: DocumentId) -> Result<(), String> {
        if self.document_id != Some(id) {
            self.flush_required_frame()?;
            if let (Some(previous), Some(document)) = (self.document_id, self.document.take()) {
                self.parked_documents.insert(previous, document);
            }
            self.document = self.parked_documents.remove(&id);
            if let Some(document) = &mut self.document {
                // Picture-cache storage may have been reused by the other document.
                document.frame.has_been_rendered = false;
            }
            self.document_id = Some(id);
            self.output_identity = None;
            self.force_redraw = true;
        }
        Ok(())
    }

    fn flush_required_frame(&mut self) -> Result<(), String> {
        if let Some(document) = &mut self.document {
            if document.frame.must_be_drawn() {
                self.gpu.render_offscreen(&mut document.frame)?;
                document.frame.has_been_rendered = true;
            }
        }
        for document in self.parked_documents.values_mut() {
            if document.frame.must_be_drawn() {
                self.gpu.render_offscreen(&mut document.frame)?;
                document.frame.has_been_rendered = true;
            }
        }
        Ok(())
    }

    fn notify(&mut self, checkpoint: Checkpoint) {
        crate::util::drain_filter(
            &mut self.notifications,
            |request| request.when() == checkpoint,
            |request| request.notify(),
        );
    }

    fn apply_resources(&mut self, updates: ResourceUpdateList) -> Result<(), String> {
        if !updates.is_nop() {
            for request in self.prepared_requests.values_mut() {
                if request.render == Some(false) { request.render = None; }
            }
        }
        let start = std::time::Instant::now();
        self.gpu.update_resources(vec![updates])?;
        self.resource_upload_time += start.elapsed();
        self.notify(Checkpoint::FrameTexturesUpdated);
        Ok(())
    }

    fn update_until(&mut self, limit: Option<api::FramePublishId>) -> Result<(), String> {
        loop {
            let Some(message) = self
                .pending_message
                .take()
                .or_else(|| self.result_rx.try_recv().ok())
            else {
                return Ok(());
            };
            if let (ResultMsg::PublishDocument(publish, ..), Some(limit)) = (&message, limit) {
                if *publish > limit {
                    self.pending_message = Some(message);
                    return Ok(());
                }
            }
            self.process_message(message)?;
        }
    }

    fn process_message(&mut self, message: ResultMsg) -> Result<(), String> {
        self.filtering_locked = true;
        match message {
            ResultMsg::PublishDocument(_, id, mut document, updates) => {
                self.check_document(id)?;
                self.flush_required_frame()?;
                if let Some(mut previous) = self.document.take() {
                    document.profile.merge(&mut previous.profile);
                }
                self.apply_resources(updates)?;
                self.document = Some(document);
                let request = self.prepared_requests.entry(id).or_default();
                if request.render == Some(false) { request.render = None; }
            }
            ResultMsg::RenderDocumentOffscreen(id, mut document, updates) => {
                self.check_document(id)?;
                self.flush_required_frame()?;
                self.apply_resources(updates)?;
                self.gpu.render_offscreen(&mut document.frame)?;
            }
            ResultMsg::UpdateResources {
                resource_updates,
                memory_pressure,
                discard_active_documents,
                trim_upload_buffers,
                ..
            } => {
                if memory_pressure || discard_active_documents {
                    self.flush_required_frame()?;
                    self.document = None;
                    self.parked_documents.clear();
                    self.last_output = None;
                    self.output_identity = None;
                    if let Some(metrics) = self.gpu.metrics() {
                        metrics.set(crate::device::hal::diagnostics::RenderGauge::RetainedOutputBytes, 0);
                    }
                    self.last_descriptor = None;
                    self.last_device_rect = None;
                }
                self.apply_resources(resource_updates)?;
                if memory_pressure || discard_active_documents || trim_upload_buffers {
                    self.gpu.trim_transient_resources(trim_upload_buffers || memory_pressure)?;
                }
            }
            ResultMsg::PublishPipelineInfo(info) => {
                self.pipeline_info.epochs.extend(info.epochs);
                for key in info.removed_pipelines {
                    if !self.pipeline_info.removed_pipelines.contains(&key) {
                        self.pipeline_info.removed_pipelines.push(key);
                    }
                }
            }
            ResultMsg::AppendNotificationRequests(requests) => {
                self.notifications.extend(requests);
                self.notify(Checkpoint::FrameTexturesUpdated);
            }
            ResultMsg::SetParameter(api::Parameter::Bool(
                api::BoolParameter::Multithreading,
                _,
            )) => {}
            ResultMsg::SetParameter(api::Parameter::Float(api::FloatParameter::SlowCpuFrameThreshold, threshold)) => {
                if !threshold.is_finite() || threshold < 0.0 { return Err("Invalid CPU frame threshold".into()); }
                self.slow_cpu_frame_threshold = Duration::try_from_secs_f64(threshold as f64 / 1000.0)
                    .map_err(|_| "CPU frame threshold exceeds supported duration")?;
            }
            // These tune GL upload/copy strategies; HAL uses explicit staging and transfers.
            ResultMsg::SetParameter(api::Parameter::Bool(
                api::BoolParameter::PboUploads
                | api::BoolParameter::BatchedUploads
                | api::BoolParameter::DrawCallsForTextureCopy,
                _,
            ))
            | ResultMsg::SetParameter(api::Parameter::Int(api::IntParameter::BatchedUploadThreshold, _)) => {}
            ResultMsg::ForceRedraw => {
                self.force_redraw = true;
                if let Some(metrics) = self.gpu.metrics() {
                    metrics.add(crate::device::hal::diagnostics::RenderCounter::ForceRedraws, 1);
                }
            }
            ResultMsg::DebugCommand(command) => {
                use crate::render_api::DebugCommand;
                match command {
                    DebugCommand::ClearCaches(_)
                    | DebugCommand::SimulateLongSceneBuild(_)
                    | DebugCommand::EnableNativeCompositor(false)
                    | DebugCommand::SetBatchingLookback(_) => {}
                    DebugCommand::GetDebugFlags(reply) => {
                        let _ = reply.send(self.debug_flags);
                    }
                    DebugCommand::SetFlags(flags) => {
                        let allowed = api::DebugFlags::ECHO_DRIVER_MESSAGES
                            | api::DebugFlags::MISSING_SNAPSHOT_PINK
                            | api::DebugFlags::TEXTURE_CACHE_DBG_CLEAR_EVICTED
                            | api::DebugFlags::DISABLE_COMPOSITOR_CLIPS
                            | api::DebugFlags::DISABLE_BATCHING;
                        let allowed = allowed | api::DebugFlags::GPU_TIME_QUERIES;
                        if !(flags - allowed).is_empty() {
                            return Err(format!("Unsupported HAL debug flags {flags:?}"));
                        }
                        self.debug_flags = flags;
                        self.gpu.enable_gpu_profiling(flags.contains(api::DebugFlags::GPU_TIME_QUERIES));
                    }
                    _ => return Err("Unsupported HAL renderer debug command".into()),
                }
            }
            ResultMsg::DebugOutput(output) => match output {
                #[cfg(feature = "capture")]
                crate::internal_types::DebugOutput::SaveCapture(config, externals) => {
                    self.flush_required_frame()?;
                    let size = self.last_output.as_ref().map(|output| api::units::DeviceIntSize::new(output.size[0] as i32, output.size[1] as i32));
                    self.gpu.save_capture(config, externals, size)?;
                }
                #[cfg(feature = "replay")]
                crate::internal_types::DebugOutput::LoadCapture(config, externals) => {
                    self.flush_required_frame()?;
                    self.document = None;
                    self.document_id = None;
                    self.parked_documents.clear();
                    self.last_output = None;
                    self.output_identity = None;
                    if let Some(metrics) = self.gpu.metrics() {
                        metrics.set(crate::device::hal::diagnostics::RenderGauge::RetainedOutputBytes, 0);
                    }
                    self.last_descriptor = None;
                    self.last_device_rect = None;
                    self.gpu.load_capture(config, externals)?;
                    self.force_redraw = true;
                }
            },
            ResultMsg::RefreshShader(_) => return Err("HAL runtime shader reload is unavailable; rebuild the shader pack".into()),
        }
        Ok(())
    }

    pub fn service_hidden_frame(&mut self) -> Result<(), String> {
        self.output_identity = None;
        self.force_redraw = true;
        if self.gpu.has_acquired_surface() { self.gpu.discard_surface()?; }
        self.flush_required_frame()?;
        self.gpu.poll()?;
        self.damage.clear();
        self.did_rasterize = false;
        self.resource_upload_time = Duration::ZERO;
        self.last_upload_time = Duration::ZERO;
        self.last_upload_bytes = self.gpu.take_resource_upload_bytes();
        if let Some(document) = &mut self.document {
            document.profile.clear();
            document.frame_stats.take();
        }
        self.notify(Checkpoint::FrameRendered);
        if let Some(metrics) = self.gpu.metrics() {
            metrics.add(crate::device::hal::diagnostics::RenderCounter::HiddenSkips, 1);
        }
        Ok(())
    }

    pub fn has_frame(&self) -> bool { self.document.is_some() }

    pub fn configure_filtering(&mut self, filtering: crate::device::hal::Filtering) -> Result<(), String> {
        if self.filtering_locked {
            return Err("HAL filtering must be configured once, before processing renderer messages".into());
        }
        self.gpu.filtering = filtering;
        self.filtering_locked = true;
        Ok(())
    }

    pub fn filtering(&self) -> crate::device::hal::Filtering { self.gpu.filtering }

    pub fn has_presentable_output(&self) -> bool {
        self.last_output.as_ref().map_or(false, |output| !output.size.contains(&0))
    }

    pub fn prepare_frame_if_ready(&mut self, document_id: DocumentId) -> Result<Option<PreparedFrameInfo>, String> {
        let previous = self.prepared_requests.get(&document_id).map_or(0, |request| request.generation);
        let ready = self.ready.state.lock().unwrap().documents.get(&document_id)
            .map_or(false, |frame| frame.generation > previous);
        if ready { self.prepare_frame(document_id).map(Some) } else { Ok(None) }
    }

    pub fn prepare_frame(&mut self, document_id: DocumentId) -> Result<PreparedFrameInfo, String> {
        let previous = self.prepared_requests.get(&document_id).map_or(0, |request| request.generation);
        let (generation, publish, present, render) = self.ready.wait_document(document_id, previous)?;
        self.ready_generation = generation;
        self.update_until(Some(publish))?;
        self.check_document(document_id)?;
        let request = self.prepared_requests.entry(document_id).or_default();
        request.generation = generation;
        request.render = Some(render || request.render == Some(true));
        if let Some(document) = &mut self.document {
            document.frame.present = present;
        }
        let frame = &self.document.as_ref().ok_or("No WR frame available")?.frame;
        let mut primitive_instances = 0;
        for pass in &frame.passes {
            for target in &pass.picture_cache {
                if let crate::render_target::PictureCacheTargetKind::Draw {
                    alpha_batch_container,
                } = &target.kind
                {
                    for batch in alpha_batch_container
                        .opaque_batches
                        .iter()
                        .chain(&alpha_batch_container.alpha_batches)
                    {
                        primitive_instances += batch.instances.len();
                    }
                }
            }
        }
        Ok(PreparedFrameInfo {
            render,
            present,
            passes: frame.passes.len(),
            picture_tiles: frame.composite_state.tiles.len(),
            primitive_instances,
        })
    }

    fn current_output_identity(&self) -> Option<OutputIdentity> {
        Some(OutputIdentity {
            document: self.document_id?,
            rect: self.document.as_ref()?.frame.device_rect,
            clear_color: self.clear_color,
            surface_generation: self.surface_info().map(|surface| surface.generation),
        })
    }

    pub fn has_current_output(&self) -> bool {
        let Some(id) = self.document_id else { return false; };
        let Some(request) = self.prepared_requests.get(&id) else { return false; };
        let Some(document) = self.document.as_ref() else { return false; };
        let Some(output) = self.last_output.as_ref() else { return false; };
        let pending = self.ready.state.lock().unwrap().documents.get(&id)
            .map_or(false, |frame| frame.generation > request.generation);
        request.render == Some(false) && !pending && !self.force_redraw && !self.is_failed()
            && document.frame.present && !document.frame.must_be_drawn()
            && self.output_identity.is_some() && self.output_identity == self.current_output_identity()
            && output.origin == document.frame.device_rect.min
            && output.size == [document.frame.device_rect.width() as u32, document.frame.device_rect.height() as u32]
            && self.gpu.has_owned_output(output)
    }

    pub fn render_if_needed(&mut self) -> Result<RenderOutcome, String> {
        let id = self.document_id.ok_or("Prepare a WR frame before conditional rendering")?;
        {
            let generation = self.prepared_requests.get(&id).map_or(0, |request| request.generation);
            if self.ready.state.lock().unwrap().documents.get(&id)
                .map_or(false, |frame| frame.generation > generation) {
                return Err("Prepare the pending WR frame before conditional rendering".into());
            }
        }
        let skip_offscreen = !self.is_failed()
            && self.document_id.and_then(|id| self.prepared_requests.get(&id))
                .map_or(false, |request| request.render == Some(false))
            && self.document.as_ref().map_or(false, |document| {
                !document.frame.present && !document.frame.must_be_drawn()
            });
        if !skip_offscreen && !self.has_current_output() { return self.render().map(RenderOutcome::Rendered); }
        if !skip_offscreen {
            if let Some(metrics) = self.gpu.metrics() {
                metrics.add(crate::device::hal::diagnostics::RenderCounter::ReusedOutputs, 1);
            }
        }
        self.damage.clear();
        self.did_rasterize = false;
        if let Some(document) = &mut self.document {
            document.frame.has_been_rendered = true;
            document.profile.clear();
            document.frame_stats.take();
        }
        self.notify(Checkpoint::FrameRendered);
        Ok(if skip_offscreen { RenderOutcome::Skipped } else { RenderOutcome::Reused })
    }

    fn store_output(&mut self, output: RenderedFrame<A>) {
        self.output_identity = self.current_output_identity();
        self.last_output = Some(output);
        if let Some(id) = self.document_id {
            self.prepared_requests.entry(id).or_default().render = Some(false);
        }
    }

    fn execute_frame(&mut self) -> Result<RenderedFrame<A>, String> {
        let retained = self.partial_composition && !self.force_redraw
            && self.output_identity.is_some() && self.output_identity == self.current_output_identity()
            && self.last_output.as_ref().map_or(false, |output| self.gpu.has_owned_output(output));
        self.output_identity = None;
        let start = std::time::Instant::now();
        let document = self.document.as_mut().ok_or("No prepared WR frame")?;
        let frame = &document.frame;
        let state = &frame.composite_state;
        let partial_damage = if retained && frame.present && !frame.has_been_rendered
            && state.dirty_rects_are_valid && self.last_descriptor.as_ref() == Some(&state.descriptor)
            && frame.deferred_resolves.is_empty() && state.external_surfaces.is_empty() {
            state.tiles.iter().filter_map(|tile| {
                if tile.local_dirty_rect.is_empty() { return None; }
                state.get_device_rect(&tile.local_dirty_rect, tile.transform_index)
                    .intersection(&tile.device_clip_rect)
                    .map(|rect| rect.round_out().to_i32())
                    .and_then(|rect| rect.intersection(&frame.device_rect))
            }).reduce(|a, b| a.union(&b))
                .filter(|rect| !rect.is_empty() && *rect != frame.device_rect)
        } else { None };
        let damage = if !frame.present || frame.device_rect.is_empty() { Vec::new() }
            else { vec![partial_damage.unwrap_or(frame.device_rect)] };
        let did_rasterize = !frame.has_been_rendered && state.did_rasterize_any_tile;
        let output = if let Some(damage) = partial_damage {
            self.gpu.render_retained(&mut document.frame, self.clear_color, self.last_output.as_ref().unwrap(), damage)?
        } else {
            self.gpu.render(&mut document.frame, Vec::new(), self.clear_color)?
        };
        if let Some(metrics) = self.gpu.metrics() {
            metrics.set(crate::device::hal::diagnostics::RenderGauge::RetainedOutputBytes,
                output.size[0] as u64 * output.size[1] as u64 * 4);
        }
        if document.frame.present {
            self.gpu.end_compositor_frame(&document.frame, FrameCompletion { owner: self.backend_id, serial: output.serial })?;
        }
        document.frame.has_been_rendered = true;
        if document.frame.present && !document.frame.device_rect.is_empty() {
            self.last_descriptor = Some(document.frame.composite_state.descriptor.clone());
            self.last_device_rect = Some(document.frame.device_rect);
        }
        self.damage = damage;
        self.did_rasterize = did_rasterize;
        self.last_upload_time = std::mem::replace(&mut self.resource_upload_time, Duration::ZERO);
        self.last_upload_bytes = self.gpu.take_resource_upload_bytes();
        self.last_compositor_surfaces = [
            document.profile.get_or(crate::profiler::COMPOSITOR_SURFACE_OVERLAYS, 0.0) as usize,
            document.profile.get_or(crate::profiler::COMPOSITOR_SURFACE_UNDERLAYS, 0.0) as usize,
        ];
        document.profile.clear();
        let composite_time = start.elapsed();
        if self.cpu_timings.len() == 64 { self.cpu_timings.pop_front(); }
        self.cpu_timings.push_back(CpuTiming { completion: FrameCompletion { owner: self.backend_id, serial: output.serial },
            composite_time, resource_upload_time: self.last_upload_time,
            is_slow: composite_time + self.last_upload_time >= self.slow_cpu_frame_threshold });
        self.notify(Checkpoint::FrameRendered);
        self.force_redraw = false;
        Ok(output)
    }

    pub fn render_frame(&mut self) -> Result<FrameOutput, String> {
        let output = self.execute_frame()?;
        let size = output.size;
        let stats = output.stats;
        self.store_output(output);
        let pixels = if size[0] == 0 || size[1] == 0 {
            Vec::new()
        } else {
            let rect = api::units::FramebufferIntRect::from_size(
                api::units::FramebufferIntSize::new(size[0] as i32, size[1] as i32),
            );
            let handle = self.request_readback_inner(rect, false)?;
            self.wait_readback(handle)?
        };
        Ok(FrameOutput { size, pixels, stats })
    }

    pub fn render(&mut self) -> Result<super::RenderResults, String> {
        let output = self.execute_frame()?;
        if !crate::device::hal::diagnostics::quiet() {
            log::debug!("HAL rendered WR frame: {:?}", output.stats);
        }
        let document = self.document.as_mut().unwrap();
        let mut results = super::RenderResults::default();
        results.stats.total_draw_calls = output.stats.wr_draw_calls;
        results.stats.color_target_count = output.stats.color_targets;
        results.stats.alpha_target_count = output.stats.alpha_targets;
        results.stats.resource_upload_time = self.last_upload_time.as_secs_f64() * 1000.0;
        results.stats.texture_upload_mb = self.last_upload_bytes as f64 / (1024.0 * 1024.0);
        results.compositor_surface_overlays = self.last_compositor_surfaces[0];
        results.compositor_surface_underlays = self.last_compositor_surfaces[1];
        results.dirty_rects.extend_from_slice(&self.damage);
        results.did_rasterize_any_tile = self.did_rasterize;
        results.picture_cache_debug = std::mem::replace(
            &mut document.frame.composite_state.picture_cache_debug,
            crate::tile_cache::PictureCacheDebugInfo::new(),
        );
        if let Some(stats) = document.frame_stats.take() {
            results.stats.merge(&stats);
        }
        self.store_output(output);
        Ok(results)
    }

    pub fn read_pixels_rgba8(
        &self,
        rect: api::units::FramebufferIntRect,
    ) -> Result<Vec<u8>, String> {
        self.wait_readback(self.request_readback(rect)?)
    }

    pub fn frame_completion(&self) -> Option<FrameCompletion> {
        self.last_output.as_ref().map(|output| FrameCompletion {
            owner: self.backend_id,
            serial: output.serial,
        })
    }

    pub fn poll_completion(&self, completion: FrameCompletion) -> Result<bool, String> {
        if completion.owner != self.backend_id { return Err("HAL completion belongs to another renderer".into()); }
        self.gpu.poll_completion(completion.serial)
    }

    pub fn request_readback(&self, rect: api::units::FramebufferIntRect) -> Result<ReadbackHandle, String> {
        self.request_readback_inner(rect, true)
    }

    fn request_readback_inner(&self, rect: api::units::FramebufferIntRect, flip_rows: bool) -> Result<ReadbackHandle, String> {
        let output = self
            .last_output
            .as_ref()
            .ok_or("HAL frame has not been rendered")?;
        let [width, height] = output.size;
        if rect.min.x < 0
            || rect.min.y < 0
            || rect.max.x as u32 > width
            || rect.max.y as u32 > height
            || rect.is_empty()
        {
            return Err("Invalid HAL readback rectangle".into());
        }
        let bytes = self.gpu.readback_bytes(rect.width() as u32, rect.height() as u32)?;
        if self.readbacks.borrow().len() >= 8 || bytes > (64 << 20) - self.readback_bytes.get() {
            return Err("HAL readback request budget exhausted".into());
        }
        let id = self.next_readback.get();
        let next = id.checked_add(1).ok_or("HAL readback handle overflow")?;
        let source_rect = api::units::DeviceIntRect::from_origin_and_size(
            api::units::DeviceIntPoint::new(rect.min.x, height as i32 - rect.max.y),
            api::units::DeviceIntSize::new(rect.width(), rect.height()),
        );
        let ticket = self.gpu.start_readback(output, source_rect)?;
        let handle = ReadbackHandle { owner: self.backend_id, id };
        self.readbacks.borrow_mut().insert(handle, ReadbackRequest { ticket, flip_rows, format: ImageFormat::RGBA8, kind: ReadbackKind::Pixels });
        self.readback_bytes.set(self.readback_bytes.get() + bytes);
        self.next_readback.set(next);
        Ok(handle)
    }

    pub fn poll_readback(&self, handle: ReadbackHandle) -> Result<Option<Vec<u8>>, String> {
        self.readback_result(handle, false)
    }

    pub fn wait_readback(&self, handle: ReadbackHandle) -> Result<Vec<u8>, String> {
        self.readback_result(handle, true)?.ok_or_else(|| "HAL readback did not complete".into())
    }

    fn readback_result(&self, handle: ReadbackHandle, wait: bool) -> Result<Option<Vec<u8>>, String> {
        if handle.owner != self.backend_id { return Err("HAL readback belongs to another renderer".into()); }
        let mut requests = self.readbacks.borrow_mut();
        if self.gpu.is_failed() {
            requests.clear();
            self.readback_bytes.set(0);
            return Err("HAL readbacks cancelled after renderer failure".into());
        }
        let request = requests.get(&handle).ok_or("Unknown or released HAL readback")?;
        let result = self.gpu.poll_readback(&request.ticket, wait);
        if let Err(error) = result {
            requests.clear();
            self.readback_bytes.set(0);
            return Err(error);
        }
        let Some(mut pixels) = result.unwrap() else { return Ok(None); };
        if request.flip_rows {
            let [width, height] = request.ticket.size;
            let stride = width as usize * 4;
            for y in 0..height as usize / 2 {
                let (top, bottom) = pixels.split_at_mut((height as usize - 1 - y) * stride);
                top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
            }
        }
        if request.format == ImageFormat::BGRA8 {
            for pixel in pixels.chunks_exact_mut(4) { pixel.swap(0, 2); }
        }
        self.readback_bytes.set(self.readback_bytes.get() - request.ticket.bytes());
        requests.remove(&handle);
        Ok(Some(pixels))
    }

    pub fn cancel_readback(&self, handle: ReadbackHandle) -> Result<(), String> {
        if handle.owner != self.backend_id { return Err("HAL readback belongs to another renderer".into()); }
        let request = self.readbacks.borrow_mut().remove(&handle).ok_or("Unknown or released HAL readback")?;
        self.readback_bytes.set(self.readback_bytes.get() - request.ticket.bytes());
        Ok(())
    }

    pub fn get_screenshot_async(&mut self, window_rect: api::units::DeviceIntRect, buffer_size: api::units::DeviceIntSize,
                                format: ImageFormat) -> Result<(ScreenshotHandle, api::units::DeviceIntSize), String> {
        if !matches!(format, ImageFormat::RGBA8 | ImageFormat::BGRA8) || window_rect.is_empty() || buffer_size.is_empty() {
            return Err("Unsupported screenshot format or dimensions".into());
        }
        let scale = (buffer_size.width as f32 / window_rect.width() as f32)
            .min(buffer_size.height as f32 / window_rect.height() as f32);
        let size = (window_rect.size().to_f32() * scale).round().to_i32();
        if size.is_empty() { return Err("Screenshot size rounds to zero".into()); }
        let bytes = self.gpu.readback_bytes(size.width as u32, size.height as u32)?;
        if self.readbacks.borrow().len() >= 8 || bytes > (64 << 20) - self.readback_bytes.get() {
            return Err("HAL readback request budget exhausted".into());
        }
        let id = self.next_readback.get();
        let next = id.checked_add(1).ok_or("HAL readback handle overflow")?;
        let frame = self.last_output.as_ref().ok_or("No rendered frame to capture")?;
        let rect = window_rect.translate(-frame.origin.to_vector());
        let ticket = self.gpu.scaled_readback(frame, rect, size)?;
        let readback = ReadbackHandle { owner: self.backend_id, id };
        self.readbacks.borrow_mut().insert(readback, ReadbackRequest {
            ticket, flip_rows: false, format, kind: ReadbackKind::Screenshot,
        });
        self.readback_bytes.set(self.readback_bytes.get() + bytes);
        self.next_readback.set(next);
        Ok((ScreenshotHandle { readback, size, format }, size))
    }

    pub fn map_and_recycle_screenshot(&self, handle: ScreenshotHandle, destination: &mut [u8], stride: usize, format: ImageFormat) -> Result<bool, String> {
        self.map_capture(handle, destination, stride, format, false)
    }

    fn map_capture(&self, handle: ScreenshotHandle, destination: &mut [u8], stride: usize, format: ImageFormat, wait: bool) -> Result<bool, String> {
        if !matches!(format, ImageFormat::RGBA8 | ImageFormat::BGRA8) { return Err("Unsupported screenshot destination format".into()); }
        let row = handle.size.width as usize * 4;
        let required = stride.checked_mul(handle.size.height as usize - 1).and_then(|offset| offset.checked_add(row))
            .ok_or("Screenshot destination size overflow")?;
        if stride < row || destination.len() < required { return Err("Screenshot destination is too small".into()); }
        let mut pixels = if wait { self.wait_readback(handle.readback)? } else {
            let Some(pixels) = self.poll_readback(handle.readback)? else { return Ok(false); };
            pixels
        };
        if format != handle.format {
            for pixel in pixels.chunks_exact_mut(4) { pixel.swap(0, 2); }
        }
        for (source, target) in pixels.chunks_exact(row).zip(destination.chunks_mut(stride)) {
            target[..row].copy_from_slice(source);
        }
        Ok(true)
    }

    pub fn record_frame(&self, format: ImageFormat) -> Result<(RecordedFrameHandle, api::units::DeviceIntSize), String> {
        if !matches!(format, ImageFormat::RGBA8 | ImageFormat::BGRA8) { return Err("Unsupported recording format".into()); }
        let output = self.last_output.as_ref().ok_or("No frame to record")?;
        let size = api::units::DeviceIntSize::new(output.size[0] as i32, output.size[1] as i32);
        let rect = api::units::FramebufferIntRect::from_size(api::units::FramebufferIntSize::new(size.width, size.height));
        let readback = self.request_readback_inner(rect, false)?;
        let mut requests = self.readbacks.borrow_mut();
        let request = requests.get_mut(&readback).unwrap();
        request.format = format;
        request.kind = ReadbackKind::Recording;
        Ok((RecordedFrameHandle(ScreenshotHandle { readback, size, format }), size))
    }

    pub fn map_recorded_frame(&self, handle: RecordedFrameHandle, destination: &mut [u8], stride: usize) -> Result<bool, String> {
        self.map_and_recycle_screenshot(handle.0, destination, stride, handle.0.format)
    }

    fn release_capture_requests(&self, kind: ReadbackKind) {
        let mut requests = self.readbacks.borrow_mut();
        requests.retain(|_, request| request.kind != kind);
        self.readback_bytes.set(requests.values().map(|request| request.ticket.bytes()).sum());
    }

    pub fn release_profiler_structures(&mut self) {
        self.release_capture_requests(ReadbackKind::Screenshot);
        self.gpu.release_capture_buffers();
    }
    pub fn release_composition_recorder_structures(&mut self) {
        self.release_capture_requests(ReadbackKind::Recording);
        self.gpu.release_capture_buffers();
    }
    pub fn supports_bgra_readback(&self) -> bool { true }
}

impl<A: BackendApi> Drop for RendererCore<A> {
    fn drop(&mut self) {
        if let Some(metrics) = self.gpu.metrics() {
            metrics.set(crate::device::hal::diagnostics::RenderGauge::RetainedOutputBytes, 0);
        }
        if let Some(sender) = self.api_tx.take() {
            let _ = sender.send(ApiMsg::UnregisterWindow(self.backend_id, None));
        }
    }
}

#[derive(Clone, Copy)]
struct ReadyFrame {
    generation: u64,
    publish: api::FramePublishId,
    present: bool,
    render_generation: u64,
}

#[derive(Default)]
struct ReadyState {
    generation: u64,
    frame: Option<DocumentId>,
    documents: HashMap<DocumentId, ReadyFrame>,
    shutdown: bool,
}

#[derive(Default)]
struct FrameReady {
    state: Mutex<ReadyState>,
    changed: Condvar,
}

impl FrameReady {
    fn publish(&self, document: DocumentId, publish: api::FramePublishId, present: bool, render: bool) {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        state.frame = Some(document);
        let generation = state.generation;
        let render_generation = if render { generation } else {
            state.documents.get(&document).map_or(0, |frame| frame.render_generation)
        };
        state.documents.insert(document, ReadyFrame { generation, publish, present, render_generation });
        self.changed.notify_all();
    }

    fn wait(
        &self,
        generation: u64,
    ) -> Result<(u64, DocumentId, api::FramePublishId, bool, bool), String> {
        let (state, _) = self
            .changed
            .wait_timeout_while(
                self.state.lock().unwrap(),
                Duration::from_secs(60),
                |state| state.generation == generation && !state.shutdown,
            )
            .unwrap();
        if state.generation == generation {
            return Err(if state.shutdown {
                "WR backend shut down"
            } else {
                "Timed out waiting for WR frame"
            }
            .into());
        }
        let document = state.frame.ok_or("No WR frame notification")?;
        let frame = state.documents[&document];
        Ok((frame.generation, document, frame.publish, frame.present, frame.render_generation > generation))
    }

    fn wait_document(&self, document: DocumentId, generation: u64) -> Result<(u64, api::FramePublishId, bool, bool), String> {
        let (state, _) = self.changed.wait_timeout_while(self.state.lock().unwrap(), Duration::from_secs(60),
            |state| state.documents.get(&document).map_or(true, |frame| frame.generation <= generation) && !state.shutdown).unwrap();
        state.documents.get(&document).filter(|frame| frame.generation > generation)
            .map(|frame| (frame.generation, frame.publish, frame.present, frame.render_generation > generation))
            .ok_or_else(|| if state.shutdown { "WR backend shut down".into() } else { "Timed out waiting for WR document".into() })
    }
}

struct FrameNotifier {
    inner: Box<dyn RenderNotifier>,
    ready: Arc<FrameReady>,
    metrics: Option<Arc<crate::device::hal::diagnostics::RenderMetrics>>,
}
impl RenderNotifier for FrameNotifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self {
            inner: self.inner.clone(),
            ready: self.ready.clone(),
            metrics: self.metrics.clone(),
        })
    }
    fn wake_up(&self, composite_needed: bool) {
        if let Some(metrics) = &self.metrics {
            use crate::device::hal::diagnostics::RenderCounter;
            metrics.add(if composite_needed { RenderCounter::WakeRender } else { RenderCounter::WakeUpdate }, 1);
        }
        self.inner.wake_up(composite_needed);
    }
    fn external_event(&self, event: api::ExternalEvent) {
        self.inner.external_event(event);
    }
    fn shut_down(&self) {
        self.ready.state.lock().unwrap().shutdown = true;
        self.ready.changed.notify_all();
        self.inner.shut_down();
    }
    fn new_frame_ready(
        &self,
        document: DocumentId,
        publish: api::FramePublishId,
        params: &api::FrameReadyParams,
    ) {
        if let Some(metrics) = &self.metrics {
            use crate::device::hal::diagnostics::RenderCounter;
            metrics.add(RenderCounter::FrameReady, 1);
            metrics.add(if params.render { RenderCounter::RenderRequested } else { RenderCounter::NoRenderRequested }, 1);
            if params.scrolled { metrics.add(RenderCounter::ScrolledRequests, 1); }
        }
        self.ready.publish(document, publish, params.present, params.render);
        self.inner.new_frame_ready(document, publish, params);
    }
}

#[cfg(all(test, wr_hal_vulkan))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ShutdownNotice(Arc<AtomicBool>);
    impl RenderNotifier for ShutdownNotice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self(self.0.clone()))
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(
            &self,
            _: DocumentId,
            _: api::FramePublishId,
            _: &api::FrameReadyParams,
        ) {
        }
        fn external_event(&self, event: api::ExternalEvent) {
            assert_eq!(event.unwrap(), 42);
            self.0.store(true, Ordering::SeqCst);
        }
        fn shut_down(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn forwards_shutdown() {
        let notified = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(FrameReady::default());
        let notifier = FrameNotifier {
            inner: Box::new(ShutdownNotice(notified.clone())),
            ready,
            metrics: None,
        };
        notifier.clone().shut_down();
        assert!(notified.load(Ordering::SeqCst));
    }
    #[test]
    fn forwards_external_events() {
        let notified = Arc::new(AtomicBool::new(false));
        let notifier = FrameNotifier {
            inner: Box::new(ShutdownNotice(notified.clone())),
            ready: Arc::new(FrameReady::default()),
            metrics: None,
        };
        notifier.clone().external_event(api::ExternalEvent::from_raw(42));
        assert!(notified.load(Ordering::SeqCst));
    }

    #[test]
    fn coalesces_ready_notifications_and_wakes_on_shutdown() {
        let ready = FrameReady::default();
        let id = DocumentId::new(api::IdNamespace(7), 1);
        for serial in 1..=1000 {
            ready.publish(id, api::FramePublishId(serial), true, true);
        }
        let (generation, document, publish, present, render) = ready.wait(0).unwrap();
        assert!(present);
        assert!(render);
        assert_eq!((generation, document, publish.0), (1000, id, 1000));
        ready.state.lock().unwrap().shutdown = true;
        assert!(ready.wait(generation).unwrap_err().contains("shut down"));
    }

    #[test]
    fn ready_render_requests_survive_coalescing_per_document() {
        let ready = FrameReady::default();
        let first = DocumentId::new(api::IdNamespace(7), 1);
        let second = DocumentId::new(api::IdNamespace(7), 2);
        let publish = api::FramePublishId(42);
        ready.publish(first, publish, true, true);
        ready.publish(second, publish, false, false);
        ready.publish(first, publish, true, false);
        let (generation, latest, present, render) = ready.wait_document(first, 0).unwrap();
        assert_eq!(latest, publish);
        assert!(present && render);
        let (_, _, present, render) = ready.wait_document(second, 0).unwrap();
        assert!(!present && !render);
        ready.publish(first, publish, false, false);
        let (consumed, _, present, render) = ready.wait_document(first, generation).unwrap();
        assert!(!present && !render);
        ready.publish(first, publish, false, true);
        ready.publish(first, publish, true, false);
        let (_, _, present, render) = ready.wait_document(first, consumed).unwrap();
        assert!(present && render);
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn prepared_frame_decisions_preserve_explicit_invalidation() {
        use api::units::*;
        use api::*;
        use crate::render_api::Transaction;
        let (mut renderer, sender) = create_vulkan_renderer(&Options { validation: true, ..Default::default() },
            WebRenderOptions::default(), Box::new(ShutdownNotice(Arc::new(AtomicBool::new(false))))).unwrap();
        let mut api = sender.create_api();
        let document = api.add_document(DeviceIntSize::new(16, 16));
        let pipeline = PipelineId(0, 0);
        let rect = LayoutRect::from_size(LayoutSize::new(16.0, 16.0));
        let info = CommonItemProperties { clip_rect: rect, clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(pipeline), flags: PrimitiveFlags::default() };
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        let first = renderer.prepare_frame(document).unwrap();
        assert!(first.render && first.present);
        let pixels = renderer.render_frame().unwrap().pixels;
        assert_eq!(pixels.len(), 16 * 16 * 4);
        assert!(pixels.chunks_exact(4).all(|pixel| pixel == [255, 0, 0, 255]));

        let mut transaction = Transaction::new();
        transaction.generate_frame(2, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        let unchanged = renderer.prepare_frame(document).unwrap();
        assert!(!unchanged.render && unchanged.present);
        assert!(renderer.prepare_frame_if_ready(document).unwrap().is_none());

        let mut transaction = Transaction::new();
        transaction.invalidate_rendered_frame(RenderReasons::TESTING);
        transaction.generate_frame(3, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        let invalidated = renderer.prepare_frame(document).unwrap();
        assert!(invalidated.render && invalidated.present);
        assert!(renderer.core.document.as_ref().unwrap().frame.has_been_rendered);
        assert_eq!(renderer.render_frame().unwrap().pixels, pixels);
        api.delete_document(document);
    }

    struct Checkpoints(Arc<Mutex<Vec<Checkpoint>>>);
    impl api::NotificationHandler for Checkpoints {
        fn notify(&self, checkpoint: Checkpoint) {
            self.0.lock().unwrap().push(checkpoint);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn failures_cancel_pending_reads_and_release_images() {
        use api::units::*;
        use api::*;
        use crate::device::hal::{FailurePoint, ExternalImageProvider, ExternalImageLease, ExternalImageSource, ExternalImageRelease, NativeImage};
        use crate::render_api::Transaction;
        use std::rc::Rc;
        struct Provider { image: NativeImage, fail: Rc<Cell<bool>>, releases: Rc<RefCell<Vec<ExternalImageRelease>>> }
        impl ExternalImageProvider for Provider {
            fn acquire(&mut self, _: ExternalImageId, _: u8, _: bool) -> Result<ExternalImageLease, String> {
                if self.fail.get() { return Err("Injected provider acquisition failure".into()); }
                let releases = self.releases.clone();
                ExternalImageLease::new(self.image.descriptor(), TexelRect::new(0.0, 0.0, 4.0, 4.0), self.image.generation(),
                    ExternalImageSource::Native(self.image.clone()), move |status| releases.borrow_mut().push(status))
            }
        }
        for fault in [Some(FailurePoint::Record), Some(FailurePoint::Submit), Some(FailurePoint::Map), None] {
            let (mut renderer, sender) = create_vulkan_renderer(&Options { validation: true, ..Default::default() },
                WebRenderOptions::default(), Box::new(ShutdownNotice(Arc::new(AtomicBool::new(false))))).unwrap();
            let mut api = sender.create_api();
            let doc = api.add_document(DeviceIntSize::new(16, 16));
            let pipeline = PipelineId(0, 0);
            let descriptor = ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
            let producer = renderer.external_image_device();
            let native = producer.create_image(descriptor, &[255, 0, 0, 255].repeat(16)).unwrap();
            let releases = Rc::new(RefCell::new(Vec::new()));
            let fail = Rc::new(Cell::new(false));
            renderer.set_external_image_provider(Box::new(Provider { image: native, fail: fail.clone(), releases: releases.clone() })).unwrap();
            let key = api.generate_image_key();
            let external = ImageData::External(ExternalImageData { id: ExternalImageId(12), channel_index: 0,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D), normalized_uvs: false });
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.0);
            let rect = LayoutRect::from_size(LayoutSize::new(16.0, 16.0));
            let info = CommonItemProperties { clip_rect: rect, clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(pipeline), flags: PrimitiveFlags::default() };
            builder.push_image(&info, rect, ImageRendering::Pixelated, AlphaType::PremultipliedAlpha, key, ColorF::WHITE);
            let mut txn = Transaction::new();
            txn.add_image(key, descriptor, external.clone(), None);
            txn.set_root_pipeline(pipeline);
            txn.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
            txn.generate_frame(0, true, false, RenderReasons::TESTING);
            api.send_transaction(doc, txn);
            renderer.prepare_frame(doc).unwrap();
            let first = renderer.render_frame().unwrap();
            assert_eq!(&first.pixels[..4], &[255, 0, 0, 255]);
            assert_eq!(*releases.borrow(), [ExternalImageRelease::Complete]);
            let rect = FramebufferIntRect::from_size(FramebufferIntSize::new(16, 16));
            let pending = renderer.request_readback(rect).unwrap();
            let second = renderer.request_readback(rect).unwrap();
            if fault == Some(FailurePoint::Map) {
                renderer.inject_failure(FailurePoint::Map);
                assert!(renderer.wait_readback(pending).is_err());
            } else {
                let mut txn = Transaction::new();
                txn.update_image(key, descriptor, external, &DirtyRect::All);
                txn.generate_frame(1, true, false, RenderReasons::TESTING);
                api.send_transaction(doc, txn);
                renderer.prepare_frame(doc).unwrap();
                if let Some(point) = fault { renderer.inject_failure(point); } else { fail.set(true); }
                assert!(renderer.render().is_err());
                if fault == Some(FailurePoint::Submit) { assert_eq!(releases.borrow().last(), Some(&ExternalImageRelease::Abandoned)); }
                if fault == Some(FailurePoint::Record) { assert_eq!(releases.borrow().last(), Some(&ExternalImageRelease::Unused)); }
            }
            assert!(renderer.is_failed());
            assert!(renderer.poll().is_err());
            assert!(renderer.wait_readback(second).is_err());
            assert!(renderer.core.readbacks.borrow().is_empty());
            assert_eq!(renderer.core.readback_bytes.get(), 0);
            assert!(renderer.request_readback(rect).is_err());
            assert!(renderer.render().is_err());
            api.shut_down(true);
            drop(renderer);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn external_images_update_crop_flip_and_release() { external_images_roundtrip(false); }

    #[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
    #[test]
    #[ignore = "Requires DMA-BUF-capable Vulkan hardware"]
    fn dmabuf_images_render_crop_flip_and_release() { external_images_roundtrip(true); }

    fn external_images_roundtrip(dmabuf: bool) {
        use api::units::*;
        use api::*;
        use crate::device::hal::{ExternalImageLease, ExternalImageProvider, ExternalImageRelease, ExternalImageSource};
        use crate::render_api::Transaction;
        use std::rc::Rc;
        struct ImageState {
            source: ExternalImageSource,
            descriptor: ImageDescriptor,
            uv: TexelRect,
            generation: u64,
        }
        struct Provider {
            image: Rc<RefCell<ImageState>>,
            acquired: Rc<Cell<usize>>,
            released: Rc<RefCell<Vec<ExternalImageRelease>>>,
        }
        impl ExternalImageProvider for Provider {
            fn acquire(&mut self, id: ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease, String> {
                assert_eq!((id.0, channel), (19, 1));
                self.acquired.set(self.acquired.get() + 1);
                let image = self.image.borrow();
                let released = self.released.clone();
                ExternalImageLease::new(image.descriptor, image.uv, image.generation, image.source.clone(),
                    move |status| released.borrow_mut().push(status))
            }
        }
        let (mut renderer, sender) = create_vulkan_renderer(
            &Options { validation: true, ..Options::default() }, WebRenderOptions::default(),
            Box::new(ShutdownNotice(Arc::new(AtomicBool::new(false)))),
        ).unwrap();
        let mut api = sender.create_api();
        let document = api.add_document(DeviceIntSize::new(17, 9));
        let pipeline = PipelineId(0, 0);
        let key = api.generate_image_key();
        let producer = renderer.external_image_device();
        #[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
        let exporting_device = if dmabuf {
            if !producer.dmabuf_capabilities().unwrap().supported() {
                println!("DMA-BUF rendering unavailable on this driver");
                api.shut_down(true);
                return;
            }
            Some(crate::hal::create_vulkan_image_device(&Options { validation: true, ..Default::default() }).unwrap())
        } else { None };
        let create_image = |descriptor, data: &[u8]| {
            #[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
            if let Some(exporter) = &exporting_device {
                let original = exporter.create_image(descriptor, data).unwrap();
                let export = exporter.export_dmabuf_image(&original, 0).unwrap();
                let copied = unsafe { producer.copy_dmabuf_planes(std::slice::from_ref(export.plane()), export.ready()) }.unwrap();
                return copied.images()[0].clone();
            }
            producer.create_image(descriptor, data).unwrap()
        };
        let descriptor = ImageDescriptor::new(7, 5, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
        let pixels = |serial: u8| -> Vec<u8> {
            (0..5u8).flat_map(|y| (0..7u8).flat_map(move |x| [x * 31, y * 47, 17 + serial, 255])).collect()
        };
        let image = Rc::new(RefCell::new(ImageState {
            source: ExternalImageSource::Native(create_image(descriptor, &pixels(0))),
            descriptor, uv: TexelRect::new(0.0, 0.0, 7.0, 5.0), generation: 0,
        }));
        let acquired = Rc::new(Cell::new(0));
        let released = Rc::new(RefCell::new(Vec::new()));
        renderer.set_external_image_provider(Box::new(Provider {
            image: image.clone(), acquired: acquired.clone(), released: released.clone(),
        })).unwrap();
        for serial in 0..4u8 {
            let data = pixels(serial);
            let uv = match serial {
                1 => TexelRect::new(7.0, 5.0, 0.0, 0.0),
                2 => TexelRect::new(1.0, 1.0, 6.0, 4.0),
                _ => TexelRect::new(0.0, 0.0, 7.0, 5.0),
            };
            {
                let mut state = image.borrow_mut();
                if serial == 3 {
                    state.source = ExternalImageSource::Buffer(Arc::new(data.clone()));
                } else if serial == 2 || dmabuf {
                    state.source = ExternalImageSource::Native(create_image(descriptor, &data));
                } else if let ExternalImageSource::Native(native) = &state.source {
                    producer.update_image(native, descriptor, &data).unwrap();
                }
                state.uv = uv;
                state.generation = serial as u64;
            }
            let mut transaction = Transaction::new();
            let external = ImageData::External(ExternalImageData {
                id: ExternalImageId(19), channel_index: 1,
                image_type: if serial == 3 { ExternalImageType::Buffer }
                    else { ExternalImageType::TextureHandle(if serial == 2 { ImageBufferKind::TextureRect } else { ImageBufferKind::Texture2D }) },
                normalized_uvs: serial == 1,
            });
            if serial == 0 {
                transaction.add_image(key, descriptor, external, None);
                let mut builder = DisplayListBuilder::new(pipeline);
                builder.begin(60.0);
                let info = CommonItemProperties {
                    clip_rect: LayoutRect::from_size(LayoutSize::new(17.0, 9.0)),
                    clip_chain_id: ClipChainId::INVALID, spatial_id: SpatialId::root_scroll_node(pipeline),
                    flags: PrimitiveFlags::default(),
                };
                builder.push_stacking_context(info.spatial_id, info.flags, None, TransformStyle::Flat,
                    MixBlendMode::Normal, &[], &[], RasterSpace::Screen, StackingContextFlags::empty(), None);
                builder.push_image(&info, LayoutRect::from_origin_and_size(LayoutPoint::new(3.0, 2.0), LayoutSize::new(7.0, 5.0)),
                    ImageRendering::Pixelated, AlphaType::PremultipliedAlpha, key, ColorF::WHITE);
                builder.pop_stacking_context();
                transaction.set_root_pipeline(pipeline);
                transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
            } else {
                transaction.update_image(key, descriptor, external, &DirtyRect::All);
            }
            transaction.generate_frame(serial as u64, true, false, RenderReasons::TESTING);
            api.send_transaction(document, transaction);
            renderer.prepare_frame(document).unwrap();
            renderer.render().unwrap();
            if serial == 3 { assert!(renderer.core.last_upload_bytes >= 7 * 5 * 4); }
            if serial == 0 {
                assert!(released.borrow().is_empty());
                if let ExternalImageSource::Native(native) = &image.borrow().source {
                    assert!(producer.update_image(native, descriptor, &data).unwrap_err().contains("acquired"));
                }
            }
            let bottom_up = renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(17, 9))).unwrap();
            let output: Vec<u8> = bottom_up.chunks_exact(17 * 4).rev().flatten().copied().collect();
            for y in 0..5usize {
                for x in 0..7usize {
                    let sx = (uv.uv0.x + (x as f32 + 0.5) / 7.0 * (uv.uv1.x - uv.uv0.x)).floor() as usize;
                    let sy = (uv.uv0.y + (y as f32 + 0.5) / 5.0 * (uv.uv1.y - uv.uv0.y)).floor() as usize;
                    let expected = &data[(sy * 7 + sx) * 4..(sy * 7 + sx + 1) * 4];
                    let offset = ((y + 2) * 17 + x + 3) * 4;
                    assert_eq!(&output[offset..offset + 4], expected, "frame {serial}, pixel {x},{y}");
                }
            }
            renderer.poll().unwrap();
            assert_eq!(released.borrow().len(), acquired.get());
            assert!(released.borrow().iter().all(|status| *status == ExternalImageRelease::Complete));
        }
        assert!(acquired.get() >= 4);
        api.shut_down(true);
    }

    #[test]
    #[ignore = "Requires a Vulkan ICD and validation layer"]
    fn queued_publications_resources_and_checkpoints() {
        use api::units::*;
        use api::*;
        use crate::render_api::{RenderApi, Transaction};
        let (mut renderer, sender) = create_vulkan_renderer(
            &Options {
                validation: true,
                ..Options::default()
            },
            WebRenderOptions::default(),
            Box::new(ShutdownNotice(Arc::new(AtomicBool::new(false)))),
        )
        .unwrap();
        assert!(renderer.enable_gpu_profiling(true));
        renderer.core.process_message(ResultMsg::SetParameter(Parameter::Float(FloatParameter::SlowCpuFrameThreshold, 0.0))).unwrap();
        assert!(renderer.core.process_message(ResultMsg::SetParameter(Parameter::Float(FloatParameter::SlowCpuFrameThreshold, f32::NAN))).is_err());
        for parameter in [BoolParameter::PboUploads, BoolParameter::BatchedUploads, BoolParameter::DrawCallsForTextureCopy] {
            for enabled in [false, true] {
                renderer.core.process_message(ResultMsg::SetParameter(Parameter::Bool(parameter, enabled))).unwrap();
            }
        }
        renderer.core.process_message(ResultMsg::SetParameter(Parameter::Int(api::IntParameter::BatchedUploadThreshold, 65536))).unwrap();
        let mut api = sender.create_api();
        let id = api.add_document(DeviceIntSize::new(64, 64));
        let pipeline = PipelineId(0, 0);
        let send = |api: &mut RenderApi, serial: u64, present: bool| {
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.0);
            let info = CommonItemProperties {
                clip_rect: LayoutRect::from_size(LayoutSize::new(64.0, 64.0)),
                clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(pipeline),
                flags: PrimitiveFlags::default(),
            };
            builder.push_rect(&info, info.clip_rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
            builder.push_rect(
                &info,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(8.0, 8.0),
                    LayoutSize::new(16.0, 16.0),
                ),
                ColorF::new(0.0, 0.0, 1.0, 1.0),
            );
            let mut transaction = Transaction::new();
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(
                Epoch(serial as u32),
                api.get_namespace_id(),
                builder.end(),
            );
            transaction.generate_frame(serial, present, false, RenderReasons::TESTING);
            api.send_transaction(id, transaction);
        };
        let mut generation = 0;
        for serial in 1..=3 {
            send(&mut api, serial, true);
            generation = renderer.core.ready.wait(generation).unwrap().0;
        }
        renderer.prepare_frame(id).unwrap();
        assert_eq!(renderer.core.ready_generation, generation);
        let output = renderer.render_frame().unwrap();
        assert_eq!(&output.pixels[..4], &[255, 0, 0, 255]);
        let completion = renderer.frame_completion().unwrap();
        let timings = renderer.take_gpu_timings().unwrap();
        assert!(timings.iter().any(|timing| timing.completion == completion));
        assert!(timings.iter().all(|timing| timing.nanoseconds.is_finite() && timing.nanoseconds >= 0.0));
        let cpu_timings = renderer.take_cpu_timings();
        assert_eq!(cpu_timings.last().unwrap().completion, completion);
        assert!(cpu_timings.last().unwrap().is_slow);
        let original_rect = renderer.core.document.as_ref().unwrap().frame.device_rect;
        renderer.core.document.as_mut().unwrap().frame.device_rect = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(5, 7),
            DeviceIntSize::new(40, 40),
        );
        let cropped = renderer.render_frame().unwrap();
        assert_eq!(cropped.size, [40, 40]);
        assert_eq!(&cropped.pixels[..4], &[255, 0, 0, 255]);
        assert_eq!(
            &cropped.pixels[(2 * 40 + 4) * 4..(2 * 40 + 5) * 4],
            &[0, 0, 255, 255]
        );
        renderer.core.document.as_mut().unwrap().frame.device_rect = original_rect;
        renderer.render().unwrap();
        assert!(renderer.core.readbacks.borrow().is_empty());
        let completion = renderer.frame_completion().unwrap();
        let crop_rect = FramebufferIntRect::from_origin_and_size(
            FramebufferIntPoint::new(7, 37), FramebufferIntSize::new(19, 21),
        );
        let old_crop = renderer.request_readback(crop_rect).unwrap();
        let cancelled = renderer.request_readback(crop_rect).unwrap();
        let (other, _other_sender) = create_vulkan_renderer(
            &Options { validation: true, ..Options::default() },
            WebRenderOptions::default(),
            Box::new(ShutdownNotice(Arc::new(AtomicBool::new(false)))),
        ).unwrap();
        assert!(other.poll_readback(old_crop).is_err());
        assert!(other.poll_completion(completion).is_err());
        assert!(completion.is_complete_at(other.poll_completed().unwrap()).is_err());
        drop(other);
        renderer.core.document.as_mut().unwrap().frame.device_rect = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(5, 7), DeviceIntSize::new(40, 40),
        );
        renderer.render().unwrap();
        let new_rect = FramebufferIntRect::from_size(FramebufferIntSize::new(40, 40));
        let new_frame = renderer.request_readback(new_rect).unwrap();
        let expected_crop: Vec<u8> = (6..27).rev().flat_map(|y| {
            output.pixels[(y * 64 + 7) * 4..(y * 64 + 26) * 4].iter().copied()
        }).collect();
        assert_eq!(renderer.wait_readback(old_crop).unwrap(), expected_crop);
        assert!(renderer.poll_completion(completion).unwrap());
        assert!(completion.is_complete_at(renderer.poll_completed().unwrap()).unwrap());
        assert!(renderer.poll_readback(old_crop).is_err());
        renderer.cancel_readback(cancelled).unwrap();
        assert!(renderer.wait_readback(cancelled).is_err());
        let expected_new: Vec<u8> = cropped.pixels.chunks_exact(40 * 4).rev().flatten().copied().collect();
        let new_pixels = renderer.poll_readback(new_frame).unwrap()
            .unwrap_or_else(|| renderer.wait_readback(new_frame).unwrap());
        assert_eq!(new_pixels, expected_new);
        let handles: Vec<_> = (0..8).map(|_| renderer.request_readback(new_rect).unwrap()).collect();
        assert!(renderer.request_readback(new_rect).unwrap_err().contains("budget"));
        for handle in handles { renderer.cancel_readback(handle).unwrap(); }
        assert_eq!(renderer.core.readback_bytes.get(), 0);
        assert!(renderer.core.readbacks.borrow().is_empty());
        assert!(renderer.request_readback(FramebufferIntRect::zero()).is_err());
        renderer.core.document.as_mut().unwrap().frame.device_rect = original_rect;
        let pipeline_info = renderer.flush_pipeline_info();
        renderer.render().unwrap();
        let repeated = renderer.render().unwrap();
        assert_eq!(repeated.dirty_rects.as_slice(), &[original_rect]);
        assert!(!repeated.did_rasterize_any_tile);
        let repeated_completion = renderer.frame_completion().unwrap();
        assert!(matches!(renderer.render_if_needed().unwrap(), RenderOutcome::Reused));
        assert!(renderer.core.damage.is_empty());
        assert_eq!(renderer.frame_completion().unwrap(), repeated_completion);
        let (shot, shot_size) = renderer.get_screenshot_async(original_rect, DeviceIntSize::new(16, 16), ImageFormat::BGRA8).unwrap();
        assert_eq!(shot_size, DeviceIntSize::new(16, 16));
        assert!(renderer.map_and_recycle_screenshot(shot, &mut [0; 1], 1, ImageFormat::RGBA8).is_err());
        let mut screenshot = vec![0xa5; 71 * 16];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !renderer.map_and_recycle_screenshot(shot, &mut screenshot, 71, ImageFormat::RGBA8).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            renderer.poll().unwrap();
        }
        for y in 0..16 {
            for x in 0..16 {
                let expected = if (2..6).contains(&x) && (2..6).contains(&y) { [0, 0, 255, 255] } else { [255, 0, 0, 255] };
                assert_eq!(&screenshot[y * 71 + x * 4..y * 71 + x * 4 + 4], &expected);
            }
            assert!(screenshot[y * 71 + 64..(y + 1) * 71].iter().all(|&value| value == 0xa5));
        }
        let (blocking_shot, _) = renderer.get_screenshot_async(original_rect, DeviceIntSize::new(16, 16), ImageFormat::BGRA8).unwrap();
        let mut blocking_pixels = vec![0xa5; 71 * 16];
        assert!(renderer.wait_and_recycle_screenshot(blocking_shot, &mut blocking_pixels, 71, ImageFormat::RGBA8).unwrap());
        assert_eq!(blocking_pixels, screenshot);
        assert!(renderer.core.readbacks.borrow().is_empty());
        let (blocking_recording, _) = renderer.record_frame(ImageFormat::RGBA8).unwrap();
        let mut blocking_pixels = vec![0; 64 * 64 * 4];
        assert!(renderer.wait_map_recorded_frame(blocking_recording, &mut blocking_pixels, 64 * 4).unwrap());
        assert_eq!(blocking_pixels, output.pixels);
        assert!(renderer.core.readbacks.borrow().is_empty());
        let (recorded, recorded_size) = renderer.record_frame(ImageFormat::BGRA8).unwrap();
        assert_eq!(recorded_size, DeviceIntSize::new(64, 64));
        renderer.core.document.as_mut().unwrap().frame.device_rect = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(5, 7), DeviceIntSize::new(40, 40),
        );
        renderer.render().unwrap();
        let mut recorded_pixels = vec![0; 64 * 64 * 4];
        while !renderer.map_recorded_frame(recorded, &mut recorded_pixels, 64 * 4).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            renderer.poll().unwrap();
        }
        let mut expected_recording = output.pixels.clone();
        for pixel in expected_recording.chunks_exact_mut(4) { pixel.swap(0, 2); }
        assert_eq!(recorded_pixels, expected_recording);
        let capture_rect = renderer.core.document.as_ref().unwrap().frame.device_rect;
        let (cancelled_shot, _) = renderer.get_screenshot_async(capture_rect, DeviceIntSize::new(8, 8), ImageFormat::RGBA8).unwrap();
        renderer.release_profiler_structures();
        assert!(renderer.poll_readback(cancelled_shot.readback).is_err());
        let (cancelled_recording, _) = renderer.record_frame(ImageFormat::RGBA8).unwrap();
        renderer.release_composition_recorder_structures();
        assert!(renderer.poll_readback(cancelled_recording.0.readback).is_err());
        renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(40, 40))).unwrap();
        renderer.poll().unwrap();
        let before_trim = renderer.memory_stats();
        renderer.core.gpu.trim_transient_resources(true).unwrap();
        let after_trim = renderer.memory_stats();
        assert_eq!(after_trim.cached_buffer_bytes, 0);
        assert_eq!(after_trim.cached_texture_bytes, 0);
        assert!(after_trim.texture_bytes < before_trim.texture_bytes);
        assert!(after_trim.buffer_bytes < before_trim.buffer_bytes);
        let report = renderer.report_memory();
        assert_eq!(report.gpu.buffer_bytes, after_trim.buffer_bytes);
        assert_eq!(report.gpu.texture_bytes, after_trim.texture_bytes);
        assert!(report.cpu.frame_allocator > 0);
        renderer.core.document.as_mut().unwrap().frame.device_rect = original_rect;
        assert_eq!(pipeline_info.epochs[&(pipeline, id)], Epoch(3));
        assert!(renderer.flush_pipeline_info().epochs.is_empty());

        let checkpoints = Arc::new(Mutex::new(Vec::new()));
        let mut transaction = Transaction::new();
        for when in [Checkpoint::FrameTexturesUpdated, Checkpoint::FrameRendered] {
            transaction.notify(NotificationRequest::new(
                when,
                Box::new(Checkpoints(checkpoints.clone())),
            ));
        }
        api.send_transaction(id, transaction);
        api.flush_scene_builder();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while checkpoints.lock().unwrap().is_empty() {
            renderer.update().unwrap();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            *checkpoints.lock().unwrap(),
            [Checkpoint::FrameTexturesUpdated]
        );
        renderer.render_frame().unwrap();
        assert_eq!(
            *checkpoints.lock().unwrap(),
            [Checkpoint::FrameTexturesUpdated, Checkpoint::FrameRendered]
        );
        assert!(renderer.core.notifications.is_empty());

        send(&mut api, 4, false);
        renderer.prepare_frame(id).unwrap();
        assert!(renderer.render_frame().unwrap().pixels.is_empty());
        send(&mut api, 5, true);
        renderer.prepare_frame(id).unwrap();
        let mut document = renderer.core.document.take().unwrap();
        document.frame.has_texture_cache_tasks = true;
        document.frame.has_been_rendered = false;
        renderer.core.document = Some(document);
        renderer
            .core.process_message(ResultMsg::UpdateResources {
                resource_updates: ResourceUpdateList {
                    native_surface_updates: Vec::new(),
                    texture_updates: crate::internal_types::TextureUpdateList::new(),
                },
                memory_pressure: false,
                discard_active_documents: true,
                trim_upload_buffers: true,
            })
            .unwrap();
        assert!(renderer.core.document.is_none());
        assert!(renderer.core.last_output.is_none());
        renderer.update().unwrap();
        send(&mut api, 6, true);
        renderer.prepare_frame(id).unwrap();
        let mut offscreen = renderer.core.document.take().unwrap();
        offscreen.frame.device_rect = DeviceIntRect::zero();
        renderer
            .core.process_message(ResultMsg::RenderDocumentOffscreen(
                id,
                offscreen,
                ResourceUpdateList {
                    native_surface_updates: Vec::new(),
                    texture_updates: crate::internal_types::TextureUpdateList::new(),
                },
            ))
            .unwrap();
        assert!(renderer.core.document.is_none());
        assert!(renderer.core.last_output.is_none());
        let completion = renderer.submit_work().unwrap();
        assert!(completion.serial > 0);
        assert!(renderer.frame_completion().is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !renderer.poll_completion(completion).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        for epoch in 0..1000 {
            let mut info = PipelineInfo::default();
            info.epochs.insert((pipeline, id), Epoch(epoch));
            renderer
                .core.process_message(ResultMsg::PublishPipelineInfo(info))
                .unwrap();
        }
        assert_eq!(renderer.flush_pipeline_info().epochs.len(), 1);
        assert!(renderer.flush_pipeline_info().epochs.is_empty());
        send(&mut api, 7, true);
        renderer.core.ready.wait(renderer.core.ready_generation).unwrap();
        api.shut_down(true);
        renderer.update().unwrap();
        renderer.core.ready_generation = renderer.core.ready.state.lock().unwrap().generation;
        assert!(renderer.core.ready.wait(renderer.core.ready_generation).is_err());
    }
}
