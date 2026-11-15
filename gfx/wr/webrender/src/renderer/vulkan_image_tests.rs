/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_tiles_images_that_prefer_compositor_surfaces() {
    let (mut renderer, mut api, rx) = renderer();
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
            let pixels = capture(&mut renderer, size);
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

