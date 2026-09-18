/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::*;
use api::*;
use crate::device::hal::{ExternalImageProvider, Options};
use crate::render_api::Transaction;
use crate::WebRenderOptions;
use std::cell::RefCell;

struct Notice;
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self)
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    fn shut_down(&self) {}
}

struct Provider(WeakVulkanDmaBufImage, TexelRect);
impl ExternalImageProvider for Provider {
    fn acquire(&mut self, _: ExternalImageId, _: u8, _: bool) -> Result<ExternalImageLease> {
        self.0
            .upgrade()
            .ok_or("Publication released too early")?
            .lease(self.1)
    }
}

fn device() -> ExternalImageDevice {
    crate::hal::create_vulkan_image_device(&Options {
        validation: true,
        ..Default::default()
    })
    .unwrap()
}

fn exported(producer: &ExternalImageDevice, format: ImageFormat, modifier: u64) -> DmaBufExport {
    let desc = ImageDescriptor::new(17, 9, format, ImageDescriptorFlags::IS_OPAQUE);
    let mut bytes = Vec::new();
    for y in 0..9 {
        for x in 0..17 {
            let mut pixel = [x * 11, y * 23, 71, 128];
            if format == ImageFormat::BGRA8 {
                pixel.swap(0, 2);
            }
            bytes.extend(pixel);
        }
    }
    let image = producer.create_image(desc, &bytes).unwrap();
    producer.export_dmabuf_image(&image, modifier).unwrap()
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF and sync-file sharing"]
fn vulkan_dmabuf_direct_sampling_and_lifetime() {
    let producer = device();
    let caps = producer.dmabuf_capabilities().unwrap();
    let mut tested = 0;
    for (format, modifiers) in caps.formats() {
        if !matches!(format, ImageFormat::RGBA8 | ImageFormat::BGRA8) {
            continue;
        }
        for &modifier in modifiers {
            let (mut renderer, sender) = crate::hal::create_vulkan_renderer(
                &Options {
                    validation: true,
                    ..Default::default()
                },
                WebRenderOptions::default(),
                Box::new(Notice),
            )
            .unwrap();
            let consumer = renderer.external_image_device();
            let export = exported(&producer, *format, modifier);
            let layout = export.plane().layout();
            let inspection =
                DmaBufPlane::new(export.plane().as_fd().try_clone_to_owned().unwrap(), layout);
            if !consumer.supports_dmabuf_sampling(layout) {
                continue;
            }
            assert_ne!(
                producer.vulkan_context().unwrap().device.handle(),
                consumer.vulkan_context().unwrap().device.handle()
            );
            let releases = Rc::new(RefCell::new(Vec::new()));
            let observed = releases.clone();
            let image = unsafe {
                consumer.import_vulkan_dmabuf(export.plane(), export.ready(), 1, move |status| {
                    observed.borrow_mut().push(status)
                })
            }
            .unwrap();
            assert!(image.belongs_to(&consumer));
            assert!(!image.belongs_to(&producer));
            let access = image.0 .0.release.access.as_ref().unwrap();
            assert_eq!(access.external_family, vk::QUEUE_FAMILY_EXTERNAL);
            assert!(Rc::ptr_eq(
                &image.0 .0.image.texture(&access.owner).unwrap(),
                &access.texture
            ));
            let uv = TexelRect::new(0.0, 0.0, 17.0, 9.0);
            let retained = image.lease(uv).unwrap();
            let weak = image.downgrade();
            renderer
                .set_external_image_provider(Box::new(Provider(image.downgrade(), uv)))
                .unwrap();
            drop(export);
            drop(image);
            let mut api = sender.create_api();
            let document = api.add_document(DeviceIntSize::new(36, 9));
            let pipeline = PipelineId(0, 0);
            let key = api.generate_image_key();
            let mut transaction = Transaction::new();
            transaction.add_image(
                key,
                ImageDescriptor::new(17, 9, *format, ImageDescriptorFlags::IS_OPAQUE),
                ImageData::External(ExternalImageData {
                    id: ExternalImageId(91),
                    channel_index: 0,
                    image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                    normalized_uvs: false,
                }),
                None,
            );
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.0);
            let info = CommonItemProperties {
                clip_rect: LayoutRect::from_size(LayoutSize::new(36.0, 9.0)),
                clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(pipeline),
                flags: PrimitiveFlags::default(),
            };
            builder.push_stacking_context(
                info.spatial_id,
                info.flags,
                None,
                TransformStyle::Flat,
                MixBlendMode::Normal,
                &[],
                &[],
                RasterSpace::Screen,
                StackingContextFlags::empty(),
                None,
            );
            for x in [0, 19] {
                builder.push_image(
                    &info,
                    LayoutRect::from_origin_and_size(
                        LayoutPoint::new(x as f32, 0.0),
                        LayoutSize::new(17.0, 9.0),
                    ),
                    ImageRendering::Auto,
                    AlphaType::PremultipliedAlpha,
                    key,
                    ColorF::WHITE,
                );
            }
            builder.pop_stacking_context();
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
            transaction.generate_frame(1, true, false, RenderReasons::TESTING);
            api.send_transaction(document, transaction);
            renderer.prepare_frame(document).unwrap();
            renderer.render().unwrap();
            let pixels = renderer
                .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
                    36, 9,
                )))
                .unwrap();
            for y in 0..9usize {
                for x in 0..17usize {
                    let expected = [(x * 11) as u8, (y * 23) as u8, 71, 255];
                    for origin in [0, 19] {
                        let offset = ((8 - y) * 36 + origin + x) * 4;
                        assert_eq!(
                            &pixels[offset..offset + 4],
                            &expected,
                            "format {:?} modifier {:#x}",
                            format,
                            modifier
                        );
                    }
                }
            }
            renderer.poll().unwrap();
            assert!(releases.borrow().is_empty());
            assert!(weak.upgrade().is_some());
            drop(retained);
            assert_eq!(*releases.borrow(), [ExternalImageRelease::Complete]);
            assert!(weak.upgrade().is_none());
            let copied = unsafe {
                producer.copy_dmabuf_planes(&[inspection], &SyncFile::already_signaled())
            }
            .unwrap();
            producer.wait_dmabuf_release(copied.release()).unwrap();
            let unchanged = producer.read_image(&copied.images()[0]).unwrap();
            assert!(unchanged.chunks_exact(4).all(|pixel| pixel[3] == 128));
            api.shut_down(true);
            tested += 1;
        }
    }
    assert!(tested >= 2);
    println!("Direct RGBA/BGRA sampled tuples: {}", tested);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF and sync-file sharing"]
