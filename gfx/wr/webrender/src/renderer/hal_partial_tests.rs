/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::*;
use api::*;
use crate::device::hal::diagnostics::RenderCounter;
use crate::device::hal::{
    ExternalImageLease, ExternalImageProvider, ExternalImageRelease,
    ExternalImageSource, NativeImage,
};
use crate::render_api::Transaction;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Notice;

impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> { Box::new(Self) }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    fn shut_down(&self) {}
}

#[derive(Clone, Copy)]
struct Item {
    rect: LayoutRect,
    clip: LayoutRect,
    color: ColorF,
    flags: PrimitiveFlags,
}

impl Item {
    fn new(rect: LayoutRect, color: ColorF) -> Self {
        Self {
            rect,
            clip: rect,
            color,
            flags: PrimitiveFlags::default(),
        }
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

    fn send(&mut self, items: &[Item]) {
        let mut builder = DisplayListBuilder::new(self.pipeline);
        builder.begin(60.0);
        for item in items {
            builder.push_rect(
                &CommonItemProperties {
                    clip_rect: item.clip,
                    clip_chain_id: ClipChainId::INVALID,
                    spatial_id: SpatialId::root_scroll_node(self.pipeline),
                    flags: item.flags,
                },
                item.rect,
                item.color,
            );
        }
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(self.pipeline);
        transaction.set_display_list(
            Epoch(self.epoch),
            self.api.get_namespace_id(),
            builder.end(),
        );
        self.epoch += 1;
        transaction.generate_frame(self.epoch as u64, true, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
    }

    fn resize_and_send(&mut self, size: DeviceIntSize, items: &[Item]) {
        self.view_and_send(DeviceIntRect::from_size(size), items);
    }

    fn view_and_send(&mut self, view: DeviceIntRect, items: &[Item]) {
        self.size = view.size();
        let mut builder = DisplayListBuilder::new(self.pipeline);
        builder.begin(60.0);
        for item in items {
            builder.push_rect(
                &CommonItemProperties {
                    clip_rect: item.clip,
                    clip_chain_id: ClipChainId::INVALID,
                    spatial_id: SpatialId::root_scroll_node(self.pipeline),
                    flags: item.flags,
                },
                item.rect,
                item.color,
            );
        }
        let mut transaction = Transaction::new();
        transaction.set_document_view(view);
        transaction.set_root_pipeline(self.pipeline);
        transaction.set_display_list(
            Epoch(self.epoch),
            self.api.get_namespace_id(),
            builder.end(),
        );
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

    fn shutdown(self) { self.api.shut_down(true); }
}

#[derive(Clone, Copy)]
struct Counts {
    executions: u64,
    full: u64,
    partial: u64,
    pixels: u64,
}

impl Counts {
    fn get(renderer: &Renderer) -> Self {
        let (frame, _) = renderer.render_metrics().unwrap();
        Self {
            executions: frame.count(RenderCounter::Executions),
            full: frame.count(RenderCounter::FullCompositions),
            partial: frame.count(RenderCounter::PartialCompositions),
            pixels: frame.count(RenderCounter::ComposedPixels),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            executions: self.executions - before.executions,
            full: self.full - before.full,
            partial: self.partial - before.partial,
            pixels: self.pixels - before.pixels,
        }
    }
}

struct Rendered {
    delta: Counts,
}

fn render(scene: &mut Scene, force: bool) -> Rendered {
    assert!(scene.prepare().render);
    let before = Counts::get(&scene.renderer);
    if force {
        scene.renderer.force_redraw();
    }
    match scene.renderer.render_if_needed().unwrap() {
        RenderOutcome::Rendered(_) => {},
        RenderOutcome::Reused | RenderOutcome::Skipped => panic!("required frame was not rendered"),
    }
    Rendered {
        delta: Counts::get(&scene.renderer).since(before),
    }
}

fn assert_full(delta: Counts, size: DeviceIntSize) {
    assert_eq!(delta.executions, 1);
    assert_eq!(delta.full, 1);
    assert_eq!(delta.partial, 0);
    assert_eq!(delta.pixels, size.width as u64 * size.height as u64);
}

fn assert_partial(delta: Counts, size: DeviceIntSize) {
    let area = size.width as u64 * size.height as u64;
    assert_eq!(delta.executions, 1);
    assert_eq!(delta.full, 0);
    assert_eq!(delta.partial, 1);
    assert!(delta.pixels > 0 && delta.pixels < area);
}

fn full_rect(size: DeviceIntSize) -> LayoutRect {
    LayoutRect::from_size(LayoutSize::new(size.width as f32, size.height as f32))
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> LayoutRect {
    LayoutRect::from_origin_and_size(
        LayoutPoint::new(x, y),
        LayoutSize::new(width, height),
    )
}

fn base_items(size: DeviceIntSize, overlay: Item) -> Vec<Item> {
    vec![
        Item::new(full_rect(size), ColorF::new(0.125, 0.25, 0.375, 1.0)),
        overlay,
    ]
}

fn initialized_pair(size: DeviceIntSize, items: &[Item]) -> (Scene, Scene) {
    let mut candidate = Scene::new(size);
    let mut control = Scene::new(size);
    candidate.send(items);
    control.send(items);
    assert_full(render(&mut candidate, false).delta, size);
    assert_full(render(&mut control, true).delta, size);
    assert_eq!(candidate.pixels(), control.pixels());
    (candidate, control)
}

fn initialized_pair_with_view(view: DeviceIntRect, items: &[Item]) -> (Scene, Scene) {
    let mut candidate = Scene::new(view.size());
    let mut control = Scene::new(view.size());
    candidate.view_and_send(view, items);
    control.view_and_send(view, items);
    assert_full(render(&mut candidate, false).delta, view.size());
    assert_full(render(&mut control, true).delta, view.size());
    assert_eq!(candidate.pixels(), control.pixels());
    (candidate, control)
}

fn assert_same_pixels(candidate: &Scene, control: &Scene) {
    assert_eq!(candidate.pixels(), control.pixels());
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn nonzero_origin_unions_separated_wr_damage() {
    let size = DeviceIntSize::new(256, 256);
    let view = DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(13, 17), size);
    let first = Item::new(rect(48.0, 64.0, 16.0, 16.0), ColorF::new(1.0, 0.0, 0.0, 1.0));
    let second = Item::new(rect(176.0, 144.0, 16.0, 16.0), ColorF::new(0.0, 1.0, 0.0, 1.0));
    let mut initial = base_items(size, first);
    initial.push(second);
    let (mut candidate, mut control) = initialized_pair_with_view(view, &initial);
    let previous_descriptor = candidate.renderer.core.last_descriptor.clone().unwrap();
    let mut items = base_items(
        size,
        Item::new(first.rect, ColorF::new(0.0, 0.0, 1.0, 1.0)),
    );
    items.push(Item::new(second.rect, ColorF::new(1.0, 1.0, 0.0, 1.0)));
    candidate.send(&items);
    control.send(&items);
    let prepared = candidate.prepare();
    assert!(prepared.render);
    let state = &candidate.renderer.core.document.as_ref().unwrap().frame.composite_state;
    assert!(state.dirty_rects_are_valid);
    assert!(state.external_surfaces.is_empty());
    assert!(state.descriptor == previous_descriptor);
    let before = Counts::get(&candidate.renderer);
    let results = match candidate.renderer.render_if_needed().unwrap() {
        RenderOutcome::Rendered(results) => results,
        RenderOutcome::Reused | RenderOutcome::Skipped => panic!("localized update was not rendered"),
    };
    let delta = Counts::get(&candidate.renderer).since(before);
    assert_partial(delta, size);
    assert_eq!(results.dirty_rects.len(), 1);
    let damage = results.dirty_rects[0];
    for point in [DeviceIntPoint::new(62, 82), DeviceIntPoint::new(190, 162)] {
        assert!(damage.min.x <= point.x && damage.max.x > point.x);
        assert!(damage.min.y <= point.y && damage.max.y > point.y);
    }
    let dirty_pixels = damage.width() as i64 * damage.height() as i64;
    assert!(dirty_pixels > 0 && dirty_pixels < size.width as i64 * size.height as i64);
    assert_full(render(&mut control, true).delta, size);
    assert_same_pixels(&candidate, &control);
    candidate.shutdown();
    control.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn overlapping_move_remove_and_clip_match_full_render() {
    let size = DeviceIntSize::new(256, 256);
    let start = Item::new(rect(40.0, 48.0, 48.0, 40.0), ColorF::new(1.0, 0.0, 0.0, 0.5));
    let mut overlap = Item::new(start.rect, ColorF::new(0.0, 1.0, 0.0, 0.5));
    overlap.rect = rect(56.0, 56.0, 48.0, 40.0);
    let moved = Item::new(rect(136.0, 88.0, 48.0, 40.0), ColorF::new(0.0, 1.0, 0.0, 0.5));
    let mut clipped = moved;
    clipped.clip = rect(144.0, 96.0, 16.0, 16.0);
    let mut changed_overlap = overlap;
    changed_overlap.color = ColorF::new(0.0, 0.0, 1.0, 0.5);
    let cases = [
        (vec![start, overlap], vec![start, changed_overlap]),
        (vec![start], vec![moved]),
        (vec![start], vec![]),
        (vec![moved], vec![clipped]),
    ];
    for (initial_overlays, overlays) in cases {
        let mut initial = vec![Item::new(
            full_rect(size),
            ColorF::new(0.125, 0.25, 0.375, 1.0),
        )];
        initial.extend(initial_overlays);
        let (mut candidate, mut control) = initialized_pair(size, &initial);
        let mut items = vec![Item::new(
            full_rect(size),
            ColorF::new(0.125, 0.25, 0.375, 1.0),
        )];
        items.extend(overlays);
        candidate.send(&items);
        control.send(&items);
        let partial = render(&mut candidate, false);
        assert_eq!(partial.delta.executions, 1);
        assert_eq!(partial.delta.full + partial.delta.partial, 1);
        if partial.delta.partial == 1 {
            assert_partial(partial.delta, size);
        } else {
            assert_full(partial.delta, size);
        }
        assert_full(render(&mut control, true).delta, size);
        assert_same_pixels(&candidate, &control);
        candidate.shutdown();
        control.shutdown();
    }
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn pending_readback_keeps_the_previous_output() {
    let size = DeviceIntSize::new(2048, 1024);
    let first = Item::new(
        rect(48.0, 64.0, 16.0, 16.0),
        ColorF::new(1.0, 0.0, 0.0, 1.0),
    );
    let second = Item::new(
        rect(176.0, 144.0, 16.0, 16.0),
        ColorF::new(0.0, 1.0, 0.0, 1.0),
    );
    let mut old = base_items(size, first);
    old.push(second);
    let mut new = base_items(
        size,
        Item::new(first.rect, ColorF::new(0.0, 0.0, 1.0, 1.0)),
    );
    new.push(Item::new(second.rect, ColorF::new(1.0, 1.0, 0.0, 1.0)));
    let (mut candidate, mut control) = initialized_pair(size, &old);
    let old_pixels = candidate.pixels();
    let pending = candidate.renderer.request_readback(FramebufferIntRect::from_size(
        FramebufferIntSize::new(size.width, size.height),
    )).unwrap();
    candidate.send(&new);
    control.send(&new);
    let partial = render(&mut candidate, false);
    assert_partial(partial.delta, size);
    assert!(partial.delta.pixels <= size.width as u64 * size.height as u64 / 2);
    assert_full(render(&mut control, true).delta, size);
    assert_eq!(candidate.renderer.wait_readback(pending).unwrap(), old_pixels);
    assert_same_pixels(&candidate, &control);
    candidate.shutdown();
    control.shutdown();
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn force_and_clear_color_require_full_composition() {
    let size = DeviceIntSize::new(256, 256);
    let first = base_items(
        size,
        Item::new(rect(16.0, 16.0, 16.0, 16.0), ColorF::new(1.0, 0.0, 0.0, 1.0)),
    );
    let second = base_items(
        size,
        Item::new(rect(16.0, 16.0, 16.0, 16.0), ColorF::new(0.0, 1.0, 0.0, 1.0)),
    );
    {
        let (mut candidate, mut control) = initialized_pair(size, &first);
        candidate.send(&second);
        control.send(&second);
        candidate.renderer.force_redraw();
        assert_full(render(&mut candidate, false).delta, size);
        assert_full(render(&mut control, true).delta, size);
        assert_same_pixels(&candidate, &control);
        candidate.shutdown();
        control.shutdown();
    }
    {
        let initial = vec![first[1]];
        let update = vec![second[1]];
        let (mut candidate, mut control) = initialized_pair(size, &initial);
        candidate.send(&update);
        control.send(&update);
        let color = ColorF::new(0.25, 0.0, 0.25, 1.0);
        candidate.renderer.set_clear_color(color);
        control.renderer.set_clear_color(color);
        assert_full(render(&mut candidate, false).delta, size);
        assert_full(render(&mut control, true).delta, size);
        assert_same_pixels(&candidate, &control);
        let pixels = candidate.pixels();
        assert!((pixels[0] as i16 - 64).abs() <= 1);
        assert_eq!(&pixels[1..4], &[0, 64, 255]);
        candidate.shutdown();
        control.shutdown();
    }
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn document_view_resize_requires_full_composition() {
    let initial_size = DeviceIntSize::new(256, 256);
    let new_size = DeviceIntSize::new(192, 128);
    let initial = vec![Item::new(
        full_rect(initial_size),
        ColorF::new(0.125, 0.25, 0.375, 1.0),
    )];
    let resized = vec![Item::new(
        full_rect(new_size),
        ColorF::new(0.125, 0.25, 0.375, 1.0),
    )];
    let (mut candidate, mut control) = initialized_pair(initial_size, &initial);
    candidate.resize_and_send(new_size, &resized);
    control.resize_and_send(new_size, &resized);
    assert_full(render(&mut candidate, false).delta, new_size);
    assert_full(render(&mut control, true).delta, new_size);
    assert_same_pixels(&candidate, &control);
    candidate.shutdown();
    control.shutdown();
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
            TexelRect::new(0.0, 0.0, 32.0, 32.0),
            self.generation.get(),
            ExternalImageSource::Native(image),
            move |status| releases.borrow_mut().push(status),
        )
    }
}

struct ExternalScene {
    scene: Scene,
    image: Rc<RefCell<NativeImage>>,
    generation: Rc<Cell<u64>>,
    releases: Rc<RefCell<Vec<ExternalImageRelease>>>,
    descriptor: ImageDescriptor,
}

impl ExternalScene {
    fn new(color: [u8; 4]) -> Self {
        let mut scene = Scene::new(DeviceIntSize::new(256, 256));
        let device = scene.renderer.external_image_device();
        let descriptor = ImageDescriptor::new(
            32,
            32,
            ImageFormat::RGBA8,
            ImageDescriptorFlags::IS_OPAQUE,
        );
        let image = Rc::new(RefCell::new(
            device.create_image(descriptor, &color.repeat(32 * 32)).unwrap(),
        ));
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
            id: ExternalImageId(91),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        }), None);
        let full = full_rect(scene.size);
        let image_rect = rect(48.0, 48.0, 32.0, 32.0);
        let mut builder = DisplayListBuilder::new(scene.pipeline);
        builder.begin(60.0);
        builder.push_rect(
            &CommonItemProperties {
                clip_rect: full,
                clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(scene.pipeline),
                flags: PrimitiveFlags::default(),
            },
            full,
            ColorF::new(0.125, 0.25, 0.375, 1.0),
        );
        builder.push_image(
            &CommonItemProperties {
                clip_rect: image_rect,
                clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(scene.pipeline),
                flags: PrimitiveFlags::PREFER_COMPOSITOR_SURFACE,
            },
            image_rect,
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            key,
            ColorF::WHITE,
        );
        transaction.set_root_pipeline(scene.pipeline);
        transaction.set_display_list(Epoch(0), scene.api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        scene.api.send_transaction(scene.document, transaction);
        scene.epoch = 1;
        Self { scene, image, generation, releases, descriptor }
    }

    fn render_initial(&mut self, force: bool) {
        let size = self.scene.size;
        assert_full(render(&mut self.scene, force).delta, size);
        self.drain(1);
    }

    fn update(&mut self, color: [u8; 4]) {
        let device = self.scene.renderer.external_image_device();
        device.update_image(
            &self.image.borrow(),
            self.descriptor,
            &color.repeat(32 * 32),
        ).unwrap();
        self.generation.set(self.generation.get() + 1);
        let mut transaction = Transaction::new();
        transaction.invalidate_rendered_frame(RenderReasons::TESTING);
        self.scene.epoch += 1;
        transaction.generate_frame(
            self.scene.epoch as u64,
            true,
            false,
            RenderReasons::TESTING,
        );
        self.scene.api.send_transaction(self.scene.document, transaction);
    }

    fn drain(&mut self, expected_releases: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.releases.borrow().len() < expected_releases
            || self.scene.renderer.has_pending_gpu_work()
        {
            self.scene.renderer.poll().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
}

#[test]
#[ignore = "Requires Vulkan validation and WR_HAL_RENDER_METRICS=1"]
fn same_id_external_update_requires_full_composition() {
    let mut candidate = ExternalScene::new([255, 0, 0, 255]);
    let mut control = ExternalScene::new([255, 0, 0, 255]);
    candidate.render_initial(false);
    control.render_initial(true);
    assert_same_pixels(&candidate.scene, &control.scene);
    candidate.update([0, 0, 255, 255]);
    control.update([0, 0, 255, 255]);
    assert!(candidate.scene.prepare().render);
    assert!(!candidate
        .scene
        .renderer
        .core
        .document
        .as_ref()
        .unwrap()
        .frame
        .composite_state
        .external_surfaces
        .is_empty());
    let before = Counts::get(&candidate.scene.renderer);
    let outcome = candidate.scene.renderer.render_if_needed().unwrap();
    assert!(matches!(outcome, RenderOutcome::Rendered(_)));
    assert_full(
        Counts::get(&candidate.scene.renderer).since(before),
        candidate.scene.size,
    );
    let control_size = control.scene.size;
    assert_full(render(&mut control.scene, true).delta, control_size);
    assert_same_pixels(&candidate.scene, &control.scene);
    candidate.drain(2);
    control.drain(2);
    candidate.scene.shutdown();
    control.scene.shutdown();
}
