/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::*;
use api::*;
use crate::device::hal::diagnostics::{RenderCounter, RenderGauge};
use crate::device::hal::{
    ExternalImageLease, ExternalImageProvider, ExternalImageRelease,
    ExternalImageSource, NativeImage,
};
use crate::render_api::Transaction;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    fn send_color(
        &mut self,
        color: ColorF,
        checkpoints: Option<Arc<Mutex<Vec<Checkpoint>>>>,
    ) {
        let rect = LayoutRect::from_size(LayoutSize::new(
            self.size.width as f32,
            self.size.height as f32,
        ));
        let mut builder = DisplayListBuilder::new(self.pipeline);
        builder.begin(60.0);
        builder.push_rect(
            &CommonItemProperties {
                clip_rect: rect,
                clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(self.pipeline),
                flags: PrimitiveFlags::default(),
            },
            rect,
            color,
        );
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(self.pipeline);
        transaction.set_display_list(
            Epoch(self.epoch),
            self.api.get_namespace_id(),
            builder.end(),
        );
        if let Some(checkpoints) = checkpoints {
            transaction.notify(NotificationRequest::new(
                Checkpoint::FrameRendered,
                Box::new(Checkpoints(checkpoints)),
            ));
        }
        self.epoch += 1;
        transaction.generate_frame(self.epoch as u64, true, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
    }

    fn request(&mut self, checkpoints: Arc<Mutex<Vec<Checkpoint>>>) {
        let mut transaction = Transaction::new();
        transaction.notify(NotificationRequest::new(
            Checkpoint::FrameRendered,
            Box::new(Checkpoints(checkpoints)),
        ));
        self.epoch += 1;
        transaction.generate_frame(self.epoch as u64, true, false, RenderReasons::TESTING);
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

#[derive(Clone, Copy)]
struct Counts {
    executions: u64,
    offscreen: u64,
    full: u64,
    partial: u64,
    acquires: u64,
    presents: u64,
    present_updates: u64,
    hidden: u64,
}

impl Counts {
    fn get(renderer: &Renderer) -> Self {
        let frame = renderer.render_metrics().unwrap().0;
        Self {
            executions: frame.count(RenderCounter::Executions),
            offscreen: frame.count(RenderCounter::OffscreenExecutions),
            full: frame.count(RenderCounter::FullCompositions),
            partial: frame.count(RenderCounter::PartialCompositions),
            acquires: frame.count(RenderCounter::Acquires),
            presents: frame.count(RenderCounter::Presents),
            present_updates: frame.count(RenderCounter::FullPresentUpdates)
                + frame.count(RenderCounter::PartialPresentUpdates)
                + frame.count(RenderCounter::UnchangedPresentUpdates),
            hidden: frame.count(RenderCounter::HiddenSkips),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            executions: self.executions - before.executions,
            offscreen: self.offscreen - before.offscreen,
            full: self.full - before.full,
            partial: self.partial - before.partial,
            acquires: self.acquires - before.acquires,
            presents: self.presents - before.presents,
            present_updates: self.present_updates - before.present_updates,
            hidden: self.hidden - before.hidden,
        }
    }

    fn assert_no_onscreen(self) {
        assert_eq!(self.executions, self.offscreen);
        assert_eq!(self.full, 0);
        assert_eq!(self.partial, 0);
        assert_eq!(self.acquires, 0);
        assert_eq!(self.presents, 0);
        assert_eq!(self.present_updates, 0);
    }
}

fn render_initial(scene: &mut Scene, color: ColorF) {
    scene.send_color(color, None);
    assert!(scene.prepare().render);
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn hidden_updates_acknowledge_and_render_only_latest_when_visible() {
    let mut scene = Scene::new(DeviceIntSize::new(32, 32));
    render_initial(&mut scene, ColorF::new(1.0, 0.0, 0.0, 1.0));
    scene.assert_color([255, 0, 0, 255]);
    let before = Counts::get(&scene.renderer);
    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    for color in [
        ColorF::new(0.0, 1.0, 0.0, 1.0),
        ColorF::new(1.0, 1.0, 0.0, 1.0),
        ColorF::new(0.0, 0.0, 1.0, 1.0),
    ] {
        scene.send_color(color, Some(checkpoints.clone()));
        assert!(scene.prepare().render);
        scene.renderer.service_hidden_frame().unwrap();
        assert!(!scene.renderer.has_current_output());
    }
    let hidden = Counts::get(&scene.renderer).since(before);
    hidden.assert_no_onscreen();
    assert_eq!(hidden.executions, 0);
    assert_eq!(hidden.offscreen, 0);
    assert_eq!(hidden.hidden, 3);
    assert_eq!(
        *checkpoints.lock().unwrap(),
        [
            Checkpoint::FrameRendered,
            Checkpoint::FrameRendered,
            Checkpoint::FrameRendered,
        ],
    );
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    scene.assert_color([0, 0, 255, 255]);
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn hidden_no_op_acknowledges_without_reusing_output() {
    let mut scene = Scene::new(DeviceIntSize::new(32, 32));
    render_initial(&mut scene, ColorF::new(1.0, 0.0, 0.0, 1.0));
    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    scene.request(checkpoints.clone());
    assert!(!scene.prepare().render);
    let before = Counts::get(&scene.renderer);
    scene.renderer.service_hidden_frame().unwrap();
    let hidden = Counts::get(&scene.renderer).since(before);
    hidden.assert_no_onscreen();
    assert_eq!(hidden.executions, 0);
    assert_eq!(hidden.offscreen, 0);
    assert_eq!(hidden.hidden, 1);
    assert_eq!(*checkpoints.lock().unwrap(), [Checkpoint::FrameRendered]);
    assert!(!scene.renderer.has_current_output());
    assert!(matches!(scene.renderer.render_if_needed().unwrap(), RenderOutcome::Rendered(_)));
    scene.assert_color([255, 0, 0, 255]);
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn hidden_raw_image_flushes_mandatory_offscreen_work() {
    let mut scene = Scene::new(DeviceIntSize::new(32, 32));
    let descriptor = ImageDescriptor::new(
        16,
        16,
        ImageFormat::RGBA8,
        ImageDescriptorFlags::IS_OPAQUE,
    );
    let key = scene.api.generate_image_key();
    let rect = LayoutRect::from_size(LayoutSize::new(32.0, 32.0));
    let mut builder = DisplayListBuilder::new(scene.pipeline);
    builder.begin(60.0);
    builder.push_image(
        &CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(scene.pipeline),
            flags: PrimitiveFlags::default(),
        },
        rect,
        ImageRendering::Pixelated,
        AlphaType::PremultipliedAlpha,
        key,
        ColorF::WHITE,
    );
    let mut transaction = Transaction::new();
    transaction.add_image(
        key,
        descriptor,
        ImageData::new([255, 0, 0, 255].repeat(16 * 16)),
        None,
    );
    transaction.set_root_pipeline(scene.pipeline);
    transaction.set_display_list(Epoch(0), scene.api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    scene.api.send_transaction(scene.document, transaction);
    assert!(scene.prepare().render);
    assert!(scene
        .renderer
        .core
        .document
        .as_ref()
        .unwrap()
        .frame
        .must_be_drawn());
    let before = Counts::get(&scene.renderer);
    scene.renderer.service_hidden_frame().unwrap();
    let hidden = Counts::get(&scene.renderer).since(before);
    hidden.assert_no_onscreen();
    assert_eq!(hidden.offscreen, 1);
    assert_eq!(hidden.hidden, 1);
    scene.shutdown();
}

struct ImageProvider {
    image: NativeImage,
    releases: Rc<RefCell<Vec<ExternalImageRelease>>>,
}

impl ExternalImageProvider for ImageProvider {
    fn acquire(&mut self, _: ExternalImageId, _: u8, _: bool) -> Result<ExternalImageLease, String> {
        let releases = self.releases.clone();
        ExternalImageLease::new(
            self.image.descriptor(),
            TexelRect::new(0.0, 0.0, 16.0, 16.0),
            1,
            ExternalImageSource::Native(self.image.clone()),
            move |status| releases.borrow_mut().push(status),
        )
    }
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn hidden_external_work_releases_and_drains() {
    let mut scene = Scene::new(DeviceIntSize::new(32, 32));
    let descriptor = ImageDescriptor::new(
        16,
        16,
        ImageFormat::RGBA8,
        ImageDescriptorFlags::IS_OPAQUE,
    );
    let image = scene.renderer.external_image_device().create_image(
        descriptor,
        &[0, 255, 0, 255].repeat(16 * 16),
    ).unwrap();
    let releases = Rc::new(RefCell::new(Vec::new()));
    scene.renderer.set_external_image_provider(Box::new(ImageProvider {
        image,
        releases: releases.clone(),
    })).unwrap();
    let key = scene.api.generate_image_key();
    let rect = LayoutRect::from_size(LayoutSize::new(32.0, 32.0));
    let mut builder = DisplayListBuilder::new(scene.pipeline);
    builder.begin(60.0);
    builder.push_image(
        &CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(scene.pipeline),
            flags: PrimitiveFlags::default(),
        },
        rect,
        ImageRendering::Pixelated,
        AlphaType::PremultipliedAlpha,
        key,
        ColorF::WHITE,
    );
    let mut transaction = Transaction::new();
    transaction.add_image(key, descriptor, ImageData::External(ExternalImageData {
        id: ExternalImageId(41),
        channel_index: 0,
        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
        normalized_uvs: false,
    }), None);
    transaction.set_root_pipeline(scene.pipeline);
    transaction.set_display_list(Epoch(0), scene.api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    scene.api.send_transaction(scene.document, transaction);
    assert!(scene.prepare().render);
    assert!(scene
        .renderer
        .core
        .document
        .as_ref()
        .unwrap()
        .frame
        .must_be_drawn());
    let before = Counts::get(&scene.renderer);
    scene.renderer.service_hidden_frame().unwrap();
    let hidden = Counts::get(&scene.renderer).since(before);
    hidden.assert_no_onscreen();
    assert_eq!(hidden.hidden, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while releases.borrow().is_empty() || scene.renderer.has_pending_gpu_work() {
        scene.renderer.poll().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(releases.borrow().as_slice(), &[ExternalImageRelease::Complete]);
    let metrics = scene.renderer.render_metrics().unwrap().1;
    assert_eq!(metrics.gauge(RenderGauge::PendingSubmissions), 0);
    assert_eq!(metrics.gauge(RenderGauge::ExternalLeases), 0);
    scene.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn explicit_hidden_render_preserves_old_readback_and_returns_latest_pixels() {
    let mut scene = Scene::new(DeviceIntSize::new(32, 32));
    render_initial(&mut scene, ColorF::new(1.0, 0.0, 0.0, 1.0));
    let readback_rect = FramebufferIntRect::from_size(FramebufferIntSize::new(32, 32));
    let old = scene.renderer.request_readback(readback_rect).unwrap();
    scene.send_color(ColorF::new(0.0, 0.0, 1.0, 1.0), None);
    assert!(scene.prepare().render);
    let before_hidden = Counts::get(&scene.renderer);
    scene.renderer.service_hidden_frame().unwrap();
    let hidden = Counts::get(&scene.renderer).since(before_hidden);
    hidden.assert_no_onscreen();
    assert_eq!(hidden.hidden, 1);

    let before_render = Counts::get(&scene.renderer);
    scene.renderer.render().unwrap();
    let explicit = Counts::get(&scene.renderer).since(before_render);
    assert_eq!(explicit.executions, 1);
    assert_eq!(explicit.acquires, 0);
    assert_eq!(explicit.presents, 0);
    assert_eq!(explicit.present_updates, 0);
    let old_pixels = scene.renderer.wait_readback(old).unwrap();
    assert!(old_pixels.chunks_exact(4).all(|pixel| pixel == [255, 0, 0, 255]));
    scene.assert_color([0, 0, 255, 255]);
    scene.shutdown();
}
