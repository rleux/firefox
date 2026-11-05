/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::wgpu::{
    ExternalTextureRegistry, NativeImage, SharedTimeline, PendingExternalRelease,
    ExternalReleaseStatus, native_test::Producer,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

struct NativeHandler {
    registry: Rc<ExternalTextureRegistry>,
    image: Rc<NativeImage>,
    ready: Rc<SharedTimeline>,
    point: Rc<Cell<u64>>,
    normal: ExternalTextureHandle,
    opaque: ExternalTextureHandle,
    locks: usize,
    acquired: bool,
    receipt: Rc<RefCell<Option<PendingExternalRelease>>>,
    pending_once: Rc<Cell<bool>>,
}
impl ExternalImageHandler for NativeHandler {
    fn lock(&mut self, _: ExternalImageId, channel: u8, _: bool) -> ExternalImage<'_> {
        self.locks += 1;
        let ready = unsafe {
            self.registry
                .acquire(&self.image, &self.ready, self.point.get())
        }
        .unwrap();
        self.acquired |= ready;
        ExternalImage {
            uv: TexelRect::new(0., 0., 4., 4.),
            source: if ready && !self.pending_once.replace(false) {
                ExternalImageSource::NativeTexture(if channel == 0 {
                    self.normal
                } else {
                    self.opaque
                })
            } else {
                ExternalImageSource::Pending
            },
        }
    }
    fn unlock(&mut self, _: ExternalImageId, _: u8) {
        self.locks -= 1;
        if self.locks == 0 && self.acquired {
            self.acquired = false;
            *self.receipt.borrow_mut() = Some(self.registry.release(&self.image).unwrap());
        }
    }
}

