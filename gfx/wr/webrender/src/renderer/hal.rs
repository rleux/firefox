/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::init::{self, BackendConnection, BackendResourceOptions};
use super::{PipelineInfo, WebRenderOptions};
use crate::api::{
    DocumentId, FontRenderMode, ImageFormat, NotificationRequest, Parameter, RenderBackendId,
    RenderNotifier,
};
use crate::api::channel::{unbounded_channel, Receiver, Sender};
use crate::composite::{CompositorConfig, CompositorKind};
use crate::device::hal::{create_vulkan_device, Options, FrameOutput};
use crate::device::hal::render::FrameRenderer;
use crate::frame_builder::FrameBuilderConfig;
use crate::internal_types::{RenderedDocument, ResourceUpdateList, ResultMsg};
use crate::render_api::{ApiMsg, RenderApiSender};
use crate::render_backend_pool::RenderBackendPool;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const MAX_DEPTH_IDS: i32 = 1 << 24;

#[derive(Debug)]
pub struct PreparedFrameInfo {
    pub passes: usize,
    pub picture_tiles: usize,
    pub primitive_instances: usize,
}

pub struct Renderer {
    gpu: FrameRenderer<wgpu_hal::api::Vulkan>,
    ready_rx: Receiver<DocumentId>,
    clear_color: api::ColorF,
    last_output: Option<FrameOutput>,
    result_rx: Receiver<ResultMsg>,
    api_tx: Option<Sender<ApiMsg>>,
    backend_id: RenderBackendId,
    pub(crate) pending_updates: Vec<ResourceUpdateList>,
    pub(crate) document: Option<RenderedDocument>,
    pub(crate) notifications: Vec<NotificationRequest>,
    pub(crate) parameters: Vec<Parameter>,
    pipeline_info: Vec<PipelineInfo>,
    force_redraw: bool,
    _pool: Arc<RenderBackendPool>,
}

