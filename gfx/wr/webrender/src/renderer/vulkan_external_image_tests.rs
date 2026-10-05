/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{
    BufferPool, ExternalTextureRegistry, SubmissionQueue, Texture, TextureFilter,
};
use std::cell::Cell;

struct Handler {
    registry: Rc<ExternalTextureRegistry>,
    image: Rc<Texture>,
    locks: Rc<Cell<usize>>,
    unlocks: Rc<Cell<usize>>,
    invalid: Rc<Cell<bool>>,
    pending: Rc<Cell<bool>>,
    handles: Vec<Option<ExternalTextureHandle>>,
}

impl ExternalImageHandler for Handler {
    fn lock(&mut self, _: ExternalImageId, channel: u8, _: bool) -> ExternalImage<'_> {
        assert_eq!(channel, 0);
        let handle = (!self.invalid.get() && !self.pending.get())
            .then(|| self.registry.register(&self.image).unwrap());
        self.handles.push(handle);
        self.locks.set(self.locks.get() + 1);
        ExternalImage {
            uv: TexelRect::new(0., 0., 2., 2.),
            source: if self.pending.get() {
                ExternalImageSource::Pending
            } else {
                handle.map_or(
                    ExternalImageSource::Invalid,
                    ExternalImageSource::NativeTexture,
                )
            },
        }
    }
    fn unlock(&mut self, _: ExternalImageId, _: u8) {
        if let Some(handle) = self.handles.pop().unwrap() {
            self.registry.unregister(handle).unwrap();
        }
        self.unlocks.set(self.unlocks.get() + 1);
    }
}

struct RenderedNotice(mpsc::Sender<Checkpoint>);

impl NotificationHandler for RenderedNotice {
    fn notify(&self, when: Checkpoint) {
        let _ = self.0.send(when);
    }
}

