/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_rotates_gradients_with_rounded_clips() {
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
    let size = DeviceIntSize::new(80, 64);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    for (epoch, angle) in [0.0f32, 0.35, -0.55].iter().copied().enumerate() {
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(80.0, 64.0));
        let root = SpaceAndClipInfo::root_scroll(pipeline);
        builder.push_rect(&CommonItemProperties::new(full, root), full, ColorF::BLACK);
        let spatial_id = builder.push_reference_frame(
            LayoutPoint::new(40.0, 32.0),
            root.spatial_id,
            TransformStyle::Flat,
            PropertyBinding::Value(LayoutTransform::rotation(
                0.0,
                0.0,
                1.0,
                euclid::Angle::radians(angle),
            )),
            ReferenceFrameKind::Transform {
                is_2d_scale_translation: false,
                should_snap: false,
                paired_with_perspective: false,
            },
        );
        builder.push_simple_stacking_context(spatial_id, PrimitiveFlags::IS_BACKFACE_VISIBLE);
        let bounds = LayoutRect::from_origin_and_size(
            LayoutPoint::new(-16.0, -12.0),
            LayoutSize::new(32.0, 24.0),
        );
        let clip = builder.define_clip_rounded_rect(
            spatial_id,
            ComplexClipRegion::new(
                bounds,
                BorderRadius::uniform(6.0),
                LayoutSideOffsets::zero(),
                ClipMode::Clip,
            ),
        );
        let clip_chain_id = builder.define_clip_chain(None, [clip]);
        let (gradient, stops) = builder.create_gradient(
            LayoutVector2D::zero(),
            LayoutVector2D::new(32.0, 0.0),
            vec![
                GradientStop {
                    offset: 0.0,
                    color: ColorF::new(1.0, 0.0, 0.0, 1.0),
                },
                GradientStop {
                    offset: 0.5,
                    color: ColorF::new(0.0, 1.0, 0.0, 1.0),
                },
                GradientStop {
                    offset: 1.0,
                    color: ColorF::new(0.0, 0.0, 1.0, 1.0),
                },
            ],
            ExtendMode::Clamp,
        );
        builder.push_gradient(
            &CommonItemProperties::new(
                bounds,
                SpaceAndClipInfo {
                    spatial_id,
                    clip_chain_id,
                },
            ),
            bounds,
            gradient,
            bounds.size(),
            LayoutSize::zero(),
            &stops,
        );
        builder.pop_stacking_context();
        builder.pop_reference_frame();
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
            assert_eq!(pixels.len(), 80 * 64 * 4);
            let (sin, cos) = angle.sin_cos();
            for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                let x = index % 80;
                let y = index / 80;
                let dx = x as f32 + 0.5 - 40.0;
                let dy = y as f32 + 0.5 - 32.0;
                let local_x = cos * dx + sin * dy;
                let local_y = -sin * dx + cos * dy;
                let qx = local_x.abs() - 10.0;
                let qy = local_y.abs() - 6.0;
                let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - 6.0;
                // Coverage at the curved edge depends on rasterizer antialiasing.
                if distance.abs() < 1.5 {
                    continue;
                }
                let expected = if distance < 0.0 {
                    let t = (local_x + 16.0) / 32.0;
                    let rgb = if t < 0.5 {
                        [1.0 - 2.0 * t, 2.0 * t, 0.0]
                    } else {
                        [0.0, 2.0 - 2.0 * t, 2.0 * t - 1.0]
                    };
                    [
                        (rgb[0] * 255.0).round() as u8,
                        (rgb[1] * 255.0).round() as u8,
                        (rgb[2] * 255.0).round() as u8,
                        255,
                    ]
                } else {
                    [0, 0, 0, 255]
                };
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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_keeps_transformed_quad_interiors_covered() {
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
    let size = DeviceIntSize::new(80, 64);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    for epoch in 0..512 {
        let angle = epoch as f32 * 0.017;
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(80.0, 64.0));
        let root = SpaceAndClipInfo::root_scroll(pipeline);
        builder.push_rect(&CommonItemProperties::new(full, root), full, ColorF::BLACK);
        let spatial_id = builder.push_reference_frame(
            LayoutPoint::new(40.0, 32.0),
            root.spatial_id,
            TransformStyle::Flat,
            PropertyBinding::Value(LayoutTransform::rotation(
                0.0,
                0.0,
                1.0,
                euclid::Angle::radians(angle),
            )),
            ReferenceFrameKind::Transform {
                is_2d_scale_translation: false,
                should_snap: false,
                paired_with_perspective: false,
            },
        );
        builder.push_simple_stacking_context(spatial_id, PrimitiveFlags::IS_BACKFACE_VISIBLE);
        let bounds = LayoutRect::from_origin_and_size(
            LayoutPoint::new(-16.0, -12.0),
            LayoutSize::new(32.0, 24.0),
        );
        builder.push_rect(
            &CommonItemProperties::new(
                bounds,
                SpaceAndClipInfo {
                    spatial_id,
                    clip_chain_id: ClipChainId::INVALID,
                },
            ),
            bounds,
            ColorF::WHITE,
        );
        builder.pop_stacking_context();
        builder.pop_reference_frame();
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        renderer.render(size, 0).unwrap();
        let pixels = renderer
            .device
            .wgpu_test_output()
            .unwrap()
            .readback(DeviceIntRect::from_size(size))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(pixels.len(), 80 * 64 * 4);
        let (sin, cos) = angle.sin_cos();
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            let x = index % 80;
            let y = index / 80;
            let dx = x as f32 + 0.5 - 40.0;
            let dy = y as f32 + 0.5 - 32.0;
            let local_x = cos * dx + sin * dy;
            let local_y = -sin * dx + cos * dy;
            let distance = (local_x.abs() - 16.0).max(local_y.abs() - 12.0);
            if distance > -0.5 {
                continue;
            }
            let expected = [255u8; 4];
            assert!(
                pixel
                    .iter()
                    .zip(expected)
                    .all(|(&a, b)| a.abs_diff(b) <= 64),
                "epoch {}, pixel ({}, {}): {:?}, expected {:?}",
                epoch,
                x,
                y,
                pixel,
                expected
            );
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
