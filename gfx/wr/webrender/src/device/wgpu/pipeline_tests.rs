/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{BlendMode, DepthFunction, RenderState};

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn generated_draw_shaders_create_pipelines() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let mut count = 0;
    for artifact in shaders::SHADERS {
        let features: Vec<_> = artifact
            .features
            .split(',')
            .filter(|f| !f.is_empty())
            .collect();
        let shader = super::super::shader::select_draw_shader(
            artifact.name,
            &features,
            artifact.buffer_tables,
        )
        .unwrap();
        let dual_source = shader.features.contains("DUAL_SOURCE_BLENDING");
        if dual_source
            && !device
                .features()
                .contains(wgt::Features::DUAL_SOURCE_BLENDING)
        {
            continue;
        }
        let format = if shader.name == "ps_quad_mask" || shader.features == "ALPHA_TARGET" {
            wgt::TextureFormat::R8Unorm
        } else {
            wgt::TextureFormat::Rgba8Unorm
        };
        let state = RenderState {
            blend_mode: if dual_source {
                BlendMode::SubpixelDualSource
            } else {
                BlendMode::None
            },
            ..RenderState::default()
        };
        DrawPipeline::new(
            &device,
            shader,
            format,
            false,
            state,
        )
        .unwrap_or_else(|error| panic!("{} {}: {error}", shader.name, shader.features));
        count += 1;
    }
    eprintln!("Created {count} draw pipelines");
    assert!(count > 1);
    let shader = shaders::SHADERS
        .iter()
        .find(|s| s.name == "ps_clear")
        .unwrap();
    for mode in [BlendMode::SubpixelDualSource, BlendMode::ShowOverdraw] {
        assert!(DrawPipeline::new(
            &device,
            shader,
            wgt::TextureFormat::Rgba8Unorm,
            false,
            RenderState {
                blend_mode: mode,
                ..RenderState::default()
            },
        )
        .is_err());
    }
    assert!(DrawPipeline::new(
        &device,
        shader,
        wgt::TextureFormat::Depth32Float,
        false,
        RenderState::default(),
    )
    .is_err());
    device.lost.set(true);
    assert!(DrawPipeline::new(
        &device,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn render_state_controls_blending_color_writes_and_depth() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let red = [1.0, 0.0, 0.0, 1.0];
    let green = [0.0, 1.0, 0.0, 1.0];
    let half_red = [0.5, 0.0, 0.0, 0.5];
    let ordinary = RenderState::default();
    let less = RenderState {
        depth_test: Some(DepthFunction::Less),
        ..ordinary
    };
    let less_write = RenderState {
        depth_write: true,
        ..less
    };
    let mut cases = vec![
        (
            vec![(
                RenderState {
                    color_write: false,
                    ..ordinary
                },
                red,
            )],
            None,
            [0, 0, 255, 255],
        ),
        (vec![(less, red)], Some(0.0), [0, 0, 255, 255]),
        (
            vec![(
                RenderState {
                    depth_test: Some(DepthFunction::Always),
                    ..ordinary
                },
                red,
            )],
            Some(0.0),
            [255, 0, 0, 255],
        ),
        (
            vec![(less_write, red), (less, green)],
            Some(1.0),
            [255, 0, 0, 255],
        ),
        (
            vec![(less, red), (less, green)],
            Some(1.0),
            [0, 255, 0, 255],
        ),
        (
            vec![
                (
                    RenderState {
                        depth_write: true,
                        ..ordinary
                    },
                    red,
                ),
                (less, green),
            ],
            Some(1.0),
            [0, 255, 0, 255],
        ),
        (
            vec![
                (less_write, red),
                (
                    RenderState {
                        depth_test: Some(DepthFunction::LessEqual),
                        ..ordinary
                    },
                    green,
                ),
            ],
            Some(1.0),
            [0, 255, 0, 255],
        ),
    ];
    for (mode, color, expected) in [
        (BlendMode::PremultipliedAlpha, half_red, [128, 0, 128, 255]),
        (BlendMode::Alpha, [1.0, 0.0, 0.0, 0.5], [128, 0, 128, 255]),
        (BlendMode::Multiply, [0.5; 4], [0, 0, 128, 128]),
        (BlendMode::PremultipliedDestOut, half_red, [0, 0, 128, 128]),
        (BlendMode::Screen, [0.5, 0.0, 0.5, 0.5], [128, 0, 255, 255]),
        (
            BlendMode::Exclusion,
            [0.5, 0.0, 0.5, 0.5],
            [128, 0, 128, 255],
        ),
        (BlendMode::PlusLighter, half_red, [128, 0, 255, 255]),
    ] {
        cases.push((
            vec![(
                RenderState {
                    blend_mode: mode,
                    ..ordinary
                },
                color,
            )],
            None,
            expected,
        ));
    }
    for (draws, depth_clear, expected) in cases {
        let pixels = super::shader::draw_clear(&device, &draws, depth_clear);
        for (actual, expected) in pixels.iter().zip(expected.repeat(4)) {
            assert!(
                actual.abs_diff(expected) <= 1,
                "{:?}, depth {:?}: {:?}",
                draws,
                depth_clear,
                pixels
            );
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pipeline_variants_share_prepared_shader_modules() {
    validation_logging();
    let device = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let shader = super::super::shader::select_draw_shader("ps_clear", &[], false).unwrap();
    let first = DrawPipeline::new(&device, shader, wgt::TextureFormat::Rgba8Unorm, false, RenderState::default()).unwrap();
    let second = DrawPipeline::new(&device, shader, wgt::TextureFormat::Bgra8Unorm, false,
        RenderState { blend_mode: BlendMode::PremultipliedAlpha, ..Default::default() }).unwrap();
    assert!(Rc::ptr_eq(&first._prepared, &second._prepared));
    let weak = Rc::downgrade(&first._prepared);
    drop(first);
    drop(second);
    assert!(weak.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