fn vulkan_dmabuf_rejection_and_failure_release() {
    let producer = device();
    let export = exported(&producer, ImageFormat::RGBA8, 0);
    for fault in [None, Some(FailurePoint::Import), Some(FailurePoint::Submit)] {
        let export = exported(&producer, ImageFormat::RGBA8, 0);
        let consumer = device();
        let releases = Rc::new(RefCell::new(Vec::new()));
        let observed = releases.clone();
        if let Some(fault) = fault {
            consumer
                .dmabuf_producer()
                .unwrap()
                .owner
                .fault
                .set(Some(fault));
        }
        let image = unsafe {
            consumer.import_vulkan_dmabuf(
                export.plane(),
                export.ready(),
                if fault.is_none() { 0 } else { 1 },
                move |status| observed.borrow_mut().push(status),
            )
        };
        assert!(image.is_err());
        assert_eq!(releases.borrow().len(), 1);
        assert_eq!(
            releases.borrow()[0],
            if fault == Some(FailurePoint::Submit) {
                ExternalImageRelease::Abandoned
            } else {
                ExternalImageRelease::Unused
            }
        );
    }
    let consumer = device();
    let mut layout = export.plane().layout();
    layout.device_uuid[0] ^= 1;
    assert!(!consumer.supports_dmabuf_sampling(layout));
    layout = export.plane().layout();
    layout.driver_uuid[0] ^= 1;
    assert!(!consumer.supports_dmabuf_sampling(layout));
    layout = export.plane().layout();
    layout.modifier = u64::MAX;
    assert!(!consumer.supports_dmabuf_sampling(layout));
    let releases = Rc::new(RefCell::new(Vec::new()));
    let observed = releases.clone();
    let image = unsafe {
        consumer.import_vulkan_dmabuf(export.plane(), export.ready(), 2, move |status| {
            observed.borrow_mut().push(status)
        })
    }
    .unwrap();
    drop(image);
    assert_eq!(*releases.borrow(), [ExternalImageRelease::Unused]);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF and sync-file sharing"]
fn vulkan_dmabuf_ownership_return_failure_abandons() {
    let producer = device();
    let consumer = device();
    let export = exported(&producer, ImageFormat::RGBA8, 0);
    let releases = Rc::new(RefCell::new(Vec::new()));
    let observed = releases.clone();
    let image = unsafe {
        consumer.import_vulkan_dmabuf(export.plane(), export.ready(), 3, move |status| {
            observed.borrow_mut().push(status)
        })
    }
    .unwrap();
    consumer
        .dmabuf_producer()
        .unwrap()
        .owner
        .fault
        .set(Some(FailurePoint::Submit));
    drop(image);
    assert_eq!(*releases.borrow(), [ExternalImageRelease::Abandoned]);
    assert!(consumer.poll().is_err());
}
