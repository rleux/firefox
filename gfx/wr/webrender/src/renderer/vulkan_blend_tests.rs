/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn blend(mode: MixBlendMode, backdrop: f32, source: f32) -> f32 {
    match mode {
        MixBlendMode::Multiply => backdrop * source,
        MixBlendMode::Screen => backdrop + source - backdrop * source,
        MixBlendMode::Difference => (backdrop - source).abs(),
        MixBlendMode::Exclusion => backdrop + source - 2.0 * backdrop * source,
        MixBlendMode::Overlay if backdrop <= 0.5 => 2.0 * backdrop * source,
        MixBlendMode::Overlay => 1.0 - 2.0 * (1.0 - backdrop) * (1.0 - source),
        _ => unreachable!(),
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_blends_against_backdrop_and_preserves_source_alpha() {
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
    let size = DeviceIntSize::new(32, 24);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let backgrounds = [
        [32u8, 96, 192],
        [192, 128, 32],
        [160, 32, 96],
        [16, 192, 128],
    ];
    let source = [128u8, 64, 192];
    let modes = [
        MixBlendMode::Multiply,
        MixBlendMode::Screen,
        MixBlendMode::Difference,
        MixBlendMode::Overlay,
        MixBlendMode::Exclusion,
    ];
    for (epoch, (mode, alpha)) in modes
        .iter()
        .flat_map(|&mode| [1.0f32, 0.5].iter().map(move |&alpha| (mode, alpha)))
        .enumerate()
    {
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(32.0, 24.0));
        let root = SpaceAndClipInfo::root_scroll(pipeline);
        let common = CommonItemProperties::new(full, root);
        for (index, color) in backgrounds.iter().enumerate() {
            builder.push_rect(
                &common,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new((index % 2 * 16) as f32, (index / 2 * 12) as f32),
                    LayoutSize::new(16.0, 12.0),
                ),
                ColorF::new(
                    color[0] as f32 / 255.0,
                    color[1] as f32 / 255.0,
                    color[2] as f32 / 255.0,
                    1.0,
                ),
            );
        }
        builder.push_stacking_context(
            root.spatial_id,
            PrimitiveFlags::IS_BACKFACE_VISIBLE,
            None,
            TransformStyle::Flat,
            mode,
            &[],
            &[],
            RasterSpace::Screen,
            StackingContextFlags::empty(),
            None,
        );
        builder.push_rect(
            &common,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(8.0, 6.0),
                LayoutSize::new(16.0, 12.0),
            ),
            ColorF::new(
                source[0] as f32 / 255.0,
                source[1] as f32 / 255.0,
                source[2] as f32 / 255.0,
                alpha,
            ),
        );
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
                .wgpu_test_output()
                .unwrap()
                .readback(DeviceIntRect::from_size(size))
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(pixels.len(), 32 * 24 * 4);
            for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                let x = index % 32;
                let y = index / 32;
                let background = backgrounds[(y / 12) * 2 + x / 16];
                let mut expected = [background[0], background[1], background[2], 255];
                if (8..24).contains(&x) && (6..18).contains(&y) {
                    for channel in 0..3 {
                        let b = background[channel] as f32 / 255.0;
                        let s = source[channel] as f32 / 255.0;
                        expected[channel] =
                            (255.0 * ((1.0 - alpha) * b + alpha * blend(mode, b, s))).round() as u8;
                    }
                }
                assert!(
                    pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 3),
                    "mode {:?}, alpha {}, redraw {}, pixel ({}, {}): {:?}, expected {:?}",
                    mode,
                    alpha,
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