pub fn create_vulkan_renderer(
    hal_options: &Options,
    mut options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
) -> Result<(Renderer, RenderApiSender), String> {
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
    let device = create_vulkan_device(hal_options)?;
    let max_internal_texture_size = options
        .max_internal_texture_size
        .unwrap_or(device.max_texture_size())
        .min(device.max_texture_size());
    if max_internal_texture_size < 2048 {
        return Err("HAL texture limit is below WebRender's minimum of 2048".into());
    }
    let config = FrameBuilderConfig {
        default_font_render_mode: if options.enable_aa {
            FontRenderMode::Alpha
        } else {
            FontRenderMode::Mono
        },
        dual_source_blending_is_supported: false,
        testing: options.testing,
        gpu_supports_fast_clears: false,
        gpu_supports_advanced_blend: false,
        advanced_blend_is_coherent: false,
        gpu_supports_render_target_partial_update: true,
        external_images_require_copy: true,
        batch_lookback_count: WebRenderOptions::BATCH_LOOKBACK_COUNT,
        background_color: Some(options.clear_color),
        compositor_kind: CompositorKind::default(),
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
    let gpu = FrameRenderer::new(device)?;
    let (ready_tx, ready_rx) = unbounded_channel();
    let notifier = Box::new(FrameNotifier {
        inner: notifier,
        ready_tx,
    });
    let (result_tx, result_rx) = unbounded_channel();
    let BackendConnection {
        api_tx,
        backend_id,
        pool,
        sender,
    } = init::create_render_backend(&mut options, notifier, result_tx, config, resources)
        .map_err(|error| format!("Starting render backend: {error:?}"))?;
    Ok((
        Renderer {
            gpu,
            ready_rx,
            clear_color: options.clear_color,
            last_output: None,
            result_rx,
            api_tx: Some(api_tx),
            backend_id,
            pending_updates: Vec::new(),
            document: None,
            notifications: Vec::new(),
            parameters: Vec::new(),
            pipeline_info: Vec::new(),
            force_redraw: true,
            _pool: pool,
        },
        sender,
    ))
}

impl Renderer {
    pub fn info(&self) -> &wgpu_types::AdapterInfo {
        self.gpu.info()
    }

    pub fn prepare_frame(&mut self, document_id: DocumentId) -> Result<PreparedFrameInfo, String> {
        let ready_document = self
            .ready_rx
            .recv_timeout(Duration::from_secs(60))
            .map_err(|error| format!("Waiting for WR frame: {error:?}"))?;
        if ready_document != document_id {
            return Err("Unexpected HAL frame notification".into());
        }
        while let Ok(message) = self.result_rx.try_recv() {
            match message {
                ResultMsg::PublishDocument(_, id, document, updates) => {
                    if id != document_id {
                        return Err("Unexpected document on HAL renderer".into());
                    }
                    self.pending_updates.push(updates);
                    if self
                        .document
                        .as_ref()
                        .is_some_and(|previous| !previous.frame.has_been_rendered)
                    {
                        return Err("HAL requires consuming frames in publication order".into());
                    }
                    self.document = Some(document);
                }
                ResultMsg::UpdateResources {
                    resource_updates,
                    discard_active_documents,
                    ..
                } => {
                    self.pending_updates.push(resource_updates);
                    if discard_active_documents {
                        self.document = None;
                    }
                }
                ResultMsg::PublishPipelineInfo(info) => self.pipeline_info.push(info),
                ResultMsg::AppendNotificationRequests(requests) => {
                    self.notifications.extend(requests)
                }
                ResultMsg::SetParameter(parameter) => self.parameters.push(parameter),
                ResultMsg::ForceRedraw => self.force_redraw = true,
                ResultMsg::DebugCommand(crate::render_api::DebugCommand::ClearCaches(_)) => {}
                ResultMsg::DebugCommand(crate::render_api::DebugCommand::SetFlags(flags)) => {
                    let allowed = api::DebugFlags::ECHO_DRIVER_MESSAGES
                        | api::DebugFlags::MISSING_SNAPSHOT_PINK
                        | api::DebugFlags::DISABLE_COMPOSITOR_CLIPS
                        | api::DebugFlags::DISABLE_BATCHING;
                    if !(flags - allowed).is_empty() {
                        return Err(format!("Unsupported HAL debug flags {flags:?}"));
                    }
                }
                ResultMsg::DebugCommand(_)
                | ResultMsg::DebugOutput(_)
                | ResultMsg::RefreshShader(_)
                | ResultMsg::RenderDocumentOffscreen(..) => {
                    return Err("Unsupported HAL renderer message".into())
                }
            }
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
            passes: frame.passes.len(),
            picture_tiles: frame.composite_state.tiles.len(),
            primitive_instances,
        })
    }

    pub fn render_frame(&mut self) -> Result<FrameOutput, String> {
        if !self.parameters.is_empty() {
            return Err(format!("Unsupported HAL parameters: {:?}", self.parameters));
        }
        let document = self.document.as_mut().ok_or("No prepared WR frame")?;
        let output = self.gpu.render(
            &document.frame,
            std::mem::take(&mut self.pending_updates),
            self.clear_color,
        )?;
        document.frame.has_been_rendered = true;
        for notification in self.notifications.drain(..) {
            notification.notify();
        }
        self.force_redraw = false;
        Ok(output)
    }

    pub fn render(&mut self) -> Result<super::RenderResults, String> {
        let output = self.render_frame()?;
        println!("HAL rendered WR frame: {:?}", output.stats);
        let document = self.document.as_mut().unwrap();
        let mut results = super::RenderResults::default();
        results.stats.total_draw_calls = output.stats.draw_calls;
        results.stats.color_target_count = output.stats.color_targets.saturating_sub(1);
        results.dirty_rects.push(document.frame.device_rect);
        results.did_rasterize_any_tile = document.frame.composite_state.did_rasterize_any_tile;
        results.picture_cache_debug = std::mem::replace(
            &mut document.frame.composite_state.picture_cache_debug,
            crate::tile_cache::PictureCacheDebugInfo::new(),
        );
        if let Some(stats) = &document.frame_stats {
            results.stats.merge(stats);
        }
        self.last_output = Some(output);
        Ok(results)
    }

    pub fn read_pixels_rgba8(
        &self,
        rect: api::units::FramebufferIntRect,
    ) -> Result<Vec<u8>, String> {
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
        let mut pixels = Vec::with_capacity(rect.width() as usize * rect.height() as usize * 4);
        for y in rect.min.y..rect.max.y {
            let offset =
                ((height as i32 - 1 - y) as usize * width as usize + rect.min.x as usize) * 4;
            pixels.extend_from_slice(&output.pixels[offset..offset + rect.width() as usize * 4]);
        }
        Ok(pixels)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(sender) = self.api_tx.take() {
            let _ = sender.send(ApiMsg::UnregisterWindow(self.backend_id, None));
        }
    }
}

struct FrameNotifier {
    inner: Box<dyn RenderNotifier>,
    ready_tx: Sender<DocumentId>,
}
impl RenderNotifier for FrameNotifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self {
            inner: self.inner.clone(),
            ready_tx: self.ready_tx.clone(),
        })
    }
    fn wake_up(&self, composite_needed: bool) {
        self.inner.wake_up(composite_needed);
    }
    fn shut_down(&self) {
        self.inner.shut_down();
    }
    fn new_frame_ready(
        &self,
        document: DocumentId,
        publish: api::FramePublishId,
        params: &api::FrameReadyParams,
    ) {
        self.inner.new_frame_ready(document, publish, params);
        let _ = self.ready_tx.send(document);
    }
}

#[cfg(test)]
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
        fn shut_down(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn forwards_shutdown() {
        let notified = Arc::new(AtomicBool::new(false));
        let (ready_tx, _) = unbounded_channel();
        let notifier = FrameNotifier {
            inner: Box::new(ShutdownNotice(notified.clone())),
            ready_tx,
        };
        notifier.clone().shut_down();
        assert!(notified.load(Ordering::SeqCst));
    }
}
