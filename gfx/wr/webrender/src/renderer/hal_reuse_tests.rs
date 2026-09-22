/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::*;
use api::*;
use crate::device::hal::{
    ExternalImageLease, ExternalImageProvider, ExternalImageRelease,
    ExternalImageSource, NativeImage,
};
use crate::render_api::Transaction;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

struct Notice;

impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> { Box::new(Self) }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    fn shut_down(&self) {}
}

struct Checkpoints(Arc<Mutex<Vec<Checkpoint>>>);

impl NotificationHandler for Checkpoints {
    fn notify(&self, checkpoint: Checkpoint) {
        self.0.lock().unwrap().push(checkpoint);
    }
}

struct Scene {
    renderer: Renderer,
    api: crate::render_api::RenderApi,
    document: DocumentId,
    pipeline: PipelineId,
    size: DeviceIntSize,
    epoch: u32,
}

impl Scene {
    fn new(size: DeviceIntSize) -> Self {
        let (renderer, sender) = create_vulkan_renderer(
            &Options { validation: true, ..Default::default() },
            WebRenderOptions::default(),
            Box::new(Notice),
        ).unwrap();
        let api = sender.create_api();
        let document = api.add_document(size);
        Self {
            renderer,
            api,
            document,
            pipeline: PipelineId(0, 0),
            size,
            epoch: 0,
        }
    }

    fn set_color(&mut self, color: ColorF) {
        self.set_color_present(color, true);
    }

    fn set_color_present(&mut self, color: ColorF, present: bool) {
        let rect = LayoutRect::from_size(LayoutSize::new(
            self.size.width as f32,
            self.size.height as f32,
        ));
        let info = CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(self.pipeline),
            flags: PrimitiveFlags::default(),
        };
        let mut builder = DisplayListBuilder::new(self.pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, color);
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(self.pipeline);
        transaction.set_display_list(
            Epoch(self.epoch),
            self.api.get_namespace_id(),
            builder.end(),
        );
        self.epoch += 1;
        transaction.generate_frame(self.epoch as u64, present, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
    }

    fn request(&mut self) {
        self.request_present(true, None);
    }

