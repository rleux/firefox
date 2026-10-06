/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::GpuBackend;
use crate::device::wgpu::{ExternalReleaseStatus, ExternalTextureRegistry, PendingExternalRelease};
use crate::device::wgpu::render_device::RenderDevice;

#[test]
#[ignore = "Requires Vulkan DMA-BUF images, shared timelines and validation"]
fn external_access_receipts_follow_submission_and_cancellation() {
    {
        let producer = device();
        let consumer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(&consumer).unwrap();
        let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
        let mut send = publish(&source, &ready, 1, None, [1., 0., 0., 1.]);
        let image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
        let mut renderer = RenderDevice::new(&consumer).unwrap();
        let registry = renderer.textures.external_textures();
        renderer.begin_frame().unwrap();
        unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.unwrap();
        let receipt = registry.release_dma_buf(&image, &released, 1).unwrap();
        assert_eq!(receipt.status(), ExternalReleaseStatus::Pending);
        assert_eq!(renderer.submissions.status().unwrap().submitted, 0);
        renderer.end_frame().unwrap();
        assert_eq!(receipt.status(), ExternalReleaseStatus::Submitted(1));
        renderer.begin_frame().unwrap();
        unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.unwrap();
        let discarded = registry.release_dma_buf(&image, &released, 2).unwrap();
        renderer.submissions.discard_recording();
        assert_eq!(discarded.status(), ExternalReleaseStatus::Abandoned);
        unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.unwrap();
        let retried = registry.release_dma_buf(&image, &released, 2).unwrap();
        renderer.end_frame().unwrap();
        assert_eq!(retried.status(), ExternalReleaseStatus::Submitted(2));
        assert_eq!(discarded.status(), ExternalReleaseStatus::Abandoned);
        renderer.submissions.wait().unwrap();
        consumer.lost.set(true);
        assert_eq!(retried.status(), ExternalReleaseStatus::Abandoned);
        consumer.lost.set(false);
        renderer.deinit();
        assert!(unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.is_err());
        assert!(registry.release_dma_buf(&image, &released, 3).is_err());
        assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images, shared timelines and validation"]
fn external_access_disconnects_failed_or_dropped_renderers() {
    {
        let producer = device();
        let consumer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(&consumer).unwrap();
        let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
        let mut send = publish(&source, &ready, 1, None, [0., 1., 0., 1.]);
        let image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
        let mut renderer = RenderDevice::new(&consumer).unwrap();
        let registry = renderer.textures.external_textures();
        renderer.begin_frame().unwrap();
        unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.unwrap();
        let failed = registry.release_dma_buf(&image, &released, 1).unwrap();
        renderer.operation::<()>(|_| Err("Injected renderer failure".into()));
        assert_eq!(failed.status(), ExternalReleaseStatus::Abandoned);
        assert!(unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.is_err());
        renderer.deinit();
        drop(renderer);
        let renderer = RenderDevice::new(&consumer).unwrap();
        let registry = renderer.textures.external_textures();
        unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.unwrap();
        let dropped = registry.release_dma_buf(&image, &released, 1).unwrap();
        let weak_queue = Rc::downgrade(&renderer.submissions);
        drop(renderer);
        assert!(weak_queue.upgrade().is_none());
        assert_eq!(dropped.status(), ExternalReleaseStatus::Abandoned);
        assert!(unsafe { registry.acquire_dma_buf(&image, &ready_import, 1) }.is_err());
        assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[cfg(feature = "debugger")]
mod window {
    use super::*;
    use api::*;
    use api::units::*;
    use crate::device::GpuBackendConfig;
    use crate::device::wgpu::X11Window;
    use crate::render_api::Transaction;
    use std::cell::{Cell, RefCell};
    use std::sync::mpsc;

    struct Notice(mpsc::Sender<()>);
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self(self.0.clone()))
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
            let _ = self.0.send(());
        }
    }

    struct Handler {
        registry: Rc<ExternalTextureRegistry>,
        image: Rc<DmaBufImage>,
        texture: Rc<Texture>,
        ready: Rc<SharedTimeline>,
        released: Rc<SharedTimeline>,
        value: Rc<Cell<u64>>,
        handles: Vec<ExternalTextureHandle>,
        receipts: Rc<RefCell<Vec<PendingExternalRelease>>>,
    }
    impl ExternalImageHandler for Handler {
        fn lock(&mut self, _: ExternalImageId, _: u8, _: bool) -> ExternalImage<'_> {
            if self.handles.is_empty() {
                unsafe {
                    self.registry
                        .acquire_dma_buf(&self.image, &self.ready, self.value.get())
                }
                .unwrap();
            }
            let handle = self.registry.register(&self.texture).unwrap();
            self.handles.push(handle);
            ExternalImage {
                uv: TexelRect::new(0., 0., 2., 2.),
                source: ExternalImageSource::NativeTexture(handle),
            }
        }
        fn unlock(&mut self, _: ExternalImageId, _: u8) {
            self.registry
                .unregister(self.handles.pop().unwrap())
                .unwrap();
            if self.handles.is_empty() {
                let receipt = self
                    .registry
                    .release_dma_buf(&self.image, &self.released, self.value.get())
                    .unwrap();
                assert_eq!(receipt.status(), ExternalReleaseStatus::Pending);
                self.receipts.borrow_mut().push(receipt);
            }
        }
    }

    #[test]
    #[ignore = "Requires Intel/X11 presentation, DMA-BUF and shared timelines"]
    fn external_access_callbacks_join_the_renderer_submission_queue() {
        validation_logging();
        let window = Rc::new(unsafe { X11Window::new() });
        let (tx, rx) = mpsc::channel();
        let (mut renderer, sender) = crate::create_webrender_instance(
            GpuBackendConfig::Vulkan(Options {
                window: Some(window.clone()),
                validation: true,
                ..Default::default()
            }),
            Box::new(Notice(tx)),
            crate::WebRenderOptions {
                enable_subpixel_aa: false,
                enable_debugger: false,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let registry = renderer.wgpu_external_textures().unwrap();
        let producer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import =
            SharedTimeline::import(registry.device(), &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(registry.device()).unwrap();
        let release_import =
            SharedTimeline::import(&producer, &released.export().unwrap()).unwrap();
        let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
        let image = unsafe {
            registry
                .device()
                .import_dma_buf(fd.as_fd(), *source.descriptor())
        }
        .unwrap();
        let texture = Texture::from_dma_buf(&image, TextureFilter::Linear, false).unwrap();
        let value = Rc::new(Cell::new(0));
        let receipts = Rc::new(RefCell::new(Vec::new()));
        renderer.set_external_image_handler(Box::new(Handler {
            registry: registry.clone(),
            image,
            texture,
            ready: ready_import,
            released,
            value: value.clone(),
            handles: Vec::new(),
            receipts: receipts.clone(),
        }));
        let mut api = sender.create_api();
        let size = DeviceIntSize::new(64, 48);
        let document = api.add_document(size);
        let pipeline = PipelineId(0, 0);
        let key = api.generate_image_key();
        for epoch in 0..3 {
            let point = epoch as u64 + 1;
            value.set(point);
            let mut color = [0., 0., 0., 1.];
            color[epoch] = 1.;
            let mut send = publish(
                &source,
                &ready,
                point,
                if epoch == 0 {
                    None
                } else {
                    Some((&release_import, point - 1))
                },
                color,
            );
            let mut transaction = Transaction::new();
            let desc =
                ImageDescriptor::new(2, 2, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
            let data = ImageData::External(ExternalImageData {
                id: ExternalImageId(97),
                channel_index: 0,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            });
            if epoch == 0 {
                transaction.add_image(key, desc, data, None);
                let mut builder = DisplayListBuilder::new(pipeline);
                builder.begin(60.);
                let full = LayoutRect::from_size(LayoutSize::new(64., 48.));
                let common =
                    CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
                for (x, rendering) in [(0., ImageRendering::Auto), (32., ImageRendering::Pixelated)]
                {
                    builder.push_image(
                        &common,
                        LayoutRect::from_origin_and_size(
                            LayoutPoint::new(x, 0.),
                            LayoutSize::new(32., 48.),
                        ),
                        rendering,
                        AlphaType::PremultipliedAlpha,
                        key,
                        ColorF::WHITE,
                    );
                }
                transaction.set_root_pipeline(pipeline);
                transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
            } else {
                transaction.update_image(key, desc, data, &DirtyRect::All);
            }
            transaction.generate_frame(point, true, false, RenderReasons::TESTING);
            api.send_transaction(document, transaction);
            rx.recv_timeout(Duration::from_secs(15)).unwrap();
            renderer.update();
            assert_eq!(
                renderer.render(size, 0).unwrap().present_result,
                Some(crate::PresentResult::Presented)
            );
            let pending = receipts.borrow_mut().drain(..).collect::<Vec<_>>();
            assert!(!pending.is_empty());
            assert!(pending
                .iter()
                .all(|receipt| receipt.status() == ExternalReleaseStatus::Submitted(point)));
            let expected = [0xff0000, 0x00ff00, 0x0000ff][epoch];
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !window
                .pixels([64, 48])
                .iter()
                .all(|&pixel| pixel == expected)
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "Window pixels did not update"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
        }
        api.delete_document(document);
        renderer.deinit();
        drop(registry);
        drop(source);
        drop(fd);
        drop(ready);
        drop(release_import);
        drop(producer);
        assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
    }
}

#[test]
#[cfg(feature = "capture")]
#[ignore = "Requires Vulkan DMA-BUF images, shared timelines and validation"]
fn external_readback_preserves_dma_buf_publication_ownership() {
    use api::{ImageBufferKind, ImageDescriptor, ImageDescriptorFlags, ImageFormat};

    let producer = device();
    let consumer = device();
    let ready = SharedTimeline::new(&producer).unwrap();
    let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
    let released = SharedTimeline::new(&consumer).unwrap();
    let release_import = SharedTimeline::import(&producer, &released.export().unwrap()).unwrap();
    let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
    let image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
    let mut renderer = RenderDevice::new(&consumer).unwrap();
    let registry = renderer.textures.external_textures();
    let descriptor = ImageDescriptor::new(2, 2, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
    for value in 1..=2 {
        let color = if value == 1 {
            [1., 0., 0., 0.]
        } else {
            [0., 0., 1., 0.]
        };
        let mut send = publish(
            &source,
            &ready,
            value,
            if value == 1 {
                None
            } else {
                Some((&release_import, value - 1))
            },
            color,
        );
        renderer.begin_frame().unwrap();
        unsafe { registry.acquire_dma_buf(&image, &ready_import, value) }.unwrap();
        for opaque in [false, true] {
            let texture = Texture::from_dma_buf(&image, TextureFilter::Nearest, opaque).unwrap();
            let handle = registry.register(&texture).unwrap();
            let mut pixel = if value == 1 {
                [255, 0, 0, 0]
            } else {
                [0, 0, 255, 0]
            };
            if opaque {
                pixel[3] = 255;
            }
            assert_eq!(
                GpuBackend::read_external_texture(
                    &mut renderer,
                    handle,
                    ImageBufferKind::Texture2D,
                    &descriptor
                ),
                pixel.repeat(4),
            );
            assert!(texture.sample_initialized());
            assert_eq!(texture.current_usage(), wgt::TextureUses::RESOURCE);
            registry.unregister(handle).unwrap();
        }
        let receipt = registry.release_dma_buf(&image, &released, value).unwrap();
        renderer.end_frame().unwrap();
        assert_eq!(receipt.status(), ExternalReleaseStatus::Submitted(value));
        renderer.submissions.wait().unwrap();
        assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(renderer.failure().is_none());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
