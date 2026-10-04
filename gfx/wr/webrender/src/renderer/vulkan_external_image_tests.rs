/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::{
    BufferPool, ExternalTextureRegistry, SubmissionQueue, Texture, TextureFilter,
};
use std::cell::Cell;

struct Handler {
    registry: Rc<ExternalTextureRegistry>,
    image: Rc<Texture>,
    locks: Rc<Cell<usize>>,
    unlocks: Rc<Cell<usize>>,
    invalid: Rc<Cell<bool>>,
    handles: Vec<Option<ExternalTextureHandle>>,
}

impl ExternalImageHandler for Handler {
    fn lock(&mut self, _: ExternalImageId, channel: u8, _: bool) -> ExternalImage<'_> {
        assert_eq!(channel, 0);
        let handle = (!self.invalid.get()).then(|| self.registry.register(&self.image).unwrap());
        self.handles.push(handle);
        self.locks.set(self.locks.get() + 1);
        ExternalImage {
            uv: TexelRect::new(0., 0., 2., 2.),
            source: handle.map_or(
                ExternalImageSource::Invalid,
                ExternalImageSource::NativeTexture,
            ),
        }
    }
    fn unlock(&mut self, _: ExternalImageId, _: u8) {
        if let Some(handle) = self.handles.pop().unwrap() {
            self.registry.unregister(handle).unwrap();
        }
        self.unlocks.set(self.unlocks.get() + 1);
    }
}

#[test]
#[ignore = "Requires Vulkan shaders and validation"]
fn vulkan_renderer_draws_registered_external_texture_handles() {
    validation_logging();
    let (tx, rx) = mpsc::channel();
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
    let registry = renderer.vulkan_external_textures().unwrap();
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
    renderer.set_external_image_handler(Box::new(Handler {
        registry: registry.clone(),
        image: image.clone(),
        locks: locks.clone(),
        unlocks: unlocks.clone(),
        invalid: invalid.clone(),
        handles: Vec::new(),
    }));
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(16, 16);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    for epoch in 0..4 {
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
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        } else {
            transaction.update_image(key, descriptor, data, &DirtyRect::All);
        }
        transaction.generate_frame(epoch + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        renderer.render(size, 0).unwrap();
        let output = renderer.device.vulkan_test_output().unwrap();
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
            }
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