#[test]
#[ignore = "Requires native Vulkan DMA-BUF, external timelines and modern synchronization validation"]
fn vulkan_renderer_drop_retires_a_pending_first_import_on_its_old_device() {
    let (mut old, mut api, rx) = renderer();
    let registry = old.wgpu_external_textures().unwrap();
    let weak = Rc::downgrade(&registry);
    let old_device = std::sync::Arc::downgrade(registry.device());
    let producer = Producer::new();
    let (image, ready) = producer.import(&registry);
    let handle = registry.register(&image, false).unwrap();
    let receipt = Rc::new(RefCell::new(None));
    producer.publish(1, [0, 0, 255, 255], None);
    let _consumer_gate = producer.block_consumer(&registry, &ready, 2);
    old.set_external_image_handler(Box::new(NativeHandler {
        registry: registry.clone(),
        image: image.clone(),
        ready,
        point: Rc::new(Cell::new(1)),
        normal: handle,
        opaque: handle,
        locks: 0,
        acquired: false,
        receipt,
        pending_once: Rc::new(Cell::new(false)),
    }));
    let size = DeviceIntSize::new(4, 4);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    let mut transaction = Transaction::new();
    transaction.add_image(
        key,
        ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
        ImageData::External(ExternalImageData {
            id: ExternalImageId(88),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        }),
        None,
    );
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.);
    let rect = LayoutRect::from_size(LayoutSize::new(4., 4.));
    let common = CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline));
    builder.push_image(
        &common,
        rect,
        ImageRendering::Pixelated,
        AlphaType::PremultipliedAlpha,
        key,
        ColorF::WHITE,
    );
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
    old.update();
    assert!(old.render(size, 0).unwrap().external_images_pending);
    let old_submission = old.gpu_submission_status().unwrap().unwrap().submitted;
    let core_observation = registry.test_defer_core_completion();
    api.delete_document(document);
    old.deinit();
    let returned = image.last_release().unwrap();
    assert!(matches!(
        returned.status(),
        ExternalReleaseStatus::Submitted(_)
    ));
    drop(image);
    drop(registry);
    assert!(weak.upgrade().is_some());
    let (mut replacement, _, _) = renderer();
    assert!(
        replacement
            .gpu_submission_status()
            .unwrap()
            .unwrap()
            .submitted
            < old_submission
    );
    for _ in 0..3 {
        replacement.device.begin_frame();
        replacement.device.end_frame();
        replacement.gpu_submission_status().unwrap();
        assert!(weak.upgrade().is_some());
        assert_eq!(producer.ready_value(), 1);
    }
    producer.open_ready(2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !weak.upgrade().unwrap().test_native_signals_complete() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(crate::wgpu::poll_retired_native_images());
    assert!(
        weak.upgrade().is_some(),
        "native signals alone cannot release the core retirement closure"
    );
    core_observation.store(true, std::sync::atomic::Ordering::Release);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::wgpu::poll_retired_native_images() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(weak.upgrade().is_none());
    assert!(matches!(
        returned.status(),
        ExternalReleaseStatus::Submitted(_)
    ));
    drop(returned);
    drop(_consumer_gate);
    assert!(
        old_device.upgrade().is_none(),
        "old device/native closure must drain after retirement"
    );
    replacement.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires native Vulkan DMA-BUF, timelines and modern synchronization validation"]
fn vulkan_renderer_native_images_prepare_before_draw_and_reacquire_same_allocation() {
    let (mut renderer, mut api, rx) = renderer();
    let registry = renderer.wgpu_external_textures().unwrap();
    let producer = Producer::new();
    let (image, ready) = producer.import(&registry);
    let normal = registry.register(&image, false).unwrap();
    let opaque = registry.register(&image, true).unwrap();
    let point = Rc::new(Cell::new(1));
    let receipt = Rc::new(RefCell::new(None));
    let pending_once = Rc::new(Cell::new(false));
    renderer.set_external_image_handler(Box::new(NativeHandler {
        registry: registry.clone(),
        image: image.clone(),
        ready,
        point: point.clone(),
        normal,
        opaque,
        locks: 0,
        acquired: false,
        receipt: receipt.clone(),
        pending_once: pending_once.clone(),
    }));
    let size = DeviceIntSize::new(12, 4);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let keys = [
        api.generate_image_key(),
        api.generate_image_key(),
        api.generate_image_key(),
    ];
    for epoch in 0..4u32 {
        let color = [32 + epoch as u8 * 16, 48, 96, 128];
        let previous = receipt.borrow().clone();
        producer.publish(
            u64::from(epoch) + 1,
            color,
            previous
                .as_ref()
                .map(|r| (image.return_timeline(), r.value())),
        );
        point.set(u64::from(epoch) + 1);
        pending_once.set(epoch == 1);
        let mut transaction = Transaction::new();
        if epoch == 0 {
            for channel in 0..2 {
                transaction.add_image(
                    keys[channel],
                    ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::empty()),
                    ImageData::External(ExternalImageData {
                        id: ExternalImageId(77),
                        channel_index: channel as u8,
                        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                        normalized_uvs: false,
                    }),
                    None,
                );
            }
            transaction.add_image(
                keys[2],
                ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
                ImageData::new(vec![0; 64]),
                None,
            );
        }
        let mut cpu = [0, 255, 0, 255].repeat(16);
        cpu[2] = epoch as u8 * 17;
        transaction.update_image(
            keys[2],
            ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
            ImageData::new(cpu),
            &if epoch == 0 {
                DirtyRect::All
            } else {
                DirtyRect::Partial(DeviceIntRect::from_size(DeviceIntSize::new(1, 1)))
            },
        );
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let full = LayoutRect::from_size(LayoutSize::new(12., 4.));
        let common = CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
        builder.push_rect(&common, full, ColorF::new(1., 0., 0., 1.));
        for column in 0..3 {
            builder.push_image(
                &common,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(column as f32 * 4., 0.),
                    LayoutSize::new(4., 4.),
                ),
                ImageRendering::Pixelated,
                AlphaType::PremultipliedAlpha,
                keys[column],
                ColorF::WHITE,
            );
        }
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        let before = renderer.gpu_buffer_last_data_f.clone();
        let mut pending = 0;
        loop {
            let result = renderer.render(size, 0).unwrap();
            if !result.external_images_pending {
                break;
            }
            pending += 1;
            assert!(renderer.gpu_buffer_last_data_f == before);
            assert!(!renderer.active_documents[&document].frame.has_been_rendered);
            assert_eq!(result.present_result, Some(crate::PresentResult::Retry));
            renderer.gpu_submission_status().unwrap();
            assert!(pending < 100);
            std::thread::yield_now();
        }
        if epoch == 0 {
            assert!(pending > 0);
        }
        let pixels = capture(&mut renderer, size);
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            let expected = match index % 12 {
                0..=3 => [color[0] + 127, 48, 96, 255],
                4..=7 => [color[0], 48, 96, 255],
                _ => [0, 255, if index == 8 { epoch as u8 * 17 } else { 0 }, 255],
            };
            assert!(
                pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                "epoch={} pixel={} {:?}",
                epoch,
                index,
                pixel
            );
        }
        assert!(matches!(
            receipt.borrow().as_ref().unwrap().status(),
            ExternalReleaseStatus::Submitted(_)
        ));
        #[cfg(feature = "capture")]
        {
            assert!(unsafe {
                registry.acquire(&image, &producer.import_ready(&registry), point.get())
            }
            .unwrap());
            let descriptor =
                ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
            let captured = renderer.device.read_external_texture(
                normal,
                ImageBufferKind::Texture2D,
                &descriptor,
            );
            assert_eq!(captured, color.repeat(16));
            let captured = renderer.device.read_external_texture(
                opaque,
                ImageBufferKind::Texture2D,
                &descriptor,
            );
            assert_eq!(captured, [color[0], color[1], color[2], 255].repeat(16));
            *receipt.borrow_mut() = Some(registry.release(&image).unwrap());
            registry.flush_idle().unwrap();
        }
    }
    api.delete_document(document);
    renderer.deinit();
    while crate::wgpu::poll_retired_native_images() {
        std::thread::yield_now();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[cfg(feature = "debugger")]
#[test]
#[ignore = "Requires X11, Vulkan DMA-BUF/timelines and modern synchronization validation"]
fn vulkan_unknown_producer_ready_does_not_block_window_pause_detach_or_drop() {
    let (mut old, mut api, rx, window, display) = super::presentation::window_renderer(false);
    let size = DeviceIntSize::new(64, 48);
    let document = api.add_document(size);
    super::presentation::publish(&mut old, &mut api, &rx, document, 0);
    assert_eq!(
        old.render(size, 0).unwrap().present_result,
        Some(crate::PresentResult::Presented)
    );
    super::presentation::observe(&window);
    let registry = old.wgpu_external_textures().unwrap();
    let producer = Producer::new();
    let (image, ready) = producer.import(&registry);
    let handle = registry.register(&image, false).unwrap();
    producer.publish(1, [0, 255, 0, 255], None);
    old.set_external_image_handler(Box::new(NativeHandler {
        registry: registry.clone(),
        image: image.clone(),
        ready,
        point: Rc::new(Cell::new(2)),
        normal: handle,
        opaque: handle,
        locks: 0,
        acquired: false,
        receipt: Rc::new(RefCell::new(None)),
        pending_once: Rc::new(Cell::new(false)),
    }));
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    let mut transaction = Transaction::new();
    transaction.add_image(
        key,
        ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
        ImageData::External(ExternalImageData {
            id: ExternalImageId(99),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        }),
        None,
    );
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.);
    let rect = LayoutRect::from_size(LayoutSize::new(64., 48.));
    let common = CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline));
    builder.push_image(
        &common,
        rect,
        ImageRendering::Pixelated,
        AlphaType::PremultipliedAlpha,
        key,
        ColorF::WHITE,
    );
    transaction.set_display_list(Epoch(1), api.get_namespace_id(), builder.end());
    transaction.generate_frame(2, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
    old.update();
    for _ in 0..4 {
        assert!(old.render(size, 0).unwrap().external_images_pending);
        assert_eq!(producer.ready_value(), 1);
        assert!(image.last_release().is_none());
        super::presentation::observe(&window);
    }
    old.set_surface_paused(true).unwrap();
    old.set_wgpu_surface(None, Default::default()).unwrap();
    let weak_window = Rc::downgrade(&window);
    drop(window);
    assert!(
        weak_window.upgrade().is_none(),
        "detach must release the original XID lease before acknowledgement"
    );
    assert_eq!(producer.ready_value(), 1);
    api.delete_document(document);
    old.deinit();
    assert_eq!(
        producer.ready_value(),
        1,
        "producer gate stayed closed across pause, detach and drop"
    );
    assert!(
        image.last_release().is_none(),
        "no GPU acquisition means no submitted ownership return"
    );
    drop(registry);
    drop(image);
    while crate::wgpu::poll_retired_native_images() {
        std::thread::yield_now();
    }
    drop(display);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires native Vulkan DMA-BUF, external timelines and modern synchronization validation"]
fn vulkan_logical_loss_abandons_return_but_drains_native_refs_on_old_device() {
    let (mut old, mut api, rx) = renderer();
    let registry = old.wgpu_external_textures().unwrap();
    let weak = Rc::downgrade(&registry);
    let old_device = std::sync::Arc::downgrade(registry.device());
    let producer = Producer::new();
    let (image, ready) = producer.import(&registry);
    let handle = registry.register(&image, false).unwrap();
    let receipt = Rc::new(RefCell::new(None));
    producer.publish(1, [0, 0, 255, 255], None);
    assert!(!unsafe { registry.acquire(&image, &ready, 1) }.unwrap());
    registry.flush_idle().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !unsafe { registry.acquire(&image, &ready, 1) }.unwrap() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    let _consumer_gate = producer.block_consumer(&registry, &ready, 2);
    old.set_external_image_handler(Box::new(NativeHandler {
        registry: registry.clone(),
        image: image.clone(),
        ready,
        point: Rc::new(Cell::new(1)),
        normal: handle,
        opaque: handle,
        locks: 0,
        acquired: false,
        receipt,
        pending_once: Rc::new(Cell::new(false)),
    }));
    let size = DeviceIntSize::new(4, 4);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    let mut transaction = Transaction::new();
    transaction.add_image(
        key,
        ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
        ImageData::External(ExternalImageData {
            id: ExternalImageId(88),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        }),
        None,
    );
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.);
    let rect = LayoutRect::from_size(LayoutSize::new(4., 4.));
    let common = CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline));
    builder.push_image(
        &common,
        rect,
        ImageRendering::Pixelated,
        AlphaType::PremultipliedAlpha,
        key,
        ColorF::WHITE,
    );
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
    old.update();
    assert!(!old.render(size, 0).unwrap().external_images_pending);
    let returned = image.last_release().unwrap();
    assert!(matches!(
        returned.status(),
        ExternalReleaseStatus::Submitted(_)
    ));
    let old_submission = old.gpu_submission_status().unwrap().unwrap().submitted;
    old.device.wgpu_test_lose_device();
    assert_eq!(returned.status(), ExternalReleaseStatus::Abandoned);
    api.delete_document(document);
    old.deinit();
    assert_eq!(returned.status(), ExternalReleaseStatus::Abandoned);
    drop(image);
    drop(registry);
    assert!(weak.upgrade().is_some());
    let (mut replacement, _, _) = renderer();
    assert!(
        replacement
            .gpu_submission_status()
            .unwrap()
            .unwrap()
            .submitted
            < old_submission
    );
    for _ in 0..3 {
        replacement.device.begin_frame();
        replacement.device.end_frame();
        replacement.gpu_submission_status().unwrap();
        assert!(weak.upgrade().is_some());
        assert_eq!(producer.ready_value(), 1);
    }
    producer.open_ready(2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::wgpu::poll_retired_native_images() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(weak.upgrade().is_none());
    assert_eq!(returned.status(), ExternalReleaseStatus::Abandoned);
    drop(returned);
    drop(_consumer_gate);
    assert!(
        old_device.upgrade().is_none(),
        "old device/native closure must drain after retirement"
    );
    replacement.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF/timelines and modern synchronization validation"]
fn vulkan_pending_preserves_cached_frame_replacement_cleanup_and_offscreen_fifo() {
    use std::sync::{Arc, atomic::AtomicUsize};
    struct CheckpointNotice(Arc<AtomicUsize>);
    impl NotificationHandler for CheckpointNotice {
        fn notify(&self, when: Checkpoint) {
            assert_eq!(when, Checkpoint::FrameRendered);
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn empty_resources() -> crate::internal_types::ResourceUpdateList {
        crate::internal_types::ResourceUpdateList {
            texture_updates: crate::internal_types::TextureUpdateList::new(),
            native_surface_updates: Vec::new(),
        }
    }
    for mode in 0..3 {
        let (mut renderer, mut api, rx) = renderer();
        let registry = renderer.wgpu_external_textures().unwrap();
        let producer = Producer::new();
        let (image, ready) = producer.import(&registry);
        let handle = registry.register(&image, false).unwrap();
        producer.publish(1, [0, 0, 255, 255], None);
        renderer.set_external_image_handler(Box::new(NativeHandler {
            registry: registry.clone(),
            image,
            ready,
            point: Rc::new(Cell::new(2)),
            normal: handle,
            opaque: handle,
            locks: 0,
            acquired: false,
            receipt: Rc::new(RefCell::new(None)),
            pending_once: Rc::new(Cell::new(false)),
        }));
        let size = DeviceIntSize::new(32, 16);
        let document = api.add_document(size);
        let pipeline = PipelineId(0, 0);
        let native = api.generate_image_key();
        let cpu = api.generate_image_key();
        let mut transaction = Transaction::new();
        transaction.add_image(
            native,
            ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
            ImageData::External(ExternalImageData {
                id: ExternalImageId(101),
                channel_index: 0,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            }),
            None,
        );
        transaction.add_image(
            cpu,
            ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
            ImageData::new([255, 0, 0, 255].repeat(16)),
            None,
        );
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let full = LayoutRect::from_size(LayoutSize::new(32., 16.));
        let common = CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
        builder.push_image(
            &common,
            LayoutRect::from_size(LayoutSize::new(4., 4.)),
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            native,
            ColorF::WHITE,
        );
        builder.push_image(
            &common,
            LayoutRect::from_origin_and_size(LayoutPoint::new(8., 8.), LayoutSize::new(4., 4.)),
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            cpu,
            ColorF::WHITE,
        );
        builder.push_box_shadow(
            &common,
            LayoutRect::from_origin_and_size(LayoutPoint::new(20., 4.), LayoutSize::new(6., 6.)),
            LayoutVector2D::zero(),
            ColorF::BLACK,
            3.,
            0.,
            BorderRadius::zero(),
            BorderRadius::zero(),
            BoxShadowClipMode::Outset,
        );
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        assert!(!renderer.update());
        assert!(
            renderer.active_documents[&document]
                .frame
                .has_texture_cache_tasks,
            "fixture must contain real cache writes"
        );
        assert!(renderer.render(size, 0).unwrap().external_images_pending);
        let snapshot = renderer.gpu_buffer_last_data_f.clone();
        let notices = Arc::new(AtomicUsize::new(0));
        if mode == 0 {
            let mut transaction = Transaction::new();
            transaction.update_image(
                cpu,
                ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
                ImageData::new([0, 255, 0, 255].repeat(16)),
                &DirtyRect::All,
            );
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.);
            builder.push_image(
                &common,
                full,
                ImageRendering::Pixelated,
                AlphaType::PremultipliedAlpha,
                cpu,
                ColorF::WHITE,
            );
            transaction.set_display_list(Epoch(1), api.get_namespace_id(), builder.end());
            transaction.generate_frame(2, true, false, RenderReasons::TESTING);
            api.send_transaction(document, transaction);
            rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        } else if mode == 1 {
            renderer.pending_result_msg = Some(ResultMsg::UpdateResources {
                resource_updates: empty_resources(),
                memory_pressure: false,
                discard_active_documents: true,
                trim_upload_buffers: false,
            });
        } else {
            let doc = renderer.active_documents.remove(&document).unwrap();
            renderer.pending_result_msg = Some(ResultMsg::RenderDocumentOffscreen(
                document,
                doc,
                empty_resources(),
            ));
            let (tx, receiver) = api::channel::unbounded_channel();
            renderer.result_rx = receiver;
            tx.send(ResultMsg::AppendNotificationRequests(vec![
                NotificationRequest::new(
                    Checkpoint::FrameRendered,
                    Box::new(CheckpointNotice(notices.clone())),
                ),
            ]))
            .unwrap();
            tx.send(ResultMsg::ForceRedraw).unwrap();
        }
        for _ in 0..3 {
            assert!(renderer.update());
            assert!(renderer.pending_result_msg.is_some());
            assert!(renderer.gpu_buffer_last_data_f == snapshot);
            assert!(renderer.flush_pipeline_info().epochs.is_empty());
            assert_eq!(notices.load(Ordering::Relaxed), 0);
            if mode != 2 {
                assert!(!renderer.active_documents[&document].frame.has_been_rendered);
                assert!(!renderer.active_documents[&document]
                    .frame
                    .deferred_resolves
                    .is_empty());
            }
        }
        producer.open_ready(2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while renderer.update() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(renderer.pending_result_msg.is_none());
        if mode == 0 {
            assert!(renderer.active_documents[&document]
                .frame
                .deferred_resolves
                .is_empty());
            assert!(!renderer.render(size, 0).unwrap().external_images_pending);
            assert_eq!(
                capture(&mut renderer, size),
                [0, 255, 0, 255].repeat((32 * 16) as usize)
            );
        } else {
            assert!(renderer.active_documents.is_empty());
            if mode == 2 {
                assert_eq!(renderer.notifications.len(), 1);
                assert_eq!(notices.load(Ordering::Relaxed), 0);
                renderer.render(size, 0).unwrap();
                assert_eq!(notices.load(Ordering::Relaxed), 1);
            }
        }
        api.delete_document(document);
        renderer.deinit();
        drop(registry);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::wgpu::poll_retired_native_images() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires native Vulkan DMA-BUF, timelines and modern synchronization validation"]
fn vulkan_native_canvas_snapshot_preserves_alpha_and_restores_sampling_state() {
    let (renderer, _api, _rx) = renderer();
    let registry = renderer.wgpu_external_textures().unwrap();
    let producer = Producer::new();
    let color = [64, 128, 191, 0];
    producer.publish(1, color, None);
    let (image, ready) = producer.import(&registry);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for _ in 0..2 {
        loop {
            if unsafe { registry.acquire(&image, &ready, 1) }.unwrap() { break; }
            registry.flush_idle().unwrap();
            registry.poll().unwrap();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut pixels = vec![0; 64];
        registry.readback(&image, &mut pixels).unwrap();
        assert_eq!(pixels, color.repeat(16));
        assert!(registry.readback(&image, &mut pixels[..63]).is_err());
        let returned = registry.release(&image).unwrap();
        registry.flush_idle().unwrap();
        while matches!(returned.status(), ExternalReleaseStatus::Pending) {
            registry.poll().unwrap();
            assert!(std::time::Instant::now() < deadline);
        }
        assert!(matches!(returned.status(), ExternalReleaseStatus::Submitted(_)));
    }
    renderer.deinit();
}

#[test]
#[ignore = "Requires native Vulkan DMA-BUF, timelines and modern synchronization validation"]
fn vulkan_native_pending_document_survives_backend_noop_and_retains_epoch() {
    struct FrameNotice(std::sync::mpsc::Sender<(bool, bool)>);
    impl RenderNotifier for FrameNotice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self(self.0.clone()))
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, params: &FrameReadyParams) {
            let _ = self.0.send((params.render, params.present));
        }
    }
    validation_logging();
    for present in [true, false] {
        eprintln!("native no-op epoch case: present={present}");
        let (tx, rx) = std::sync::mpsc::channel();
        let (mut renderer, sender) = crate::create_webrender_instance(
            GpuBackendConfig::Vulkan(Options {
                validation: true,
                ..Default::default()
            }),
            Box::new(FrameNotice(tx)),
            crate::WebRenderOptions {
                clear_color: ColorF::WHITE,
                enable_subpixel_aa: false,
                enable_debugger: false,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let mut api = sender.create_api();
        let registry = renderer.wgpu_external_textures().unwrap();
        let producer = Producer::new();
        producer.publish(1, [0, 0, 255, 255], None);
        assert_eq!(
            producer.ready_value(),
            1,
            "initialized seed before import: present={present}"
        );
        let (image, ready) = producer.import(&registry);
        let handle = registry.register(&image, false).unwrap();
        let point = Rc::new(Cell::new(2));
        renderer.set_external_image_handler(Box::new(NativeHandler {
            registry: registry.clone(),
            image,
            ready,
            point: point.clone(),
            normal: handle,
            opaque: handle,
            locks: 0,
            acquired: false,
            receipt: Rc::new(RefCell::new(None)),
            pending_once: Rc::new(Cell::new(false)),
        }));
        let size = DeviceIntSize::new(4, 4);
        let document = api.add_document(size);
        let pipeline = PipelineId(0, 0);
        let key = api.generate_image_key();
        let snapshot = (!present).then(|| SnapshotImageKey(api.generate_image_key()));
        let mut transaction = Transaction::new();
        transaction.add_image(
            key,
            ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
            ImageData::External(ExternalImageData {
                id: ExternalImageId(901),
                channel_index: 0,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            }),
            None,
        );
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let rect = LayoutRect::from_size(LayoutSize::new(4., 4.));
        if let Some(snapshot) = snapshot {
            transaction.add_snapshot_image(snapshot);
            builder.push_stacking_context(
                SpaceAndClipInfo::root_scroll(pipeline).spatial_id,
                PrimitiveFlags::default(),
                None,
                TransformStyle::Flat,
                MixBlendMode::Normal,
                &[],
                &[],
                RasterSpace::Screen,
                StackingContextFlags::empty(),
                Some(SnapshotInfo {
                    key: snapshot,
                    area: rect,
                    detached: true,
                }),
            );
        }
        builder.push_image(
            &CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline)),
            rect,
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            key,
            ColorF::WHITE,
        );
        if snapshot.is_some() {
            builder.pop_stacking_context();
        }
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(7), api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, present, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(15))
                .unwrap()
                .0
        );
        assert!(!renderer.update(), "initial update: present={present}");
        let frame = &renderer.active_documents[&document].frame;
        assert!(
            !frame.deferred_resolves.is_empty(),
            "native sampling required: present={present}"
        );
        if !present {
            assert!(
                frame.has_texture_cache_tasks,
                "offscreen snapshot must have real cache tasks"
            );
        }
        let render_size = if present { size } else { DeviceIntSize::zero() };
        assert!(
            renderer
                .render(render_size, 0)
                .unwrap()
                .external_images_pending,
            "initial native Pending required: present={present}"
        );
        assert_eq!(renderer.pending_native_document_present(), Some(present));
        assert!(renderer.flush_pipeline_info().epochs.is_empty());
        assert_eq!(
            renderer.pipeline_info.epochs.get(&(pipeline, document)),
            Some(&Epoch(7))
        );

        // Exercise the actual backend cache optimization which caused Gecko's
        // next notifier to select Update instead of retrying the pending render.
        let mut noop = Transaction::new();
        noop.generate_frame(2, !present, false, RenderReasons::TESTING);
        api.send_transaction(document, noop);
        let (render, _) = rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        assert!(
            !render,
            "backend must produce a real update-only notification: present={present}"
        );
        for _ in 0..3 {
            assert!(!renderer.update(), "FIFO update is not blocked");
            assert_eq!(renderer.pending_native_document_present(), Some(present));
            assert!(renderer.flush_pipeline_info().epochs.is_empty());
            assert_eq!(
                renderer.pipeline_info.epochs.get(&(pipeline, document)),
                Some(&Epoch(7))
            );
        }
        producer.open_ready(2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while renderer
            .render(render_size, 0)
            .unwrap()
            .external_images_pending
        {
            registry.flush_idle().unwrap();
            registry.poll().unwrap();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(renderer.pending_native_document_present(), None);
        assert_eq!(
            renderer
                .flush_pipeline_info()
                .epochs
                .get(&(pipeline, document)),
            Some(&Epoch(7))
        );
        if present {
            assert_eq!(capture(&mut renderer, size), [0, 0, 255, 255].repeat(16));
        }
        // Replacement releases the pending-document guard during pre-update.
        // Keep its epoch in Rust until the selected real render has finished.
        point.set(3);
        assert!(
            renderer
                .render(render_size, 0)
                .unwrap()
                .external_images_pending
        );
        assert_eq!(renderer.pending_native_document_present(), Some(present));
        assert!(renderer.flush_pipeline_info().epochs.is_empty());
        let mut replacement = Transaction::new();
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        builder.push_rect(
            &CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline)),
            rect,
            ColorF::new(0., 1., 0., 1.),
        );
        if let Some(snapshot) = snapshot {
            replacement.delete_snapshot_image(snapshot);
        }
        replacement.set_display_list(Epoch(8), api.get_namespace_id(), builder.end());
        replacement.generate_frame(3, present, false, RenderReasons::TESTING);
        api.send_transaction(document, replacement);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(15))
                .unwrap()
                .0
        );
        assert!(!renderer.update());
        assert_eq!(renderer.pending_native_document_present(), None);
        assert_eq!(
            renderer.pipeline_info.epochs.get(&(pipeline, document)),
            Some(&Epoch(8))
        );
        assert!(
            !renderer
                .render(render_size, 0)
                .unwrap()
                .external_images_pending
        );
        assert_eq!(
            renderer
                .flush_pipeline_info()
                .epochs
                .get(&(pipeline, document)),
            Some(&Epoch(8))
        );
        assert!(
            renderer.flush_pipeline_info().epochs.is_empty(),
            "epoch retires exactly once"
        );
        if present {
            assert_eq!(capture(&mut renderer, size), [0, 255, 0, 255].repeat(16));
        }
        api.delete_document(document);
        renderer.deinit();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
