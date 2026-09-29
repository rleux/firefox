/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};

fn device() -> Rc<Device> {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    device
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_pool_reuse_and_discard_are_transactional() {
    let device = device();
    let mut pool = TexturePool::new(&device);
    let format = wgt::TextureFormat::Rgba8Unorm;
    let first = pool.acquire(3, 2, format, false).unwrap();
    let first_id = Rc::as_ptr(&first);
    let commands = upload_queue(&device);
    first
        .upload(&commands, rect(0, 0, 3, 2), &[17; 24], None, 0, None)
        .unwrap();
    commands.submit().unwrap();
    drop(first);
    let second = pool.acquire(3, 2, format, false).unwrap();
    assert_ne!(Rc::as_ptr(&second), first_id);
    commands.wait().unwrap();
    let reused = pool.acquire(3, 2, format, false).unwrap();
    assert_eq!(Rc::as_ptr(&reused), first_id);
    assert!(reused.initialized());
    let previous = reused.current_usage();
    let mut abandoned_submission = Submission::new(&device).unwrap();
    let mut abandoned = abandoned_submission.recording().unwrap();
    reused.invalidate(&mut abandoned).unwrap();
    assert!(!reused.initialized());
    assert_eq!(reused.current_usage(), previous);
    let mut conflict_submission = Submission::new(&device).unwrap();
    let mut conflict = conflict_submission.recording().unwrap();
    assert!(reused.invalidate(&mut conflict).is_err());
    drop(abandoned);
    drop(abandoned_submission);
    assert!(reused.initialized());
    assert_eq!(
        reused.readback(rect(0, 0, 3, 2)).unwrap().wait().unwrap(),
        vec![17; 24]
    );
    let update = upload_queue(&device);
    reused.invalidate(&mut update.recording().unwrap()).unwrap();
    reused
        .upload(&update, rect(1, 0, 1, 1), &[53; 4], None, 0, None)
        .unwrap();
    update.submit().unwrap();
    let mut expected = vec![0; 24];
    expected[4..8].fill(53);
    assert_eq!(
        reused.readback(rect(0, 0, 3, 2)).unwrap().wait().unwrap(),
        expected
    );
    update.wait().unwrap();
    drop(reused);
    for texture in [
        pool.acquire(4, 2, format, false).unwrap(),
        pool.acquire(3, 2, wgt::TextureFormat::Bgra8Unorm, false)
            .unwrap(),
        pool.acquire(3, 2, format, true).unwrap(),
    ] {
        assert_ne!(Rc::as_ptr(&texture), first_id);
    }
    let live = pool.acquire(3, 2, format, false).unwrap();
    let mut readback = live.readback(rect(0, 0, 3, 2)).unwrap();
    pool.clear();
    assert_eq!(pool.bytes(), 0);
    drop(live);
    assert_eq!(readback.wait().unwrap(), expected);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_pool_admits_new_shapes_at_count_limit_without_evicting_retained_textures() {
    let device = device();
    let mut pool = TexturePool::new(&device);
    let queue = upload_queue(&device);
    let format = wgt::TextureFormat::Rgba8Unorm;
    let retained = pool.acquire(1, 1, format, false).unwrap();
    let retained_id = Rc::as_ptr(&retained);
    retained
        .upload(&queue, rect(0, 0, 1, 1), &[17; 4], None, 0, None)
        .unwrap();
    queue.submit().unwrap();
    drop(retained);
    for width in 2..=128 {
        drop(pool.acquire(width, 1, format, false).unwrap());
    }
    assert_eq!(pool.bytes(), 33024);
    let new_shape = pool.acquire(129, 1, format, false).unwrap();
    let weak = Rc::downgrade(&new_shape);
    drop(new_shape);
    assert!(
        weak.upgrade().is_some(),
        "The new shape was not cached at the count limit"
    );
    let reused = pool.acquire(129, 1, format, false).unwrap();
    assert!(Rc::ptr_eq(&reused, &weak.upgrade().unwrap()));
    assert_eq!(pool.bytes(), 33024 - 8 + 129 * 4);

    let other = pool.acquire(1, 1, format, false).unwrap();
    assert_ne!(Rc::as_ptr(&other), retained_id);
    queue.wait().unwrap();
    let retained = pool.acquire(1, 1, format, false).unwrap();
    assert_eq!(Rc::as_ptr(&retained), retained_id);
    assert_eq!(
        retained.readback(rect(0, 0, 1, 1)).unwrap().wait().unwrap(),
        [17; 4]
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_pool_bounds_retained_storage_and_rejects_invalid_requests() {
    let device = device();
    let mut pool = TexturePool::new(&device);
    assert!(pool
        .acquire(0, 1, wgt::TextureFormat::R8Unorm, false)
        .is_err());
    assert!(pool
        .acquire(1, 1, wgt::TextureFormat::Depth32Float, false)
        .is_err());
    let held: Vec<_> = (0..129)
        .map(|_| {
            pool.acquire(1, 1, wgt::TextureFormat::R8Unorm, false)
                .unwrap()
        })
        .collect();
    assert_eq!(pool.bytes(), 128);
    drop(held);
    pool.clear();
    let large = pool
        .acquire(4096, 4096, wgt::TextureFormat::Rgba8Unorm, false)
        .unwrap();
    assert_eq!(pool.bytes(), 64 << 20);
    let small = pool
        .acquire(1, 1, wgt::TextureFormat::R8Unorm, false)
        .unwrap();
    assert_eq!(pool.bytes(), 64 << 20);
    drop(small);
    drop(large);
    let small = pool
        .acquire(1, 1, wgt::TextureFormat::R8Unorm, false)
        .unwrap();
    assert_eq!(pool.bytes(), 1);
    drop(small);
    let depth = pool
        .acquire(2, 3, wgt::TextureFormat::Depth32Float, true)
        .unwrap();
    let depth_id = Rc::as_ptr(&depth);
    assert!(depth.target_view().is_some());
    drop(depth);
    assert_eq!(
        Rc::as_ptr(
            &pool
                .acquire(2, 3, wgt::TextureFormat::Depth32Float, true)
                .unwrap()
        ),
        depth_id
    );
    device.lost.set(true);
    assert!(pool
        .acquire(2, 3, wgt::TextureFormat::Depth32Float, true)
        .is_err());
    pool.clear();
    drop(pool);
    assert_eq!(Rc::strong_count(&device), 1);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
