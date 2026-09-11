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
use crate::device::hal::{create_vulkan_device, Options, FrameOutput};
use crate::device::hal::render::FrameRenderer;
use crate::frame_builder::FrameBuilderConfig;
use crate::internal_types::{RenderedDocument, ResourceUpdateList, ResultMsg};
use crate::render_api::{ApiMsg, RenderApiSender};
use crate::render_backend_pool::RenderBackendPool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) const MAX_DEPTH_IDS: i32 = 1 << 22;

#[derive(Debug)]
pub struct PreparedFrameInfo {
    pub passes: usize,
    pub picture_tiles: usize,
    pub primitive_instances: usize,
}

pub struct Renderer {
    gpu: FrameRenderer<wgpu_hal::api::Vulkan>,
    ready: Arc<FrameReady>,
    ready_generation: u64,
    pending_message: Option<ResultMsg>,
    document_id: Option<DocumentId>,
    clear_color: api::ColorF,
    last_output: Option<FrameOutput>,
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
    let mut gpu = FrameRenderer::new(device)?;
    if options.enable_dithering {
        gpu.enable_dithering()?;
    }
    let ready = Arc::new(FrameReady::default());
    let notifier = Box::new(FrameNotifier {
        inner: notifier,
        ready: ready.clone(),
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
            ready,
            ready_generation: 0,
            pending_message: None,
            document_id: None,
            clear_color: options.clear_color,
            last_output: None,
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

impl Renderer {
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
        self.update_until(None)
    }

    fn check_document(&mut self, id: DocumentId) -> Result<(), String> {
        match self.document_id {
            Some(previous) if previous != id => {
                Err("Multiple documents require HAL embedding integration".into())
            }
            _ => {
                self.document_id = Some(id);
                Ok(())
            }
        }
    }

    fn flush_required_frame(&mut self) -> Result<(), String> {
        if let Some(document) = &mut self.document {
            if document.frame.must_be_drawn() {
                self.gpu.render_offscreen(&document.frame)?;
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
        self.gpu.update_resources(vec![updates])?;
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
        match message {
            ResultMsg::PublishDocument(_, id, mut document, updates) => {
                self.check_document(id)?;
                self.flush_required_frame()?;
                if let Some(mut previous) = self.document.take() {
                    document.profile.merge(&mut previous.profile);
                }
                self.apply_resources(updates)?;
                self.document = Some(document);
            }
            ResultMsg::RenderDocumentOffscreen(id, document, updates) => {
                self.check_document(id)?;
                self.flush_required_frame()?;
                self.apply_resources(updates)?;
                self.gpu.render_offscreen(&document.frame)?;
            }
            ResultMsg::UpdateResources {
                resource_updates,
                memory_pressure,
                discard_active_documents,
                ..
            } => {
                if memory_pressure || discard_active_documents {
                    self.flush_required_frame()?;
                    self.document = None;
                    self.last_output = None;
                }
                self.apply_resources(resource_updates)?;
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
            ResultMsg::SetParameter(parameter) => {
                return Err(format!(
                    "HAL has no consumer for renderer parameter {parameter:?}"
                ));
            }
            ResultMsg::ForceRedraw => self.force_redraw = true,
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
                            | api::DebugFlags::DISABLE_COMPOSITOR_CLIPS
                            | api::DebugFlags::DISABLE_BATCHING;
                        if !(flags - allowed).is_empty() {
                            return Err(format!("Unsupported HAL debug flags {flags:?}"));
                        }
                        self.debug_flags = flags;
                    }
                    _ => return Err("Unsupported HAL renderer debug command".into()),
                }
            }
            ResultMsg::DebugOutput(_) | ResultMsg::RefreshShader(_) => {
                return Err("HAL capture and shader reload require embedding integration".into());
            }
        }
        Ok(())
    }

    pub fn prepare_frame(&mut self, document_id: DocumentId) -> Result<PreparedFrameInfo, String> {
        let (generation, ready_document, publish, present) =
            self.ready.wait(self.ready_generation)?;
        if ready_document != document_id {
            return Err("Unexpected HAL frame notification".into());
        }
        self.ready_generation = generation;
        self.update_until(Some(publish))?;
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
            passes: frame.passes.len(),
            picture_tiles: frame.composite_state.tiles.len(),
            primitive_instances,
        })
    }

    pub fn render_frame(&mut self) -> Result<FrameOutput, String> {
        let document = self.document.as_mut().ok_or("No prepared WR frame")?;
        let output = self
            .gpu
            .render(&document.frame, Vec::new(), self.clear_color)?;
        document.frame.has_been_rendered = true;
        self.notify(Checkpoint::FrameRendered);
        self.force_redraw = false;
        Ok(output)
    }

