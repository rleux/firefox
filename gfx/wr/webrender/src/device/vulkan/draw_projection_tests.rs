/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::BufferPool;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn logical_target_origins_align_projection_and_scissors() {
    let device = device();
    let quad = quad(&device);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let pipeline = draw(&device, None).bindings.pipeline.clone();
    let transform = Transform3D::ortho(0.0, 2.0, 0.0, 2.0, -1.0, 1.0);
    for origin in [DeviceIntPoint::new(37, 53), DeviceIntPoint::new(-7, -11)] {
        let target = target(&device);
        let mut pass = pass(&target);
        pass.origin = origin;
        let projection = pass.projection(&transform);
        let x = origin.x as f32;
        let y = origin.y as f32;
        let red = floats(&[x, y, x + 2.0, y + 2.0, 1.0, 0.0, 0.0, 1.0]);
        let green = floats(&[x, y, x + 2.0, y + 1.0, 0.0, 1.0, 0.0, 1.0]);
        let batches = [
            DrawBatch {
                pipeline: &pipeline,
                projection: Some(&projection),
                textures: &[],
                buffers: &[],
                instances: &red,
                instance_count: 1,
                scissor: DeviceIntRect::from_origin_and_size(
                    DeviceIntPoint::new(origin.x - 1, origin.y + 1),
                    DeviceIntSize::new(2, 3),
                ),
            },
            DrawBatch {
                pipeline: &pipeline,
                projection: Some(&projection),
                textures: &[],
                buffers: &[],
                instances: &green,
                instance_count: 1,
                scissor: DeviceIntRect::from_origin_and_size(origin, DeviceIntSize::new(2, 2)),
            },
        ];
        pass.record_batches(
            &mut queue.recording().unwrap(),
            &queue,
            &quad,
            None,
            &batches,
        )
        .unwrap();
        queue.wait().unwrap();
        assert_eq!(
            pixels(&target),
            [0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255, 0, 0, 0, 0]
        );
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn gl_projection_preserves_image_orientation_and_maps_clip_depth() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let texture = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let colors = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ];
    texture
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
            &colors,
            None,
            0,
            None,
        )
        .unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let shader =
        crate::device::vulkan::shader::select_draw_shader("cs_scale", &["TEXTURE_2D"], false)
            .unwrap();
    let pipeline = DrawPipeline::new(
        &device,
        shader,
        target.format(),
        true,
        RenderState {
            depth_test: Some(crate::device::DepthFunction::Less),
            ..RenderState::default()
        },
    )
    .unwrap();
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
    pass.origin = DeviceIntPoint::new(11, 19);
    pass.depth = Some(&depth);
    pass.clear_depth = Some(0.5);
    let mut transform = Transform3D::ortho(0.0, 2.0, 0.0, 2.0, -1.0, 1.0);
    let textures = [(texture, TextureFilter::Nearest)];
    let data = floats(&[11.0, 19.0, 13.0, 21.0, 0.0, 0.0, 1.0, 1.0, 0.0]);
    for (z, visible) in [(-0.5, true), (0.5, false)] {
        transform.m43 = z;
        let projection = pass.projection(&transform);
        pass.clear_color = Some(wgt::Color::TRANSPARENT);
        let batch = DrawBatch {
            pipeline: &pipeline,
            projection: Some(&projection),
            textures: &textures,
            buffers: &[],
            instances: &data,
            instance_count: 1,
            scissor: DeviceIntRect::from_origin_and_size(pass.origin, DeviceIntSize::new(2, 2)),
        };
        pass.record_batches(
            &mut queue.recording().unwrap(),
            &queue,
            &quad,
            Some(&samplers),
            &[batch],
        )
        .unwrap();
        queue.wait().unwrap();
        assert_eq!(pixels(&target), if visible { colors } else { [0; 16] });
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn target_projection_preserves_homogeneous_coordinates_and_checks_bounds() {
    let device = device();
    let target = target(&device);
    let mut pass = pass(&target);
    pass.origin = DeviceIntPoint::new(3, -5);
    let transform = Transform3D::new(
        1.0, 2.0, 3.0, 0.25, 4.0, 5.0, 6.0, -0.5, 7.0, 8.0, 9.0, 0.75, 10.0, 11.0, 12.0, 1.0,
    );
    let converted = pass.projection(&transform);
    let apply = |matrix: [f32; 16], input: [f32; 4]| -> [f32; 4] {
        std::array::from_fn(|row| {
            (0..4)
                .map(|column| matrix[column * 4 + row] * input[column])
                .sum()
        })
    };
    for input in [[0.0, 0.0, 0.0, 1.0], [2.0, -1.0, 3.0, 1.0]] {
        let gl = apply(transform.to_array(), input);
        assert_eq!(
            apply(converted, input),
            [
                gl[0] - 3.0 * gl[3],
                -gl[1] - 5.0 * gl[3],
                (gl[2] + gl[3]) * 0.5,
                gl[3]
            ]
        );
    }
    let quad = quad(&device);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    for origin in [
        DeviceIntPoint::new(i32::MAX, 0),
        DeviceIntPoint::new(0, i32::MAX),
    ] {
        pass.origin = origin;
        let mut recording = queue.recording().unwrap();
        assert!(pass
            .record(&mut recording, &quad, &[])
            .unwrap_err()
            .contains("overflow"));
        assert!(pass
            .record_batches(&mut recording, &queue, &quad, None, &[])
            .unwrap_err()
            .contains("overflow"));
    }
    assert!(!target.initialized());
    assert_eq!(pool.bytes(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
