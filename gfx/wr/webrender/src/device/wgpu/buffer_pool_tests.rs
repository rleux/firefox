/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn device() -> Rc<Device> {
    validation_logging();
    Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    )
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pool_returns_exclusive_buffers_after_submission_retirement() {
    let device = device();
    let pool = BufferPool::new(&device);
    let mut first = pool.upload(&[17; 8], wgt::BufferUses::COPY_SRC).unwrap();
    assert!(Rc::get_mut(&mut first).is_some());
    assert_eq!(pool.bytes(), 0);
    let first_id = Rc::as_ptr(&first);
    let (mut commands, output) = record_upload(&device, &first);
    commands.submit().unwrap();
    pool.recycle(first);
    let second = pool.upload(&[23; 8], wgt::BufferUses::COPY_SRC).unwrap();
    assert_ne!(Rc::as_ptr(&second), first_id);
    assert_eq!(pool.bytes(), 8);
    commands.wait(None).unwrap();
    assert_eq!(map_upload(&device, &**output, 8), vec![17; 8]);
    let mut reused = pool.upload(&[29; 4], wgt::BufferUses::COPY_SRC).unwrap();
    assert_eq!(Rc::as_ptr(&reused), first_id);
    assert!(Rc::get_mut(&mut reused).is_some());
    assert_eq!(pool.bytes(), 0);
    assert_eq!(reused.binding_size(), 4);
    assert_eq!(&read_upload(&device, &reused)[..4], &[29; 4]);
    assert_eq!(read_upload(&device, &second), vec![23; 8]);
    let weak = Rc::downgrade(&reused);
    pool.recycle(reused);
    let other = pool.upload(&[31; 4], wgt::BufferUses::COPY_SRC).unwrap();
    assert_ne!(Rc::as_ptr(&other), first_id);
    drop(weak);
    let vertex = pool.upload(&[37; 4], wgt::BufferUses::VERTEX).unwrap();
    assert_ne!(Rc::as_ptr(&vertex), first_id);
    assert!(pool
        .upload_with(4, wgt::BufferUses::COPY_SRC, |bytes| {
            bytes[0] = 91;
            Err("writer failed".into())
        })
        .is_err());
    assert_eq!(pool.bytes(), 0);
    let (mut pending, output) = record_upload(&device, &other);
    pending.submit().unwrap();
    pool.recycle(other);
    pool.clear();
    pending.wait(None).unwrap();
    assert_eq!(map_upload(&device, &**output, 4), vec![31; 4]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn reused_short_buffers_zero_minimum_binding_padding() {
    let device = device();
    let pool = BufferPool::new(&device);
    let usage = wgt::BufferUses::STORAGE_READ_ONLY | wgt::BufferUses::COPY_SRC;
    for length in 0..=5 {
        let original = pool.upload(&[0xcc; 8], usage).unwrap();
        let allocation = Rc::as_ptr(&original);
        pool.recycle(original);
        let payload = [29; 5];
        let reused = pool.upload(&payload[..length], usage).unwrap();
        assert_eq!(Rc::as_ptr(&reused), allocation);
        assert_eq!(reused.binding_size(), length.max(4) as u64);
        let mut expected = [0xcc; 8];
        expected[..length].copy_from_slice(&payload[..length]);
        expected[length..length.max(4)].fill(0);
        assert_eq!(read_upload(&device, &reused), expected, "length {}", length);
        pool.recycle(reused);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pool_bounds_returned_storage_and_rejects_foreign_buffers() {
    let device = device();
    let pool = BufferPool::new(&device);
    let mut held = Vec::new();
    for _ in 0..257 {
        let buffer = pool.upload(&[7; 4], wgt::BufferUses::COPY_SRC).unwrap();
        held.push(buffer.clone());
        pool.recycle(buffer);
    }
    assert_eq!(pool.bytes(), 256 * 4);
    pool.recycle(held[0].clone());
    assert_eq!(pool.bytes(), 256 * 4);
    pool.clear();
    assert_eq!(read_upload(&device, &held[0]), vec![7; 4]);
    drop(held);
    let large = pool
        .upload_with(64 << 20, wgt::BufferUses::COPY_SRC, |_| Ok(()))
        .unwrap();
    pool.recycle(large.clone());
    let small = pool.upload(&[9; 4], wgt::BufferUses::COPY_SRC).unwrap();
    pool.recycle(small);
    assert_eq!(pool.bytes(), 64 << 20);
    drop(large);
    let different = pool.upload(&[13; 4], wgt::BufferUses::VERTEX).unwrap();
    pool.recycle(different);
    assert_eq!(pool.bytes(), 4);
    pool.clear();
    let foreign = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    pool.recycle(Buffer::new(&foreign, &[19; 4], wgt::BufferUses::COPY_SRC).unwrap());
    assert_eq!(pool.bytes(), 0);
    let returned = pool.upload(&[23; 4], wgt::BufferUses::COPY_SRC).unwrap();
    device.lost.set(true);
    pool.recycle(returned);
    assert_eq!(pool.bytes(), 0);
    assert!(pool.upload(&[29; 4], wgt::BufferUses::COPY_SRC).is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
