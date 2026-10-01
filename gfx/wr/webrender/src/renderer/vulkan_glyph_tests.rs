/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_draws_cached_glyphs_and_replaces_font_resources() {
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
    let size = DeviceIntSize::new(64, 52);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let mut font = api.generate_font_key();
    let mut instances = [
        api.generate_font_instance_key(),
        api.generate_font_instance_key(),
    ];
    let sizes = [20.0, 10.0];
    for epoch in 0..3 {
        let mut transaction = Transaction::new();
        if epoch == 2 {
            for &instance in &instances {
                transaction.delete_font_instance(instance);
            }
            transaction.delete_font(font);
            font = api.generate_font_key();
            instances = [
                api.generate_font_instance_key(),
                api.generate_font_instance_key(),
            ];
        }
        if epoch != 1 {
            transaction.add_raw_font(
                font,
                include_bytes!("../../../wrench/reftests/text/Ahem.ttf").to_vec(),
                0,
            );
            for (&instance, &size) in instances.iter().zip(&sizes) {
                transaction.add_font_instance(
                    instance,
                    font,
                    size,
                    Some(FontInstanceOptions {
                        render_mode: FontRenderMode::Alpha,
                        ..Default::default()
                    }),
                    None,
                    Vec::new(),
                );
            }
        }
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(64.0, 52.0));
        let root = SpaceAndClipInfo::root_scroll(pipeline);
        builder.push_rect(
            &CommonItemProperties::new(full, root),
            full,
            ColorF::new(0.0, 0.0, 64.0 / 255.0, 1.0),
        );
        let shift = epoch as f32 * 2.0;
        let square = |x, y, side| {
            LayoutRect::from_origin_and_size(LayoutPoint::new(x, y), LayoutSize::new(side, side))
        };
        let runs = [
            (
                0,
                square(4.0 + shift, 4.0, 20.0),
                if epoch == 0 {
                    ColorF::new(1.0, 0.0, 0.0, 1.0)
                } else {
                    ColorF::new(0.0, 1.0, 1.0, 1.0)
                },
                full,
            ),
            (
                0,
                square(32.0, 4.0 + shift, 20.0),
                ColorF::new(0.0, 1.0, 0.0, 1.0),
                full,
            ),
            (
                1,
                square(4.0 + shift, 32.0, 10.0),
                ColorF::new(1.0, 1.0, 1.0, 0.5),
                full,
            ),
            (
                0,
                square(32.0, 28.0, 20.0),
                ColorF::new(1.0, 0.0, 0.0, 0.5),
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(36.0, 30.0),
                    LayoutSize::new(10.0, 14.0),
                ),
            ),
        ];
        for &(instance, bounds, color, clip) in &runs {
            builder.push_text(
                &CommonItemProperties::new(clip, root),
                bounds,
                &[GlyphInstance {
                    index: 0x41,
                    // The Ahem fixture has a square glyph with baseline at 0.8em.
                    point: LayoutPoint::new(bounds.min.x, bounds.min.y + sizes[instance] * 0.8),
                }],
                instances[instance],
                color,
                None,
            );
        }
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        for redraw in 0..2 {
            renderer.render(size, 0).unwrap();
            let pixels = renderer
                .device
                .wgpu_test_output()
                .unwrap()
                .readback(DeviceIntRect::from_size(size))
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(pixels.len(), 64 * 52 * 4);
            for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                let x = index % 64;
                let y = index / 64;
                let point = LayoutPoint::new(x as f32 + 0.5, y as f32 + 0.5);
                let mut expected = [0.0, 0.0, 64.0];
                for &(_, bounds, color, clip) in &runs {
                    if bounds.contains(point) && clip.contains(point) {
                        for (channel, value) in [color.r, color.g, color.b].iter().enumerate() {
                            expected[channel] =
                                255.0 * value * color.a + expected[channel] * (1.0 - color.a);
                        }
                    }
                }
                let expected = [
                    expected[0].round() as u8,
                    expected[1].round() as u8,
                    expected[2].round() as u8,
                    255,
                ];
                assert!(
                    pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
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
    let mut transaction = Transaction::new();
    for &instance in &instances {
        transaction.delete_font_instance(instance);
    }
    transaction.delete_font(font);
    api.send_transaction(document, transaction);
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
