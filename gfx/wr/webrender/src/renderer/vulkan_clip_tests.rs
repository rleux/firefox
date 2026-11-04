/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_rotates_gradients_with_rounded_clips() {
    let (mut renderer, mut api, rx) = renderer();
    let size = DeviceIntSize::new(80, 64);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    for (epoch, angle) in [0.0f32, 0.35, -0.55].iter().copied().enumerate() {
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let full = LayoutRect::from_size(LayoutSize::new(80.0, 64.0));
        let root = SpaceAndClipInfo::root_scroll(pipeline);
        builder.push_rect(&CommonItemProperties::new(full, root), full, ColorF::BLACK);
        let spatial_id = builder.define_reference_frame(
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
            api::AlphaMode::Premultiplied,
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
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        for redraw in 0..2 {
            renderer.render(size, 0).unwrap();
            let pixels = capture(&mut renderer, size);
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
    let (mut renderer, mut api, rx) = renderer();
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
        let spatial_id = builder.define_reference_frame(
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
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        renderer.render(size, 0).unwrap();
        let pixels = capture(&mut renderer, size);
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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_gradient_borders_avoid_source_tasks_and_match_sliced_gradients() {
    let (mut renderer, mut api, rx) = renderer();
    let size = DeviceIntSize::new(300, 300);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let mut epoch = 0;
    for kind in 0..3 {
        for angle in [0.0f32, 0.17] {
            let mut actual: Vec<u8> = Vec::new();
            for reference in [false, true] {
                let mut builder = DisplayListBuilder::new(pipeline);
                builder.begin(60.0);
                let full = LayoutRect::from_size(LayoutSize::new(300.0, 300.0));
                let root = SpaceAndClipInfo::root_scroll(pipeline);
                builder.push_rect(&CommonItemProperties::new(full, root), full, ColorF::BLACK);
                let spatial_id = builder.define_reference_frame(
                    LayoutPoint::new(40.0, 40.0),
                    root.spatial_id,
                    TransformStyle::Flat,
                    PropertyBinding::Value(LayoutTransform::rotation(
                        0.0,
                        0.0,
                        1.0,
                        euclid::Angle::radians(angle),
                    )),
                    ReferenceFrameKind::Transform {
                        is_2d_scale_translation: angle == 0.0,
                        should_snap: false,
                        paired_with_perspective: false,
                    },
                );
                builder
                    .push_simple_stacking_context(spatial_id, PrimitiveFlags::IS_BACKFACE_VISIBLE);
                let bounds = LayoutRect::from_size(LayoutSize::new(200.0, 150.0));
                let clip_chain_id = if angle == 0.0 {
                    ClipChainId::INVALID
                } else {
                    let clip = builder.define_clip_rounded_rect(
                        spatial_id,
                        ComplexClipRegion::new(
                            bounds,
                            BorderRadius::uniform(24.0),
                            LayoutSideOffsets::zero(),
                            ClipMode::Clip,
                        ),
                    );
                    builder.define_clip_chain(None, [clip])
                };
                if reference {
                    // Ordinary gradients in independent spatial frames are the pixel reference.
                    let x_segments: &[([f32; 2], [f32; 2])] = if angle == 0.0 {
                        &[
                            ([0.0, 10.0], [0.0, 30.0]),
                            ([10.0, 70.0], [30.0, 180.0]),
                            ([70.0, 100.0], [180.0, 200.0]),
                        ]
                    } else {
                        &[([0.0, 100.0], [0.0, 200.0])]
                    };
                    let y_segments: &[([f32; 2], [f32; 2])] = if angle == 0.0 {
                        &[
                            ([0.0, 20.0], [0.0, 20.0]),
                            ([20.0, 60.0], [20.0, 90.0]),
                            ([60.0, 100.0], [90.0, 150.0]),
                        ]
                    } else {
                        &[([0.0, 100.0], [0.0, 150.0])]
                    };
                    for &(sy, dy) in y_segments {
                        for &(sx, dx) in x_segments {
                            let segment = LayoutRect::from_size(LayoutSize::new(
                                sx[1] - sx[0],
                                sy[1] - sy[0],
                            ));
                            let segment_spatial_id = builder.define_reference_frame(
                                LayoutPoint::new(dx[0], dy[0]),
                                spatial_id,
                                TransformStyle::Flat,
                                PropertyBinding::Value(LayoutTransform::scale(
                                    (dx[1] - dx[0]) / segment.width(),
                                    (dy[1] - dy[0]) / segment.height(),
                                    1.0,
                                )),
                                ReferenceFrameKind::Transform {
                                    is_2d_scale_translation: true,
                                    should_snap: false,
                                    paired_with_perspective: false,
                                },
                            );
                            let common = CommonItemProperties::new(
                                segment,
                                SpaceAndClipInfo {
                                    spatial_id: segment_spatial_id,
                                    clip_chain_id,
                                },
                            );
                            let (source, stops) = border_gradient_source(
                                &mut builder,
                                kind,
                                LayoutVector2D::new(sx[0], sy[0]),
                                BorderGradientStops::Continuous,
                            );
                            push_border_gradient(&mut builder, &common, segment, source, &stops);
                        }
                    }
                } else {
                    let (source, stops) = border_gradient_source(
                        &mut builder,
                        kind,
                        LayoutVector2D::zero(),
                        BorderGradientStops::Continuous,
                    );
                    builder.push_border(
                        &CommonItemProperties::new(
                            bounds,
                            SpaceAndClipInfo {
                                spatial_id,
                                clip_chain_id,
                            },
                        ),
                        bounds,
                        if angle == 0.0 {
                            LayoutSideOffsets::new(20.0, 20.0, 60.0, 30.0)
                        } else {
                            LayoutSideOffsets::new(30.0, 60.0, 60.0, 20.0)
                        },
                        BorderDetails::NinePatch(NinePatchBorder {
                            source,
                            width: 100,
                            height: 100,
                            slice: DeviceIntSideOffsets::new(20, 30, 40, 10),
                            fill: true,
                            repeat_horizontal: RepeatMode::Stretch,
                            repeat_vertical: RepeatMode::Stretch,
                        }),
                        &stops,
                    );
                }
                builder.pop_stacking_context();
                let mut transaction = Transaction::new();
                transaction.set_root_pipeline(pipeline);
                transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
                transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
                api.send_transaction(document, transaction);
                epoch += 1;
                rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
                renderer.update();
                if !reference && angle == 0.0 {
                    let source_tasks = renderer
                        .active_documents
                        .values()
                        .flat_map(|doc| doc.frame.render_tasks.tasks.iter())
                        .filter(|task| {
                            matches!(&task.kind, crate::render_task::RenderTaskKind::Prim(prim)
                            if prim.pattern == crate::pattern::PatternKind::Gradient)
                        })
                        .count();
                    assert_eq!(
                        source_tasks, 0,
                        "kind {kind}: unexpected gradient source tasks"
                    );
                }
                renderer.render(size, 0).unwrap();
                let pixels = capture(&mut renderer, size);
                if reference {
                    for (index, (a, b)) in actual
                        .chunks_exact(4)
                        .zip(pixels.chunks_exact(4))
                        .enumerate()
                    {
                        assert!(
                            a.iter().zip(b).all(|(&a, &b)| a.abs_diff(b) <= 3),
                            "kind {kind}, angle {angle}, pixel ({}, {}): {a:?}, reference {b:?}",
                            index % 300,
                            index / 300
                        );
                    }
                } else {
                    actual = pixels;
                }
            }
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_discontinuous_gradient_borders_keep_filtered_sources() {
    let (mut renderer, mut api, rx) =
        renderer_with_clear_color(ColorF::new(0.0, 0.0, 0.0, 0.0));
    let size = DeviceIntSize::new(300, 300);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let root = SpaceAndClipInfo::root_scroll(pipeline);
    let mut epoch = 0;
    for kind in 0..3 {
        for stops_kind in [
            BorderGradientStops::HardStop,
            BorderGradientStops::Wrap,
            BorderGradientStops::ConicNonUnitRepeat,
            BorderGradientStops::ConicExtended,
        ] {
            if kind != 2
                && matches!(
                    stops_kind,
                    BorderGradientStops::ConicNonUnitRepeat | BorderGradientStops::ConicExtended
                )
            {
                continue;
            }
            // Render the independent bitmap source once, before applying border slicing.
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.0);
            let source_bounds = LayoutRect::from_size(LayoutSize::new(100.0, 100.0));
            let (source, stops) =
                border_gradient_source(&mut builder, kind, LayoutVector2D::zero(), stops_kind);
            push_border_gradient(
                &mut builder,
                &CommonItemProperties::new(source_bounds, root),
                source_bounds,
                source,
                &stops,
            );
            let mut transaction = Transaction::new();
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
            transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
            api.send_transaction(document, transaction);
            epoch += 1;
            rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
            renderer.update();
            renderer.render(size, 0).unwrap();
            let pixels = capture(&mut renderer, size);
            let source_pixels: Vec<u8> = (0..100usize)
                .flat_map(|y| pixels[y * 1200..y * 1200 + 400].iter().copied())
                .collect();
            assert!(source_pixels.chunks_exact(4).any(|pixel| pixel[3] < 255));
            assert!(source_pixels
                .chunks_exact(4)
                .all(|pixel| pixel[..3].iter().all(|channel| *channel <= pixel[3])));
            let image = api.generate_image_key();
            let mut transaction = Transaction::new();
            transaction.add_image(
                image,
                ImageDescriptor::new(100, 100, ImageFormat::RGBA8, ImageDescriptorFlags::empty()),
                ImageData::new(source_pixels),
                None,
            );
            api.send_transaction(document, transaction);

            for angle in [0.0f32, 0.17] {
                let mut actual: Vec<u8> = Vec::new();
                for reference in [false, true] {
                    let mut builder = DisplayListBuilder::new(pipeline);
                    builder.begin(60.0);
                    let full = LayoutRect::from_size(LayoutSize::new(300.0, 300.0));
                    builder.push_rect(&CommonItemProperties::new(full, root), full, ColorF::BLACK);
                    let spatial_id = builder.define_reference_frame(
                        LayoutPoint::new(40.0, 40.0),
                        root.spatial_id,
                        TransformStyle::Flat,
                        PropertyBinding::Value(LayoutTransform::rotation(
                            0.0,
                            0.0,
                            1.0,
                            euclid::Angle::radians(angle),
                        )),
                        ReferenceFrameKind::Transform {
                            is_2d_scale_translation: angle == 0.0,
                            should_snap: false,
                            paired_with_perspective: false,
                        },
                    );
                    builder.push_simple_stacking_context(
                        spatial_id,
                        PrimitiveFlags::IS_BACKFACE_VISIBLE,
                    );
                    let bounds = LayoutRect::from_size(LayoutSize::new(200.0, 150.0));
                    let clip_chain_id = if angle == 0.0 {
                        ClipChainId::INVALID
                    } else {
                        let clip = builder.define_clip_rounded_rect(
                            spatial_id,
                            ComplexClipRegion::new(
                                bounds,
                                BorderRadius::uniform(24.0),
                                LayoutSideOffsets::zero(),
                                ClipMode::Clip,
                            ),
                        );
                        builder.define_clip_chain(None, [clip])
                    };
                    let (source, stops) = if reference {
                        (
                            NinePatchBorderSource::Image(image, ImageRendering::Auto),
                            Vec::<GradientStop>::new(),
                        )
                    } else {
                        border_gradient_source(
                            &mut builder,
                            kind,
                            LayoutVector2D::zero(),
                            stops_kind,
                        )
                    };
                    builder.push_border(
                        &CommonItemProperties::new(
                            bounds,
                            SpaceAndClipInfo {
                                spatial_id,
                                clip_chain_id,
                            },
                        ),
                        bounds,
                        if angle == 0.0 {
                            LayoutSideOffsets::new(20.0, 20.0, 60.0, 30.0)
                        } else {
                            LayoutSideOffsets::new(30.0, 60.0, 60.0, 20.0)
                        },
                        BorderDetails::NinePatch(NinePatchBorder {
                            source,
                            width: 100,
                            height: 100,
                            slice: DeviceIntSideOffsets::new(20, 30, 40, 10),
                            fill: true,
                            repeat_horizontal: RepeatMode::Stretch,
                            repeat_vertical: RepeatMode::Stretch,
                        }),
                        &stops,
                    );
                    builder.pop_stacking_context();
                    let mut transaction = Transaction::new();
                    transaction.set_root_pipeline(pipeline);
                    transaction.set_display_list(
                        Epoch(epoch),
                        api.get_namespace_id(),
                        builder.end(),
                    );
                    transaction.generate_frame(
                        epoch as u64 + 1,
                        true,
                        false,
                        RenderReasons::TESTING,
                    );
                    api.send_transaction(document, transaction);
                    epoch += 1;
                    rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
                    renderer.update();
                    let source_tasks = renderer.active_documents.values().flat_map(|doc| doc.frame.render_tasks.tasks.iter())
                        .filter(|task| matches!(&task.kind, crate::render_task::RenderTaskKind::Prim(prim) if prim.pattern == crate::pattern::PatternKind::Gradient)).count();
                    assert_eq!(
                        source_tasks,
                        if reference { 0 } else { 9 },
                        "kind {kind}, {stops_kind:?}, angle {angle}"
                    );
                    renderer.render(size, 0).unwrap();
                    let pixels = capture(&mut renderer, size);
                    if reference {
                        for (index, (a, b)) in actual
                            .chunks_exact(4)
                            .zip(pixels.chunks_exact(4))
                            .enumerate()
                        {
                            assert!(a.iter().zip(b).all(|(&a, &b)| a.abs_diff(b) <= 3),
                                "kind {kind}, {stops_kind:?}, angle {angle}, pixel ({}, {}): {a:?}, bitmap {b:?}", index % 300, index / 300);
                        }
                    } else {
                        actual = pixels;
                    }
                }
            }
            let mut transaction = Transaction::new();
            transaction.delete_image(image);
            api.send_transaction(document, transaction);
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[derive(Clone, Copy, Debug)]
enum BorderGradientStops {
    Continuous,
    HardStop,
    Wrap,
    ConicNonUnitRepeat,
    ConicExtended,
}

fn border_gradient_source(
    builder: &mut DisplayListBuilder,
    kind: usize,
    origin: LayoutVector2D,
    stops_kind: BorderGradientStops,
) -> (NinePatchBorderSource, Vec<GradientStop>) {
    let red = ColorF::new(1.0, 0.0, 0.0, 1.0);
    let blue = ColorF::new(0.0, 0.0, 1.0, 0.25);
    let stops = match stops_kind {
        BorderGradientStops::ConicNonUnitRepeat => vec![(0.2, red), (0.3, blue), (0.8, red)],
        BorderGradientStops::ConicExtended => vec![(-1.0, red), (0.0, blue), (2.0, red)],
        _ => {
            let mut stops = vec![(0.0, red), (0.35, blue), (0.65, blue)];
            if matches!(stops_kind, BorderGradientStops::HardStop) {
                stops.push((0.65, ColorF::new(1.0, 1.0, 0.0, 1.0)));
            }
            let end = if matches!(stops_kind, BorderGradientStops::Continuous) {
                red
            } else {
                ColorF::new(0.0, 1.0, 0.0, 1.0)
            };
            stops.push((1.0, end));
            stops
        }
    };
    let stops = stops
        .into_iter()
        .map(|(offset, color)| GradientStop { offset, color })
        .collect();
    let extend_mode = if matches!(stops_kind, BorderGradientStops::ConicNonUnitRepeat)
        || (kind != 2 && matches!(stops_kind, BorderGradientStops::Wrap))
    {
        ExtendMode::Repeat
    } else {
        ExtendMode::Clamp
    };
    let center = LayoutVector2D::new(45.0, 55.0) - origin;
    match kind {
        0 => {
            let (gradient, stops) = builder.create_gradient(
                -origin,
                LayoutVector2D::new(100.0, 100.0) - origin,
                stops,
                extend_mode,
                AlphaMode::Unpremultiplied,
            );
            (NinePatchBorderSource::Gradient(gradient), stops)
        }
        1 => {
            let (gradient, stops) = builder.create_radial_gradient(
                center,
                LayoutSize::new(65.0, 40.0),
                stops,
                extend_mode,
                AlphaMode::Unpremultiplied,
            );
            (NinePatchBorderSource::RadialGradient(gradient), stops)
        }
        _ => {
            let (gradient, stops) = builder.create_conic_gradient(
                center,
                0.7,
                stops,
                extend_mode,
                AlphaMode::Unpremultiplied,
            );
            (NinePatchBorderSource::ConicGradient(gradient), stops)
        }
    }
}

fn push_border_gradient(
    builder: &mut DisplayListBuilder,
    common: &CommonItemProperties,
    bounds: LayoutRect,
    source: NinePatchBorderSource,
    stops: &[GradientStop],
) {
    match source {
        NinePatchBorderSource::Gradient(gradient) => builder.push_gradient(
            common,
            bounds,
            gradient,
            bounds.size(),
            LayoutSize::zero(),
            stops,
        ),
        NinePatchBorderSource::RadialGradient(gradient) => builder.push_radial_gradient(
            common,
            bounds,
            gradient,
            bounds.size(),
            LayoutSize::zero(),
            stops,
        ),
        NinePatchBorderSource::ConicGradient(gradient) => builder.push_conic_gradient(
            common,
            bounds,
            gradient,
            bounds.size(),
            LayoutSize::zero(),
            stops,
        ),
        _ => unreachable!(),
    }
}
