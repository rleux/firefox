/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::BufferPool;

const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

fn batch<'a>(pipeline: &'a Rc<DrawPipeline>, instances: &'a [u8]) -> DrawBatch<'a> {
    DrawBatch {
        pipeline,
        projection: Some(&IDENTITY),
        textures: &[],
        buffers: &[],
        instances,
        instance_count: 1,
        scissor: DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queued_draw_batches_snapshot_cpu_data_and_recycle_uploads() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let pipeline = draw(&device, None).bindings.pipeline.clone();
    let mut red = floats(&[-1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0]);
    let mut green = floats(&[-1.0, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
    let mut left = IDENTITY;
    left[0] = 0.5;
    left[12] = -0.5;
    let mut right = left;
    right[12] = 0.5;
    {
        let mut batches = [batch(&pipeline, &red), batch(&pipeline, &green)];
        batches[0].projection = Some(&left);
        batches[1].projection = Some(&right);
        pass(&target)
            .record_batches(
                &mut queue.recording().unwrap(),
                &queue,
                &quad,
                None,
                &batches,
            )
            .unwrap();
    }
    red.fill(0);
    green.fill(0);
    left.fill(0.0);
    right.fill(0.0);
    assert_eq!(pool.bytes(), 0);
    assert!(!queue.has_pending_work());
    let serial = queue.submit().unwrap();
    assert_eq!(serial, 1);
    assert_eq!(pool.bytes(), 0);
    queue.wait_for(serial).unwrap();
    assert_eq!(pool.bytes(), 65536 + 64);
    assert_eq!(pixels(&target), [255, 0, 0, 255, 0, 255, 0, 255].repeat(2));

    let blue_and_yellow = floats(&[
        -1.0, -1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, -1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 1.0,
    ]);
    let mut two_instances = batch(&pipeline, &blue_and_yellow);
    two_instances.instance_count = 2;
    pass(&target)
        .record_batches(
            &mut queue.recording().unwrap(),
            &queue,
            &quad,
            None,
            &[two_instances],
        )
        .unwrap();
    queue.wait().unwrap();
    assert_eq!(pool.bytes(), 65536 + 64);
    assert_eq!(
        pixels(&target),
        [0, 0, 255, 255, 255, 255, 0, 255].repeat(2)
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queued_draw_batches_validate_input_and_recycle_abandoned_uploads() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let pipeline = draw(&device, None).bindings.pipeline.clone();
    for length in [0, 31, 33, 64] {
        let bytes = vec![0; length];
        let error = pass(&target)
            .record_batches(
                &mut queue.recording().unwrap(),
                &queue,
                &quad,
                None,
                &[batch(&pipeline, &bytes)],
            )
            .unwrap_err();
        assert!(error.contains("instance count"), "{}", error);
        assert!(!target.initialized());
        assert_eq!(pool.bytes(), 0);
    }
    let red = floats(&[-1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0]);
    let mut missing = batch(&pipeline, &red);
    missing.projection = None;
    assert!(pass(&target)
        .record_batches(
            &mut queue.recording().unwrap(),
            &queue,
            &quad,
            None,
            &[missing],
        )
        .unwrap_err()
        .contains("projection"));
    pass(&target)
        .record_batches(
            &mut queue.recording().unwrap(),
            &queue,
            &quad,
            None,
            &[batch(&pipeline, &red)],
        )
        .unwrap();
    assert!(target.initialized());
    queue.discard_recording();
    assert!(!target.initialized());
    assert_eq!(pool.bytes(), 65536 + 32);
    let mut skipped = [batch(&pipeline, &[]), batch(&pipeline, &[])];
    skipped[0].instance_count = 0;
    skipped[1].scissor =
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(2, 2), DeviceIntSize::new(1, 1));
    pass(&target)
        .record_batches(
            &mut queue.recording().unwrap(),
            &queue,
            &quad,
            None,
            &skipped,
        )
        .unwrap();
    assert_eq!(pool.bytes(), 65536 + 32);
    queue.wait().unwrap();
    assert_eq!(pool.bytes(), 65536 + 32);
    assert_eq!(pixels(&target), [0; 16]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queued_draw_batches_pack_mixed_strides_and_sample_queued_uploads() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let texture = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    texture
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[0, 255, 0, 255],
            None,
            0,
            None,
        )
        .unwrap();
    let clear = draw(&device, None).bindings.pipeline.clone();
    let scale = draw(&device, Some(texture.clone()))
        .bindings
        .pipeline
        .clone();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let weak_texture = Rc::downgrade(&texture);
    let weak_samplers = Rc::downgrade(&samplers);
    let red = floats(&[-1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0]);
    let scaled = floats(&[0.0, -1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0]);
    {
        let textures = [(texture.clone(), TextureFilter::Nearest)];
        let mut batches = [batch(&clear, &red), batch(&scale, &scaled)];
        batches[1].textures = &textures;
        pass(&target)
            .record_batches(
                &mut queue.recording().unwrap(),
                &queue,
                &quad,
                Some(&samplers),
                &batches,
            )
            .unwrap();
    }
    drop(texture);
    drop(samplers);
    assert!(weak_texture.upgrade().is_some());
    assert!(weak_samplers.upgrade().is_some());
    assert!(!queue.has_pending_work());
    assert_eq!(queue.wait().unwrap(), 1);
    assert!(weak_texture.upgrade().is_none());
    assert!(weak_samplers.upgrade().is_none());
    assert_eq!(pixels(&target), [255, 0, 0, 255, 0, 255, 0, 255].repeat(2));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
