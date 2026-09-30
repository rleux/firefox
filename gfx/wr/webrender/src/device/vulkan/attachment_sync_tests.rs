/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{BlendMode, DepthFunction};

pub(in crate::device::vulkan) fn record_overlapping_passes(
    pass: &DrawPass<'_>,
    commands: &mut Recording<'_>,
) {
    let owner = &pass.target.raw.owner;
    let quad = quad(owner);
    quad.transition(commands, wgt::BufferUses::VERTEX).unwrap();
    let shader = crate::device::vulkan::shader::select_draw_shader("ps_clear", &[], false).unwrap();
    let mut draws = Vec::new();
    for (z, color, blend) in [
        (0.3, [1.0, 0.0, 0.0, 1.0], BlendMode::None),
        (0.2, [0.0, 0.5, 0.0, 0.5], BlendMode::PremultipliedAlpha),
        (0.4, [0.0, 0.0, 1.0, 1.0], BlendMode::None),
    ] {
        let pipeline = DrawPipeline::new(
            owner,
            shader,
            pass.target.format(),
            true,
            RenderState {
                blend_mode: blend,
                depth_test: Some(DepthFunction::Less),
                depth_write: true,
                ..Default::default()
            },
        )
        .unwrap();
        let projection = Buffer::new(
            owner,
            &floats(&[
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, z, 1.0,
            ]),
            wgt::BufferUses::UNIFORM,
        )
        .unwrap();
        let instances = Buffer::new(
            owner,
            &floats(&[-1.0, -1.0, 1.0, 1.0, color[0], color[1], color[2], color[3]]),
            wgt::BufferUses::VERTEX,
        )
        .unwrap();
        // Prepare buffers before any attachment access so they cannot mask a missing dependency.
        projection
            .transition(commands, wgt::BufferUses::UNIFORM)
            .unwrap();
        instances
            .transition(commands, wgt::BufferUses::VERTEX)
            .unwrap();
        draws.push((
            z,
            Draw {
                bindings: DrawBindings::new(
                    &pipeline,
                    Some(projection),
                    Vec::new(),
                    Vec::new(),
                    None,
                )
                .unwrap(),
                instances,
                instance_offset: 0,
                instance_count: 1,
                scissor: pass.bounds().unwrap(),
            },
        ));
    }
    pass.clear_rect(
        commands,
        pass.bounds().unwrap(),
        Some([0.0, 0.0, 1.0, 1.0]),
        Some(1.0),
    )
    .unwrap();
    for (z, draw) in draws {
        DrawPass {
            target: pass.target,
            origin: pass.origin,
            depth: pass.depth,
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..z,
        }
        .record(commands, &quad, &[draw])
        .unwrap();
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn consecutive_attachment_passes_order_color_and_depth_writes() {
    let device = device();
    let target = target(&device);
    let depth = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let mut pass = pass(&target);
    pass.depth = Some(&depth);
    let mut submission = Submission::new(&device).unwrap();
    record_overlapping_passes(&pass, &mut submission.recording().unwrap());
    submission.submit().unwrap();
    assert!(submission.wait(None).unwrap());
    for pixel in pixels(&target).chunks_exact(4) {
        assert!(
            pixel
                .iter()
                .zip([128u8, 128, 0, 255])
                .all(|(&a, b)| a.abs_diff(b) <= 1),
            "{:?}",
            pixel
        );
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