    pub fn render(&mut self) -> Result<super::RenderResults, String> {
        let output = self.render_frame()?;
        println!("HAL rendered WR frame: {:?}", output.stats);
        let document = self.document.as_mut().unwrap();
        let mut results = super::RenderResults::default();
        results.stats.total_draw_calls = output.stats.wr_draw_calls;
        results.stats.color_target_count = output.stats.color_targets;
        results.stats.alpha_target_count = output.stats.alpha_targets;
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

#[derive(Default)]
struct ReadyState {
    generation: u64,
    frame: Option<(DocumentId, api::FramePublishId, bool)>,
    shutdown: bool,
}

#[derive(Default)]
struct FrameReady {
    state: Mutex<ReadyState>,
    changed: Condvar,
}

impl FrameReady {
    fn publish(&self, document: DocumentId, publish: api::FramePublishId, present: bool) {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        state.frame = Some((document, publish, present));
        self.changed.notify_all();
    }

    fn wait(
        &self,
        generation: u64,
    ) -> Result<(u64, DocumentId, api::FramePublishId, bool), String> {
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
        let (document, publish, present) = state.frame.ok_or("No WR frame notification")?;
        Ok((state.generation, document, publish, present))
    }
}

struct FrameNotifier {
    inner: Box<dyn RenderNotifier>,
    ready: Arc<FrameReady>,
}
impl RenderNotifier for FrameNotifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self {
            inner: self.inner.clone(),
            ready: self.ready.clone(),
        })
    }
    fn wake_up(&self, composite_needed: bool) {
        self.inner.wake_up(composite_needed);
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
        self.ready.publish(document, publish, params.present);
        self.inner.new_frame_ready(document, publish, params);
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
        let ready = Arc::new(FrameReady::default());
        let notifier = FrameNotifier {
            inner: Box::new(ShutdownNotice(notified.clone())),
            ready,
        };
        notifier.clone().shut_down();
        assert!(notified.load(Ordering::SeqCst));
    }
    #[test]
    fn coalesces_ready_notifications_and_wakes_on_shutdown() {
        let ready = FrameReady::default();
        let id = DocumentId::new(api::IdNamespace(7), 1);
        for serial in 1..=1000 {
            ready.publish(id, api::FramePublishId(serial), true);
        }
        let (generation, document, publish, present) = ready.wait(0).unwrap();
        assert!(present);
        assert_eq!((generation, document, publish.0), (1000, id, 1000));
        ready.state.lock().unwrap().shutdown = true;
        assert!(ready.wait(generation).unwrap_err().contains("shut down"));
    }

    struct Checkpoints(Arc<Mutex<Vec<Checkpoint>>>);
    impl api::NotificationHandler for Checkpoints {
        fn notify(&self, checkpoint: Checkpoint) {
            self.0.lock().unwrap().push(checkpoint);
        }
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
            generation = renderer.ready.wait(generation).unwrap().0;
        }
        renderer.prepare_frame(id).unwrap();
        assert_eq!(renderer.ready_generation, generation);
        let output = renderer.render_frame().unwrap();
        assert_eq!(&output.pixels[..4], &[255, 0, 0, 255]);
        let original_rect = renderer.document.as_ref().unwrap().frame.device_rect;
        renderer.document.as_mut().unwrap().frame.device_rect = DeviceIntRect::from_origin_and_size(
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
        renderer.document.as_mut().unwrap().frame.device_rect = original_rect;
        let pipeline_info = renderer.flush_pipeline_info();
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
        assert!(renderer.notifications.is_empty());

        send(&mut api, 4, false);
        renderer.prepare_frame(id).unwrap();
        assert!(renderer.render_frame().unwrap().pixels.is_empty());
        send(&mut api, 5, true);
        renderer.prepare_frame(id).unwrap();
        let mut document = renderer.document.take().unwrap();
        document.frame.has_texture_cache_tasks = true;
        document.frame.has_been_rendered = false;
        renderer.document = Some(document);
        renderer
            .process_message(ResultMsg::UpdateResources {
                resource_updates: ResourceUpdateList {
                    native_surface_updates: Vec::new(),
                    texture_updates: crate::internal_types::TextureUpdateList::new(),
                },
                memory_pressure: false,
                discard_active_documents: true,
                trim_upload_buffers: true,
            })
            .unwrap();
        assert!(renderer.document.is_none());
        assert!(renderer.last_output.is_none());
        renderer.update().unwrap();
        send(&mut api, 6, true);
        renderer.prepare_frame(id).unwrap();
        let mut offscreen = renderer.document.take().unwrap();
        offscreen.frame.device_rect = DeviceIntRect::zero();
        renderer
            .process_message(ResultMsg::RenderDocumentOffscreen(
                id,
                offscreen,
                ResourceUpdateList {
                    native_surface_updates: Vec::new(),
                    texture_updates: crate::internal_types::TextureUpdateList::new(),
                },
            ))
            .unwrap();
        assert!(renderer.document.is_none());
        assert!(renderer.last_output.is_none());
        for epoch in 0..1000 {
            let mut info = PipelineInfo::default();
            info.epochs.insert((pipeline, id), Epoch(epoch));
            renderer
                .process_message(ResultMsg::PublishPipelineInfo(info))
                .unwrap();
        }
        assert_eq!(renderer.flush_pipeline_info().epochs.len(), 1);
        assert!(renderer.flush_pipeline_info().epochs.is_empty());
        send(&mut api, 7, true);
        renderer.ready.wait(renderer.ready_generation).unwrap();
        api.shut_down(true);
        renderer.update().unwrap();
        renderer.ready_generation = renderer.ready.state.lock().unwrap().generation;
        assert!(renderer.ready.wait(renderer.ready_generation).is_err());
    }
}
