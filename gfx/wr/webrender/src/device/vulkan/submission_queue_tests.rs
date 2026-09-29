/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::{Device, Options};
use crate::device::vulkan::tests::{map_upload, validation_logging, ERRORS};
use std::sync::atomic::Ordering;
use wgpu_hal::CommandEncoder as _;

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

fn upload_copy(
    queue: &SubmissionQueue,
    bytes: &[u8],
) -> (Rc<Buffer>, Rc<Owned<hal::vulkan::Buffer>>) {
    let owner = &queue.pool.owner;
    let mut recording = queue.recording().unwrap();
    let source = queue
        .upload_in_recording(
            &mut recording,
            bytes.len(),
            wgt::BufferUses::COPY_SRC,
            |destination| {
                destination.copy_from_slice(bytes);
                Ok(())
            },
        )
        .unwrap();
    let raw = unsafe {
        owner.open.device.create_buffer(&hal::BufferDescriptor {
            label: Some("WR queued upload readback"),
            size: bytes.len() as u64,
            usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
            memory_flags: hal::MemoryFlags::PREFER_COHERENT,
        })
    }
    .unwrap();
    let target = Rc::new(Owned::new(owner, raw, hal::vulkan::Device::destroy_buffer));
    source
        .transition(&mut recording, wgt::BufferUses::COPY_SRC)
        .unwrap();
    unsafe {
        let encoder = recording.encoder();
        encoder.copy_buffer_to_buffer(
            &source.raw,
            &target,
            std::iter::once(hal::BufferCopy {
                src_offset: 0,
                dst_offset: 0,
                size: std::num::NonZeroU64::new(bytes.len() as u64).unwrap(),
            }),
        );
        encoder.transition_buffers(std::iter::once(hal::BufferBarrier {
            buffer: &**target,
            usage: hal::StateTransition {
                from: wgt::BufferUses::COPY_DST,
                to: wgt::BufferUses::MAP_READ,
            },
        }));
    }
    recording.keep(target.clone());
    (source, target)
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queue_batches_copies_and_reuses_completed_uploads() {
    let device = device();
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    assert_eq!(queue.submit().unwrap(), 0);
    assert_eq!(queue.poll().unwrap(), 0);
    assert!(queue.wait_for(1).is_err());
    let (first, output1) = upload_copy(&queue, &[17; 8]);
    let first_id = Rc::as_ptr(&first);
    let (second, output2) = upload_copy(&queue, &[23; 8]);
    assert_ne!(first_id, Rc::as_ptr(&second));
    drop(first);
    drop(second);
    assert_eq!(pool.bytes(), 0);
    assert_eq!(queue.submit().unwrap(), 1);
    assert_eq!(queue.submit().unwrap(), 1);
    assert!(queue.has_pending_work());
    queue.wait_for(1).unwrap();
    assert!(!queue.has_pending_work());
    assert_eq!(map_upload(&device, &output1, 8), vec![17; 8]);
    assert_eq!(map_upload(&device, &output2, 8), vec![23; 8]);
    assert_eq!(pool.bytes(), 16);
    assert_eq!(queue.state.borrow().ready.len(), 1);
    let (reused, output3) = upload_copy(&queue, &[31; 8]);
    assert_eq!(Rc::as_ptr(&reused), first_id);
    assert!(queue.state.borrow().ready.is_empty());
    assert_eq!(queue.wait().unwrap(), 2);
    assert_eq!(map_upload(&device, &output3, 8), vec![31; 8]);
    drop(reused);
    queue.trim().unwrap();
    assert!(queue.state.borrow().ready.is_empty());
    assert_eq!(pool.bytes(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queue_bounds_pending_submissions_and_waits_by_serial() {
    let device = device();
    let pool = Rc::new(BufferPool::new(&device));
    assert!(SubmissionQueue::new(&pool, 0).is_err());
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    queue.defer_poll.set(true);
    let first = Rc::new(());
    let first_weak = Rc::downgrade(&first);
    queue.recording().unwrap().keep(first);
    assert_eq!(queue.submit().unwrap(), 1);
    let second = Rc::new(());
    let second_weak = Rc::downgrade(&second);
    queue.recording().unwrap().keep(second);
    assert_eq!(queue.submit().unwrap(), 2);
    assert!(first_weak.upgrade().is_some());
    assert!(second_weak.upgrade().is_some());
    drop(queue.recording().unwrap());
    assert!(first_weak.upgrade().is_none());
    assert!(second_weak.upgrade().is_some());
    assert_eq!(queue.state.borrow().completed, 1);
    assert_eq!(queue.state.borrow().pending.len(), 1);
    assert_eq!(queue.submit().unwrap(), 3);
    assert_eq!(queue.state.borrow().pending.len(), 2);
    queue.wait_for(2).unwrap();
    assert!(second_weak.upgrade().is_none());
    assert_eq!(queue.state.borrow().completed, 2);
    assert_eq!(queue.state.borrow().pending.len(), 1);
    queue.wait_for(3).unwrap();
    assert_eq!(queue.poll().unwrap(), 3);
    assert!(queue.wait_for(4).is_err());
    queue.state.borrow_mut().next_serial = u64::MAX;
    assert!(queue.recording().is_err());
    assert!(!device.is_lost());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