#[test]
#[ignore = "Requires Vulkan shaders and validation"]
fn vulkan_renderer_draws_registered_external_texture_handles() {
    validation_logging();
    let (tx, rx) = mpsc::channel();
    let (rendered_tx, rendered_rx) = mpsc::channel();
    let (mut renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
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
    let image = Texture::new(
        registry.device(),
        2,
        2,
        wgpu_types::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let uploads = SubmissionQueue::new(&Rc::new(BufferPool::new(registry.device())), 2).unwrap();
    let locks = Rc::new(Cell::new(0));
    let unlocks = Rc::new(Cell::new(0));
    let invalid = Rc::new(Cell::new(false));
    let pending = Rc::new(Cell::new(false));
    renderer.set_external_image_handler(Box::new(Handler {
        registry: registry.clone(),
        image: image.clone(),
        locks: locks.clone(),
        unlocks: unlocks.clone(),
        invalid: invalid.clone(),
        pending: pending.clone(),
        handles: Vec::new(),
    }));
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(16, 16);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    let display_list = || {
        let bounds = LayoutRect::from_size(LayoutSize::new(16., 16.));
        let common = CommonItemProperties::new(bounds, SpaceAndClipInfo::root_scroll(pipeline));
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        builder.push_rect(&common, bounds, ColorF::new(0., 0., 1., 1.));
        builder.push_image(
            &common,
            bounds,
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            key,
            ColorF::WHITE,
        );
        builder.end()
    };
    for epoch in 0..5 {
        invalid.set(epoch == 2);
        let pixel = if epoch == 0 || epoch == 3 {
            [255, 0, 0, 255]
        } else {
            [0, 255, 0, 255]
        };
        image
            .upload(
                &uploads,
                DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
                &pixel.repeat(4),
                None,
                0,
                None,
            )
            .unwrap();
        uploads.submit().unwrap();
        let mut transaction = Transaction::new();
        let descriptor =
            ImageDescriptor::new(2, 2, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
        let data = ImageData::External(ExternalImageData {
            id: ExternalImageId(17),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        });
        if epoch == 0 {
            transaction.add_image(key, descriptor, data, None);
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(Epoch(0), api.get_namespace_id(), display_list());
        } else {
            transaction.update_image(key, descriptor, data, &DirtyRect::All);
        }
        transaction.generate_frame(epoch * 10 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        if epoch == 1 {
            renderer.notifications.push(NotificationRequest::new(
                Checkpoint::FrameRendered,
                Box::new(RenderedNotice(rendered_tx.clone())),
            ));
            renderer
                .pipeline_info
                .epochs
                .insert((pipeline, document), Epoch(77));
            pending.set(true);
            for _ in 0..2 {
                let result = renderer.render(size, 0).unwrap();
                assert!(result.external_images_pending);
                assert!(rendered_rx.try_recv().is_err());
                assert!(renderer.flush_pipeline_info().epochs.is_empty());
                assert_eq!(renderer.current_epoch(document, pipeline), Some(Epoch(77)));
                assert_eq!(result.present_result, Some(crate::PresentResult::Retry));
                assert!(renderer.active_documents[&document].frame.must_be_drawn());
                assert_eq!(locks.get(), unlocks.get());
                assert!(renderer.texture_resolver.external_images.is_empty());
                assert_eq!(
                    renderer
                        .device
                        .wgpu_test_output()
                        .unwrap()
                        .readback(DeviceIntRect::from_size(size))
                        .unwrap()
                        .wait()
                        .unwrap(),
                    [255, 0, 0, 255].repeat(16 * 16),
                );
            }
            let mut next = Transaction::new();
            next.update_image(
                key,
                ImageDescriptor::new(2, 2, ImageFormat::RGBA8, ImageDescriptorFlags::empty()),
                ImageData::External(ExternalImageData {
                    id: ExternalImageId(17),
                    channel_index: 0,
                    image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                    normalized_uvs: false,
                }),
                &DirtyRect::All,
            );
            next.generate_frame(12, true, false, RenderReasons::TESTING);
            api.send_transaction(document, next);
            rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
            assert!(renderer.update());
            assert!(renderer.pending_result_msg.is_some());
            assert!(renderer.render(size, 0).unwrap().external_images_pending);
            pending.set(false);
            assert!(!renderer.update());
            assert!(renderer.pending_result_msg.is_none());
        }
        if epoch >= 3 {
            pending.set(true);
            let resources = crate::internal_types::ResourceUpdateList {
                texture_updates: TextureUpdateList::new(),
                native_surface_updates: Vec::new(),
            };
            renderer.pending_result_msg = Some(if epoch == 3 {
                ResultMsg::UpdateResources {
                    resource_updates: resources,
                    memory_pressure: false,
                    discard_active_documents: true,
                    trim_upload_buffers: false,
                }
            } else {
                let doc = renderer.active_documents.remove(&document).unwrap();
                ResultMsg::RenderDocumentOffscreen(document, doc, resources)
            });
            assert!(renderer.update());
            assert!(renderer.pending_result_msg.is_some());
            assert_eq!(
                renderer.active_documents.contains_key(&document),
                epoch == 3
            );
            pending.set(false);
            assert!(!renderer.update());
            assert!(renderer.active_documents.is_empty());
            let mut next = Transaction::new();
            next.set_display_list(
                Epoch(epoch as u32 + 1),
                api.get_namespace_id(),
                display_list(),
            );
            next.invalidate_rendered_frame(RenderReasons::TESTING);
            next.generate_frame(epoch * 10 + 2, true, false, RenderReasons::TESTING);
            api.send_transaction(document, next);
            rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
            renderer.update();
        }
        renderer.render(size, 0).unwrap();
        if epoch == 1 {
            assert_eq!(rendered_rx.try_recv().unwrap(), Checkpoint::FrameRendered);
            assert!(renderer
                .flush_pipeline_info()
                .epochs
                .contains_key(&(pipeline, document)));
        }
        let output = renderer.device.wgpu_test_output().unwrap();
        assert_eq!(
            output
                .readback(DeviceIntRect::from_size(size))
                .unwrap()
                .wait()
                .unwrap(),
            if invalid.get() {
                [0, 0, 255, 255].repeat(16 * 16)
            } else {
                pixel.repeat(16 * 16)
            },
            "epoch {epoch}",
        );
    }
    assert!(locks.get() >= 4);
    assert_eq!(locks.get(), unlocks.get());
    api.delete_document(document);
    renderer.deinit();
    drop(uploads);
    drop(image);
    drop(registry);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
