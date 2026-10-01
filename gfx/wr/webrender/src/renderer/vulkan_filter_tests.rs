/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn coverage(position: i32, interval: std::ops::Range<i32>, sigma: f32) -> f32 {
    if sigma == 0.0 {
        return if interval.contains(&position) {
            1.0
        } else {
            0.0
        };
    }
    let radius = (3.0 * sigma).ceil() as i32;
    let mut total = 0.0;
    let mut covered = 0.0;
    for offset in -radius..=radius {
        let weight = (-(offset * offset) as f32 / (2.0 * sigma * sigma)).exp();
        total += weight;
        if interval.contains(&(position + offset)) {
            covered += weight;
        }
    }
    covered / total
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_blurs_opacity_groups_and_updates_filter_parameters() {
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
    let size = DeviceIntSize::new(48, 40);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    for (epoch, (sigma, opacity)) in [(0.0, 0.5), (2.0, 0.5), (3.0, 0.75)]
        .iter()
        .copied()
        .enumerate()
    {
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(48.0, 40.0));
        let root = SpaceAndClipInfo::root_scroll(pipeline);
        let common = CommonItemProperties::new(full, root);
        builder.push_rect(&common, full, ColorF::new(0.0, 0.0, 128.0 / 255.0, 1.0));
        builder.push_simple_stacking_context_with_filters(
            root.spatial_id,
            PrimitiveFlags::IS_BACKFACE_VISIBLE,
            &[
                FilterOp::Blur(sigma, sigma, true),
                FilterOp::Opacity(PropertyBinding::Value(opacity), opacity),
            ],
            &[],
        );
        for (x, color) in [
            (12.0, ColorF::new(1.0, 0.0, 0.0, 1.0)),
            (20.0, ColorF::new(0.0, 1.0, 0.0, 1.0)),
        ] {
            builder.push_rect(
                &common,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(x, 12.0),
                    LayoutSize::new(16.0, 16.0),
                ),
                color,
            );
        }
        builder.pop_stacking_context();
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
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
            assert_eq!(pixels.len(), 48 * 40 * 4);
            for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                let x = (index % 48) as i32;
                let y = (index / 48) as i32;
                let vertical = coverage(y, 12..28, sigma) * opacity;
                let red = coverage(x, 12..20, sigma) * vertical;
                let green = coverage(x, 20..36, sigma) * vertical;
                let expected = [
                    (255.0 * red).round() as u8,
                    (255.0 * green).round() as u8,
                    (128.0 * (1.0 - red - green)).round() as u8,
                    255,
                ];
                assert!(
                    pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 3),
                    "epoch {}, redraw {}, pixel ({}, {}): {:?}, expected {:?}",
                    epoch,
                    redraw,
                    x,
                    y,
                    pixel,
                    expected
                );
            }
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
