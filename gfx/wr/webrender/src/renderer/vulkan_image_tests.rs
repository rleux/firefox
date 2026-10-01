/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn color(x: usize, y: usize, epoch: usize, alpha: u8) -> [u8; 4] {
    if epoch == 2 || (epoch == 1 && (1..3).contains(&x) && (1..3).contains(&y)) {
        [16, 96, 48, alpha]
    } else {
        [8 + x as u8 * 24, 8 + y as u8 * 24, 64, alpha]
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_tiles_images_that_prefer_compositor_surfaces() {
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
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(256, 64);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let oversized_width = renderer.device.max_texture_size() + 1;
    for (epoch, (width, tile_size, promoted)) in [
        (oversized_width, None, false),
        (256, Some(64), false),
        (256, None, true),
    ]
    .iter()
    .copied()
    .enumerate()
    {
        let image = api.generate_image_key();
        let pixel = |x: i32, y: i32| {
            [
                32 + epoch as u8 * 64,
                if x % 128 < 64 { 64 } else { 192 },
                if y < 32 { 32 } else { 224 },
                255,
            ]
        };
        let data = (0..64)
            .flat_map(|y| (0..width).flat_map(move |x| pixel(x, y)))
            .collect();
        let mut transaction = Transaction::new();
        transaction.add_image(
            image,
            ImageDescriptor::new(
                width,
                64,
                ImageFormat::RGBA8,
                ImageDescriptorFlags::IS_OPAQUE,
            ),
            ImageData::new(data),
            tile_size,
        );
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let viewport = LayoutRect::from_size(LayoutSize::new(256.0, 64.0));
        let mut common =
            CommonItemProperties::new(viewport, SpaceAndClipInfo::root_scroll(pipeline));
        common.flags |= PrimitiveFlags::PREFER_COMPOSITOR_SURFACE;
        builder.push_image(
            &common,
            LayoutRect::from_size(LayoutSize::new(width as f32, 64.0)),
            ImageRendering::Auto,
            AlphaType::PremultipliedAlpha,
            image,
            ColorF::WHITE,
        );
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        for updates in &renderer.pending_texture_updates {
            for allocation in &updates.allocations {
                if let TextureCacheAllocationKind::Alloc(info)
                | TextureCacheAllocationKind::Reset(info) = &allocation.kind
                {
                    assert!(
                        info.width <= renderer.device.max_texture_size(),
                        "Oversized image allocation: {}",
                        info.width
                    );
                }
            }
        }
        for redraw in 0..2 {
            renderer.render(size, 0).unwrap();
            assert_eq!(
                renderer.active_documents[&document]
                    .frame
                    .composite_state
                    .external_surfaces
                    .len(),
                usize::from(promoted)
            );
            let pixels = renderer
                .device
                .vulkan_test_output()
                .unwrap()
                .readback(DeviceIntRect::from_size(size))
                .unwrap()
                .wait()
                .unwrap();
            for y in [16, 48] {
                for x in [16, 80, 144, 208] {
                    let offset = ((y * 256 + x) * 4) as usize;
                    assert_eq!(
                        &pixels[offset..offset + 4],
                        &pixel(x, y),
                        "epoch {epoch}, redraw {redraw}, pixel ({x}, {y})"
                    );
                }
            }
        }
        let mut transaction = Transaction::new();
        transaction.delete_image(image);
        api.send_transaction(document, transaction);
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_preserves_images_across_scroll_cycles() {
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
            texture_cache_config: crate::texture_cache::TextureCacheConfig {
                color8_linear_texture_size: 512,
                ..crate::texture_cache::TextureCacheConfig::DEFAULT
            },
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(128, 128);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let scroll_id = ExternalScrollId(1, pipeline);
    let mut transaction = Transaction::new();
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.0);
    let full = LayoutRect::from_size(LayoutSize::new(128.0, 16384.0));
    let scroll = builder.define_scroll_frame(
        SpatialId::root_scroll_node(pipeline),
        scroll_id,
        full,
        LayoutRect::from_size(LayoutSize::new(128.0, 128.0)),
        LayoutVector2D::zero(),
        0,
        HasScrollLinkedEffect::No,
    );
    let common = CommonItemProperties::new(
        full,
        SpaceAndClipInfo {
            spatial_id: scroll,
            clip_chain_id: ClipChainId::INVALID,
        },
    );
    let color =
        |image: usize, bottom: bool| [32 + image as u8, if bottom { 192 } else { 32 }, 64, 255];
    for image in 0..128 {
        let key = api.generate_image_key();
        let bytes = (0..128)
            .flat_map(|y| color(image, y >= 64).repeat(128))
            .collect::<Vec<_>>();
        transaction.add_image(
            key,
            ImageDescriptor::new(
                128,
                128,
                ImageFormat::RGBA8,
                ImageDescriptorFlags::IS_OPAQUE,
            ),
            ImageData::new(bytes),
            None,
        );
        builder.push_image(
            &common,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(0.0, image as f32 * 128.0),
                LayoutSize::new(128.0, 128.0),
            ),
            ImageRendering::Auto,
            AlphaType::PremultipliedAlpha,
            key,
            ColorF::WHITE,
        );
    }
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
    api.send_transaction(document, transaction);
    let mut copied = false;
    for frame in 0..512 {
        let step = frame % 256;
        let offset = if step < 128 {
            step * 128
        } else {
            (255 - step) * 128
        };
        let mut transaction = Transaction::new();
        transaction.set_scroll_offsets(
            scroll_id,
            vec![SampledScrollOffset {
                offset: LayoutVector2D::new(0.0, offset as f32),
                generation: 0,
            }],
        );
        transaction.generate_frame(frame as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        copied |= renderer
            .pending_texture_updates
            .iter()
            .any(|updates| !updates.copies.is_empty());
        if frame % 3 == 0 {
            continue;
        }
        renderer.render(size, 0).unwrap();
        if frame % 4 != 3 {
            continue;
        }
        let pixels = renderer
            .device
            .vulkan_test_output()
            .unwrap()
            .readback(DeviceIntRect::from_size(size))
            .unwrap()
            .wait()
            .unwrap();
        for y in [16, 48, 80, 112] {
            let expected = color(offset / 128, y >= 64);
            let index = (y * 128 + 64) * 4;
            assert_eq!(
                &pixels[index..index + 4],
                &expected,
                "frame {frame}, offset {offset}, y {y}"
            );
        }
    }
    assert!(copied, "Scrolling must exercise texture-cache compaction");
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_updates_strided_images_and_reuses_cached_tiles() {
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
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(56, 12);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let images: Vec<_> = [ImageFormat::RGBA8, ImageFormat::BGRA8]
        .iter()
        .flat_map(|&format| [255, 128].iter().map(move |&alpha| (format, alpha)))
        .map(|(format, alpha)| (api.generate_image_key(), format, alpha))
        .collect();

    for epoch in 0..3 {
        let dimension = if epoch == 2 { 2 } else { 4 };
        let mut transaction = Transaction::new();
        for &(key, format, alpha) in &images {
            let mut descriptor = ImageDescriptor::new(
                dimension as i32,
                dimension as i32,
                format,
                if alpha == 255 {
                    ImageDescriptorFlags::IS_OPAQUE
                } else {
                    ImageDescriptorFlags::empty()
                },
            );
            let stride = dimension * 4 + 8;
            descriptor.stride = Some(stride as i32);
            descriptor.offset = 8;
            let mut data = vec![0xed; descriptor.offset as usize + stride * dimension];
            for y in 0..dimension {
                for x in 0..dimension {
                    let mut pixel = color(x, y, epoch, alpha);
                    if format == ImageFormat::BGRA8 {
                        pixel.swap(0, 2);
                    }
                    let offset = descriptor.offset as usize + y * stride + x * 4;
                    data[offset..offset + 4].copy_from_slice(&pixel);
                }
            }
            if epoch == 0 {
                transaction.add_image(key, descriptor, ImageData::new(data), None);
            } else {
                let dirty = if epoch == 1 {
                    DirtyRect::Partial(
                        DeviceIntRect::from_origin_and_size(
                            DeviceIntPoint::new(1, 1),
                            DeviceIntSize::new(2, 2),
                        )
                        .cast_unit(),
                    )
                } else {
                    DirtyRect::All
                };
                transaction.update_image(key, descriptor, ImageData::new(data), &dirty);
            }
        }
        if epoch == 0 {
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.0);
            let full = LayoutRect::from_size(LayoutSize::new(56.0, 12.0));
            let mut common =
                CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
            builder.push_rect(&common, full, ColorF::new(0.0, 0.0, 64.0 / 255.0, 1.0));
            for (index, &(key, _, _)) in images.iter().enumerate() {
                let origin = LayoutPoint::new((index * 14 + 2) as f32, 2.0);
                let bounds = LayoutRect::from_origin_and_size(origin, LayoutSize::new(8.0, 8.0));
                common.clip_rect = LayoutRect::from_origin_and_size(
                    origin + LayoutVector2D::new(2.0, 0.0),
                    LayoutSize::new(6.0, 6.0),
                );
                builder.push_image(
                    &common,
                    bounds,
                    ImageRendering::Pixelated,
                    AlphaType::PremultipliedAlpha,
                    key,
                    ColorF::WHITE,
                );
            }
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        }
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        for redraw in 0..2 {
            renderer.render(size, 0).unwrap();
            let pixels = renderer
                .device
                .vulkan_test_output()
                .unwrap()
                .readback(DeviceIntRect::from_size(size))
                .unwrap()
                .wait()
                .unwrap();
            for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                let x = index % 56;
                let y = index / 56;
                let image = x / 14;
                let local_x = x % 14;
                let expected = if (4..10).contains(&local_x) && (2..8).contains(&y) {
                    let alpha = images[image].2;
                    let mut pixel = color(
                        (local_x - 2) * dimension / 8,
                        (y - 2) * dimension / 8,
                        epoch,
                        alpha,
                    );
                    pixel[2] += ((64u32 * u32::from(255 - alpha) + 127) / 255) as u8;
                    pixel[3] = 255;
                    pixel
                } else {
                    [0, 0, 64, 255]
                };
                assert!(pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                    "epoch {}, redraw {}, pixel ({}, {}): {:?}, expected {:?}",
                    epoch, redraw, x, y, pixel, expected);
            }
        }
    }
    let mut transaction = Transaction::new();
    for &(key, _, _) in &images {
        transaction.delete_image(key);
    }
    api.send_transaction(document, transaction);
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
