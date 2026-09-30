/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::BufferPool;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn rectangular_color_clears_clip_translate_and_preserve_pixels() {
    let device = device();
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
        wgt::TextureFormat::R8Unorm,
    ] {
        let target = Texture::new(&device, 3, 2, format, TextureFilter::Nearest, true).unwrap();
        let mut pass = pass(&target);
        pass.origin = DeviceIntPoint::new(-7, 13);
        let full = pass.bounds().unwrap();
        {
            let mut recording = queue.recording().unwrap();
            pass.clear_rect(&mut recording, full, Some([0.0, 0.0, 1.0, 1.0]), None)
                .unwrap();
            pass.clear_rect(
                &mut recording,
                DeviceIntRect::from_origin_and_size(
                    DeviceIntPoint::new(-8, 14),
                    DeviceIntSize::new(3, 3),
                ),
                Some([1.0, 0.0, 0.0, 1.0]),
                None,
            )
            .unwrap();
            pass.clear_rect(&mut recording, full, None, None).unwrap();
        }
        queue.wait().unwrap();
        let (red, blue) = match format {
            wgt::TextureFormat::Rgba8Unorm => (vec![255, 0, 0, 255], vec![0, 0, 255, 255]),
            wgt::TextureFormat::Bgra8Unorm => (vec![0, 0, 255, 255], vec![255, 0, 0, 255]),
            _ => (vec![255], vec![0]),
        };
        let expected: Vec<_> = [&blue, &blue, &blue, &red, &red, &blue]
            .iter()
            .flat_map(|pixel| pixel.iter().copied())
            .collect();
        assert_eq!(pixels(&target), expected);
    }
    assert_eq!(pool.bytes(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

fn depth_probe(
    pass: &DrawPass<'_>,
    queue: &SubmissionQueue,
    quad: &Rc<Buffer>,
    pipeline: &Rc<DrawPipeline>,
) {
    let projection = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    let data = floats(&[-1.0, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
    pass.record_batches(
        &mut queue.recording().unwrap(),
        queue,
        quad,
        None,
        &[DrawBatch {
            pipeline,
            projection: Some(&projection),
            textures: &[],
            buffers: &[],
            instances: &data,
            instance_count: 1,
            scissor: pass.bounds().unwrap(),
        }],
    )
    .unwrap();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn rectangular_clears_select_color_and_depth_independently() {
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
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let quad = quad(&device);
    let shader = crate::device::vulkan::shader::select_draw_shader("ps_clear", &[], false).unwrap();
    let pipeline = DrawPipeline::new(
        &device,
        shader,
        target.format(),
        true,
        RenderState {
            depth_test: Some(crate::device::DepthFunction::Less),
            depth_write: false,
            ..RenderState::default()
        },
    )
    .unwrap();
    let mut pass = pass(&target);
    pass.depth = Some(&depth);
    pass.origin = DeviceIntPoint::new(5, -3);
    pass.depth_range = 0.0..0.5;
    let full = pass.bounds().unwrap();
    let top = DeviceIntRect::from_origin_and_size(pass.origin, DeviceIntSize::new(2, 1));
    {
        let mut recording = queue.recording().unwrap();
        pass.clear_rect(&mut recording, full, Some([0.0, 0.0, 1.0, 1.0]), Some(1.0))
            .unwrap();
        pass.clear_rect(&mut recording, top, Some([1.0, 0.0, 0.0, 1.0]), Some(0.25))
            .unwrap();
    }
    depth_probe(&pass, &queue, &quad, &pipeline);
    queue.wait().unwrap();
    let red_green = [[255, 0, 0, 255].repeat(2), [0, 255, 0, 255].repeat(2)].concat();
    assert_eq!(pixels(&target), red_green);
    pass.clear_rect(&mut queue.recording().unwrap(), top, None, Some(0.75))
        .unwrap();
    queue.wait().unwrap();
    assert_eq!(pixels(&target), red_green);
    depth_probe(&pass, &queue, &quad, &pipeline);
    queue.wait().unwrap();
    assert_eq!(pixels(&target), [0, 255, 0, 255].repeat(4));
    {
        let mut recording = queue.recording().unwrap();
        pass.clear_rect(&mut recording, top, None, Some(0.25))
            .unwrap();
        pass.clear_rect(&mut recording, full, Some([0.0, 0.0, 1.0, 1.0]), None)
            .unwrap();
    }
    depth_probe(&pass, &queue, &quad, &pipeline);
    queue.wait().unwrap();
    assert_eq!(
        pixels(&target),
        [[0, 0, 255, 255].repeat(2), [0, 255, 0, 255].repeat(2)].concat()
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn rectangular_clears_validate_retain_mip_views_and_roll_back() {
    let device = device();
    let texture = Texture::new(
        &device,
        4,
        4,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let view = texture.mip_view(1).unwrap();
    let rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
    {
        let pass = pass(&view);
        let mut recording = queue.recording().unwrap();
        pass.clear_rect(&mut recording, rect, None, None).unwrap();
        pass.clear_rect(
            &mut recording,
            DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(3, 3),
                DeviceIntSize::new(1, 1),
            ),
            Some([1.0; 4]),
            None,
        )
        .unwrap();
        for depth in [-0.5, 1.5, f32::NAN, 0.5] {
            assert!(pass
                .clear_rect(&mut recording, rect, None, Some(depth))
                .is_err());
        }
        assert!(!view.initialized());
        pass.clear_rect(&mut recording, rect, Some([1.0; 4]), None)
            .unwrap();
        assert!(view.initialized());
    }
    queue.discard_recording();
    assert!(!view.initialized());
    {
        let pass = pass(&view);
        pass.clear_rect(
            &mut queue.recording().unwrap(),
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            Some([0.0, 1.0, 0.0, 1.0]),
            None,
        )
        .unwrap();
    }
    let weak = Rc::downgrade(&view);
    drop(view);
    assert!(weak.upgrade().is_some());
    queue.wait().unwrap();
    assert!(weak.upgrade().is_none());
    assert!(!texture.initialized());
    assert!(!texture.mip_view(2).unwrap().initialized());
    assert_eq!(
        pixels(&texture.mip_view(1).unwrap()),
        [0, 255, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(pool.bytes(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
