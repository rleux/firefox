/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::{cell::Cell, rc::Rc};

struct CpuBuffers {
    bytes: [Vec<u8>; 5],
    value: Rc<Cell<u8>>,
    locks: Rc<Cell<usize>>,
    unlocks: Rc<Cell<usize>>,
    locked: Option<(ExternalImageId, u8)>,
}

fn plane(id: ExternalImageId, channel: u8) -> usize {
    match (id.0, channel) {
        (0 | 1, 0) => id.0 as usize,
        (2, 0..=2) => 2 + channel as usize,
        _ => panic!("Unexpected external buffer or channel"),
    }
}

impl ExternalImageHandler for CpuBuffers {
    fn lock(&mut self, id: ExternalImageId, channel: u8, composited: bool) -> ExternalImage<'_> {
        assert!(!composited);
        assert!(self.locked.replace((id, channel)).is_none());
        self.locks.set(self.locks.get() + 1);
        let index = plane(id, channel);
        let (width, height, offset, stride) = match index {
            0 | 1 => (5, 3, 5, 28),
            2 => (5, 3, 3, 8),
            _ => (3, 2, 1, 4),
        };
        let bytes = &mut self.bytes[index];
        bytes.fill(0xe7);
        for y in 0..height {
            for x in 0..width {
                if index < 2 {
                    let pixel = if index == 0 {
                        [self.value.get(), 48, 96, 255]
                    } else {
                        [96, 48, self.value.get(), 255]
                    };
                    let start = offset + y * stride + x * 4;
                    bytes[start..start + 4].copy_from_slice(&pixel);
                } else {
                    bytes[offset + y * stride + x] =
                        if index == 2 { self.value.get() } else { 128 };
                }
            }
        }
        ExternalImage {
            uv: TexelRect::new(0.0, 0.0, width as f32, height as f32),
            source: ExternalImageSource::RawData(bytes),
        }
    }

    fn unlock(&mut self, id: ExternalImageId, channel: u8) {
        assert_eq!(self.locked.take(), Some((id, channel)));
        self.unlocks.set(self.unlocks.get() + 1);
        self.bytes[plane(id, channel)].fill(0xe7);
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_stages_external_rgb_and_video_bytes_before_unlock() {
    let (mut renderer, mut api, rx) = renderer();
    let value = Rc::new(Cell::new(32));
    let locks = Rc::new(Cell::new(0));
    let unlocks = Rc::new(Cell::new(0));
    renderer.set_external_image_handler(Box::new(CpuBuffers {
        bytes: [
            vec![0; 89],
            vec![0; 89],
            vec![0; 27],
            vec![0; 9],
            vec![0; 9],
        ],
        value: value.clone(),
        locks: locks.clone(),
        unlocks: unlocks.clone(),
        locked: None,
    }));
    let size = DeviceIntSize::new(30, 6);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let keys: [ImageKey; 5] = std::array::from_fn(|_| api.generate_image_key());
    for epoch in 0..4 {
        value.set(32 + epoch as u8 * 64);
        let mut transaction = Transaction::new();
        for (index, key) in keys.iter().copied().enumerate() {
            let (width, height, offset, stride, format, id, channel) = match index {
                0 => (5, 3, 5, 28, ImageFormat::RGBA8, 0, 0),
                1 => (5, 3, 5, 28, ImageFormat::BGRA8, 1, 0),
                2 => (5, 3, 3, 8, ImageFormat::R8, 2, 0),
                _ => (3, 2, 1, 4, ImageFormat::R8, 2, (index - 2) as u8),
            };
            let mut descriptor =
                ImageDescriptor::new(width, height, format, ImageDescriptorFlags::IS_OPAQUE);
            descriptor.offset = offset;
            descriptor.stride = Some(stride);
            let data = ImageData::External(ExternalImageData {
                id: ExternalImageId(id),
                channel_index: channel,
                image_type: ExternalImageType::Buffer,
                normalized_uvs: false,
            });
            if epoch == 0 {
                transaction.add_image(key, descriptor, data, None);
            } else {
                transaction.update_image(key, descriptor, data, &DirtyRect::All);
            }
        }
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(30.0, 6.0));
        let common = CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
        for index in 0..2 {
            builder.push_image(
                &common,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(index as f32 * 10.0, 0.0),
                    LayoutSize::new(10.0, 6.0),
                ),
                ImageRendering::Pixelated,
                AlphaType::PremultipliedAlpha,
                keys[index],
                ColorF::WHITE,
            );
        }
        builder.push_yuv_image(
            &common,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(20.0, 0.0),
                LayoutSize::new(10.0, 6.0),
            ),
            YuvData::PlanarYCbCr(keys[2], keys[3], keys[4]),
            ColorDepth::Color8,
            YuvColorSpace::Rec709,
            ColorRange::Full,
            ImageRendering::Auto,
        );
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        for _ in 0..2 {
            renderer.render(size, 0).unwrap();
            let pixels = capture(&mut renderer, size);
            for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                let expected = if index % 30 < 20 {
                    [value.get(), 48, 96, 255]
                } else {
                    [value.get(), value.get(), value.get(), 255]
                };
                assert!(
                    pixel
                        .iter()
                        .zip(expected)
                        .all(|(&actual, expected)| actual.abs_diff(expected) <= 2),
                    "epoch {}, pixel {}: {:?}, expected {:?}", epoch, index, pixel, expected
                );
            }
            assert_eq!(locks.get(), unlocks.get());
            assert!(locks.get() >= 5 * (epoch as usize + 1));
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