    fn request_present(
        &mut self,
        present: bool,
        checkpoints: Option<Arc<Mutex<Vec<Checkpoint>>>>,
    ) {
        let mut transaction = Transaction::new();
        if let Some(checkpoints) = checkpoints {
            transaction.notify(NotificationRequest::new(
                Checkpoint::FrameRendered,
                Box::new(Checkpoints(checkpoints)),
            ));
        }
        self.epoch += 1;
        transaction.generate_frame(self.epoch as u64, present, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
    }

    fn prepare(&mut self) -> PreparedFrameInfo {
        self.renderer.prepare_frame(self.document).unwrap()
    }

    fn pixels(&self) -> Vec<u8> {
        self.renderer.read_pixels_rgba8(FramebufferIntRect::from_size(
            FramebufferIntSize::new(self.size.width, self.size.height),
        )).unwrap()
    }

    fn assert_color(&self, color: [u8; 4]) {
        assert!(self.pixels().chunks_exact(4).all(|pixel| pixel == color));
    }

    fn shutdown(self) { self.api.shut_down(true); }
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn prepared_render_survives_later_no_render_until_execution() {
    use crate::device::hal::diagnostics::RenderCounter;
    let mut scene = Scene::new(DeviceIntSize::new(16, 16));
    scene.set_color(ColorF::new(1.0, 0.0, 0.0, 1.0));
    assert!(scene.prepare().render);
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    scene.assert_color([255, 0, 0, 255]);

    scene.set_color(ColorF::new(0.0, 0.0, 1.0, 1.0));
    assert!(scene.prepare().render);
    scene.request();
    assert!(!scene.prepare().render);
    let before = scene.renderer.render_metrics().unwrap().0;
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    let rendered = scene.renderer.render_metrics().unwrap().0;
    assert_eq!(rendered.count(RenderCounter::Executions), before.count(RenderCounter::Executions) + 1);
    scene.assert_color([0, 0, 255, 255]);

    scene.request();
    assert!(!scene.prepare().render);
    let before = scene.renderer.render_metrics().unwrap();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Reused));
    let reused = scene.renderer.render_metrics().unwrap();
    for counter in [
        RenderCounter::Executions,
        RenderCounter::FullCompositions,
        RenderCounter::Acquires,
        RenderCounter::Presents,
    ] {
        assert_eq!(reused.0.count(counter), before.0.count(counter));
    }
    assert_eq!(
        reused.1.count(RenderCounter::QueueSubmissions),
        before.1.count(RenderCounter::QueueSubmissions),
    );
    assert_eq!(
        reused.0.count(RenderCounter::ReusedOutputs),
        before.0.count(RenderCounter::ReusedOutputs) + 1,
    );
    scene.assert_color([0, 0, 255, 255]);
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn reuse_preserves_late_nonvisual_messages_and_notifies_frame_rendered() {
    use crate::device::hal::diagnostics::RenderCounter;
    let mut scene = Scene::new(DeviceIntSize::new(16, 16));
    scene.set_color(ColorF::new(1.0, 0.0, 0.0, 1.0));
    scene.prepare();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));

    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    scene.request_present(true, Some(checkpoints.clone()));
    assert!(!scene.prepare().render);
    scene.renderer.update().unwrap();
    let mut pipeline_info = PipelineInfo::default();
    pipeline_info.epochs.insert((scene.pipeline, scene.document), Epoch(91));
    scene
        .renderer
        .core
        .process_message(ResultMsg::PublishPipelineInfo(pipeline_info))
        .unwrap();
    scene
        .renderer
        .core
        .process_message(ResultMsg::AppendNotificationRequests(vec![
            NotificationRequest::new(
                Checkpoint::FrameTexturesUpdated,
                Box::new(Checkpoints(checkpoints.clone())),
            ),
        ]))
        .unwrap();
    assert_eq!(*checkpoints.lock().unwrap(), [Checkpoint::FrameTexturesUpdated]);
    let before = scene.renderer.render_metrics().unwrap();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Reused));
    let after = scene.renderer.render_metrics().unwrap();
    assert_eq!(after.0.count(RenderCounter::Executions), before.0.count(RenderCounter::Executions));
    assert_eq!(after.1.count(RenderCounter::QueueSubmissions), before.1.count(RenderCounter::QueueSubmissions));
    assert_eq!(
        *checkpoints.lock().unwrap(),
        [Checkpoint::FrameTexturesUpdated, Checkpoint::FrameRendered],
    );
    assert_eq!(
        scene.renderer.flush_pipeline_info().epochs[&(scene.pipeline, scene.document)],
        Epoch(91),
    );
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation"]
fn conditional_render_requires_prepare_and_failure_drops_output() {
    use crate::device::hal::FailurePoint;
    let mut scene = Scene::new(DeviceIntSize::new(16, 16));
    scene.set_color(ColorF::new(1.0, 0.0, 0.0, 1.0));
    scene
        .renderer
        .core
        .ready
        .wait(scene.renderer.core.ready_generation)
        .unwrap();
    let error = match scene.renderer.render_if_needed() {
        Err(error) => error,
        Ok(_) => panic!("conditional rendering accepted an unprepared frame"),
    };
    assert!(error.contains("Prepare"));
    assert!(!scene.renderer.has_current_output());
    scene.prepare();
    scene.renderer.inject_failure(FailurePoint::Record);
    assert!(scene.renderer.render_if_needed().is_err());
    assert!(!scene.renderer.has_current_output());
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn offscreen_no_op_skips_but_required_offscreen_frame_renders_once() {
    use crate::device::hal::diagnostics::RenderCounter;
    let mut scene = Scene::new(DeviceIntSize::new(16, 16));
    scene.set_color(ColorF::new(1.0, 0.0, 0.0, 1.0));
    scene.prepare();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));

    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    scene.renderer.force_redraw();
    scene.request_present(false, Some(checkpoints.clone()));
    let prepared = scene.prepare();
    assert!(!prepared.render && !prepared.present);
    let before = scene.renderer.render_metrics().unwrap();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Skipped));
    let skipped = scene.renderer.render_metrics().unwrap();
    for counter in [
        RenderCounter::Executions,
        RenderCounter::FullCompositions,
        RenderCounter::Acquires,
        RenderCounter::Presents,
    ] {
        assert_eq!(skipped.0.count(counter), before.0.count(counter));
    }
    assert_eq!(
        skipped.1.count(RenderCounter::QueueSubmissions),
        before.1.count(RenderCounter::QueueSubmissions),
    );
    assert_eq!(*checkpoints.lock().unwrap(), [Checkpoint::FrameRendered]);

    scene.request_present(true, None);
    assert!(!scene.prepare().render);
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));

    scene.set_color_present(ColorF::new(0.0, 0.0, 1.0, 1.0), false);
    let prepared = scene.prepare();
    assert!(prepared.render && !prepared.present);
    let before = scene.renderer.render_metrics().unwrap();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    let rendered = scene.renderer.render_metrics().unwrap();
    assert_eq!(rendered.0.count(RenderCounter::Executions), before.0.count(RenderCounter::Executions) + 1);
    let before = scene.renderer.render_metrics().unwrap();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Skipped));
    let skipped = scene.renderer.render_metrics().unwrap();
    assert_eq!(skipped.0.count(RenderCounter::Executions), before.0.count(RenderCounter::Executions));
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation"]
fn output_identity_and_force_guards_prevent_stale_reuse() {
    let mut first = Scene::new(DeviceIntSize::new(16, 16));
    assert!(!first.renderer.has_current_output());
    first.set_color(ColorF::new(1.0, 0.0, 0.0, 1.0));
    first.prepare();
    assert!(matches!(first.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    assert!(first.renderer.has_current_output());

    first.renderer.force_redraw();
    first.request();
    assert!(!first.prepare().render);
    assert!(matches!(first.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    first.renderer.set_clear_color(ColorF::new(0.0, 1.0, 0.0, 1.0));
    first.request();
    first.prepare();
    assert!(matches!(first.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));

    let second = first.api.add_document(DeviceIntSize::new(8, 8));
    let original = first.document;
    first.document = second;
    first.size = DeviceIntSize::new(8, 8);
    first.pipeline = PipelineId(0, 1);
    first.set_color(ColorF::new(0.0, 1.0, 0.0, 1.0));
    first.prepare();
    assert!(matches!(first.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    first.assert_color([0, 255, 0, 255]);

    first.document = original;
    first.size = DeviceIntSize::new(16, 16);
    first.pipeline = PipelineId(0, 0);
    first.request();
    first.prepare();
    assert!(matches!(first.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    first.assert_color([255, 0, 0, 255]);
    first.shutdown();
}

struct ImageProvider {
    image: Rc<RefCell<NativeImage>>,
    generation: Rc<Cell<u64>>,
    releases: Rc<RefCell<Vec<ExternalImageRelease>>>,
}

impl ExternalImageProvider for ImageProvider {
    fn acquire(&mut self, _: ExternalImageId, _: u8, _: bool) -> Result<ExternalImageLease, String> {
        let image = self.image.borrow().clone();
        let releases = self.releases.clone();
        ExternalImageLease::new(
            image.descriptor(),
            TexelRect::new(0.0, 0.0, 8.0, 8.0),
            self.generation.get(),
            ExternalImageSource::Native(image),
            move |status| releases.borrow_mut().push(status),
        )
    }
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn same_id_external_invalidation_redraws_and_releases_before_reuse() {
    use crate::device::hal::diagnostics::{RenderCounter, RenderGauge};
    let mut scene = Scene::new(DeviceIntSize::new(8, 8));
    let device = scene.renderer.external_image_device();
    let descriptor = ImageDescriptor::new(
        8, 8, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE,
    );
    let image = Rc::new(RefCell::new(device.create_image(
        descriptor, &[255, 0, 0, 255].repeat(64),
    ).unwrap()));
    assert!(scene.renderer.has_pending_gpu_work());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while scene.renderer.has_pending_gpu_work() {
        scene.renderer.poll().unwrap();
        assert!(std::time::Instant::now() < deadline);
    }
    let generation = Rc::new(Cell::new(1));
    let releases = Rc::new(RefCell::new(Vec::new()));
    scene.renderer.set_external_image_provider(Box::new(ImageProvider {
        image: image.clone(),
        generation: generation.clone(),
        releases: releases.clone(),
    })).unwrap();
    let key = scene.api.generate_image_key();
    let mut transaction = Transaction::new();
    transaction.add_image(key, descriptor, ImageData::External(ExternalImageData {
        id: ExternalImageId(77),
        channel_index: 0,
        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
        normalized_uvs: false,
    }), None);
    let rect = LayoutRect::from_size(LayoutSize::new(8.0, 8.0));
    let info = CommonItemProperties {
        clip_rect: rect,
        clip_chain_id: ClipChainId::INVALID,
        spatial_id: SpatialId::root_scroll_node(scene.pipeline),
        flags: PrimitiveFlags::PREFER_COMPOSITOR_SURFACE,
    };
    let mut builder = DisplayListBuilder::new(scene.pipeline);
    builder.begin(60.0);
    builder.push_image(
        &info, rect, ImageRendering::Pixelated, AlphaType::PremultipliedAlpha,
        key, ColorF::WHITE,
    );
    transaction.set_root_pipeline(scene.pipeline);
    transaction.set_display_list(Epoch(0), scene.api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    scene.api.send_transaction(scene.document, transaction);
    scene.prepare();
    assert!(!scene
        .renderer
        .core
        .document
        .as_ref()
        .unwrap()
        .frame
        .composite_state
        .external_surfaces
        .is_empty());
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    scene.assert_color([255, 0, 0, 255]);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while releases.borrow().is_empty() || scene.renderer.has_pending_gpu_work() {
        scene.renderer.poll().unwrap();
        assert!(std::time::Instant::now() < deadline);
    }

    device.update_image(
        &image.borrow(), descriptor, &[0, 0, 255, 255].repeat(64),
    ).unwrap();
    generation.set(2);
    let mut transaction = Transaction::new();
    transaction.invalidate_rendered_frame(RenderReasons::TESTING);
    transaction.generate_frame(2, true, false, RenderReasons::TESTING);
    scene.api.send_transaction(scene.document, transaction);
    assert!(scene.prepare().render);
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    scene.assert_color([0, 0, 255, 255]);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while releases.borrow().len() < 2 || scene.renderer.has_pending_gpu_work() {
        scene.renderer.poll().unwrap();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    scene.request();
    scene.prepare();
    let before = scene.renderer.render_metrics().unwrap();
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Reused));
    let after = scene.renderer.render_metrics().unwrap();
    assert_eq!(
        after.0.count(RenderCounter::Executions),
        before.0.count(RenderCounter::Executions),
    );
    assert_eq!(after.1.gauge(RenderGauge::ExternalLeases), 0);
    assert_eq!(
        after.1.count(RenderCounter::ExternalLeaseAcquires),
        after.1.count(RenderCounter::ExternalLeaseReleases),
    );
    scene.shutdown();
}
