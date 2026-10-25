use super::*;
use crate::external_sync::{self as sync, SemaphoreValue, SubmissionSync};
use std::os::fd::{AsFd, FromRawFd};

fn graphics(
    work: &Resources,
    fence: usize,
    synchronization: &SubmissionSync,
) -> Result<Box<dyn SharedSubmissionReceipt>, crate::shared::SharedSubmitError> {
    unsafe {
        work.context.submit_graphics(
            &*work.graphics.queue,
            &[],
            &[],
            (&*work.fences[fence], 1),
            synchronization,
        )
    }
}

fn compute(
    work: &Resources,
    fence: usize,
    synchronization: &SubmissionSync,
) -> Result<Box<dyn SharedSubmissionReceipt>, crate::shared::SharedSubmitError> {
    unsafe {
        work.context.submit_ordered(
            &*work.compute.queue,
            &[SharedSubmitSegment {
                lane: Lane::Compute,
                queue: &*work.compute.queue,
                command_buffers: &[],
                synchronization: Some(synchronization),
            }],
            (&*work.fences[fence], 1),
        )
    }
}

#[test]
#[ignore = "Requires Vulkan semaphore FD support and Khronos validation"]
fn scoped_opaque_fd_sync_is_not_stolen_and_receipt_retains_import() {
    let mut work = Work::new();
    let gate = work.gate_token();
    let unrelated_fence = work.fence();
    let producer_fence = work.fence();
    let consumer_fence = work.fence();
    let producer = sync::create_timeline(&*work.compute.device, true).unwrap();
    let exported = sync::export_timeline(&*producer).unwrap();
    let imported = sync::import_timeline(&*work.graphics.device, &exported).unwrap();
    let imported_weak = Arc::downgrade(&imported);
    let release = sync::create_timeline(&*work.graphics.device, false).unwrap();
    let consumer_sync = SubmissionSync {
        waits: vec![SemaphoreValue {
            semaphore: imported,
            value: Some(1),
        }],
        signals: vec![SemaphoreValue {
            semaphore: release.clone(),
            value: Some(1),
        }],
    };
    let unrelated = graphics(&work, unrelated_fence, &SubmissionSync::default());
    let unrelated = work.retain(unrelated);
    let unrelated_done = work.receipts.first().map(|receipt| receipt.wait(None));
    let before = sync::current_value(&*producer);
    let release_before = sync::current_value(&*release);
    work.armed_gates.borrow_mut().push(gate);
    let producer_sync = SubmissionSync {
        waits: vec![SemaphoreValue {
            semaphore: work.gates[gate].clone(),
            value: Some(1),
        }],
        signals: vec![SemaphoreValue {
            semaphore: producer.clone(),
            value: Some(1),
        }],
    };
    let submitted_producer = compute(&work, producer_fence, &producer_sync);
    let submitted_producer = work.retain(submitted_producer);
    let submitted_consumer = graphics(&work, consumer_fence, &consumer_sync);
    let submitted_consumer = work.retain(submitted_consumer);
    drop(consumer_sync);
    let retained = imported_weak.upgrade().is_some();
    let consumer_pending = unsafe {
        work.graphics.device.wait(
            &*work.fences[consumer_fence],
            1,
            Some(std::time::Duration::ZERO),
        )
    };
    let release_pending = sync::current_value(&*release);
    work.retire().unwrap();
    let after = sync::current_value(&*producer);
    let release_after = sync::current_value(&*release);
    work.receipts.clear();
    let released = imported_weak.upgrade().is_none();
    assert!(unrelated.is_ok(), "{unrelated:?}");
    assert_eq!(unrelated_done, Some(Ok(true)));
    assert!(matches!(before, Ok(0)), "{before:?}");
    assert!(matches!(release_before, Ok(0)), "{release_before:?}");
    assert!(matches!(release_pending, Ok(0)), "{release_pending:?}");
    assert!(matches!(release_after, Ok(1)), "{release_after:?}");
    assert!(submitted_producer.is_ok(), "{submitted_producer:?}");
    assert!(submitted_consumer.is_ok(), "{submitted_consumer:?}");
    assert_eq!(consumer_pending, Ok(false));
    assert!(retained && released);
    assert!(matches!(after, Ok(1)), "{after:?}");
    drop((producer_sync, producer, release, exported, work));
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan SYNC_FD support and Khronos validation"]
fn scoped_sync_file_wait_is_one_shot_and_retained_until_retirement() {
    let mut work = Work::new();
    let gate = work.gate_token();
    let consumer_fence = work.fence();
    let repeated_fence = work.fence();
    let device = work
        .compute
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let mut export = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let raw = unsafe {
        device.shared.raw.create_semaphore(
            &vk::SemaphoreCreateInfo::default().push_next(&mut export),
            None,
        )
    }
    .unwrap();
    work.native_semaphores.push(raw);
    work.armed_gates.borrow_mut().push(gate);
    let queue = work
        .compute
        .queue
        .as_any()
        .downcast_ref::<crate::vulkan::Queue>()
        .unwrap();
    let native_result = {
        let _host = queue.device.shared_queue_group.as_ref().unwrap().host[1].lock();
        unsafe {
            queue.submit_impl(
                &[],
                &[],
                None,
                &[crate::vulkan::semaphore_list::SemaphoreType::Timeline(
                    work.gate(gate),
                    1,
                )],
                &[crate::vulkan::semaphore_list::SemaphoreType::Binary(raw)],
                &[],
            )
        }
    };
    if native_result.is_err() {
        work.retire().unwrap();
        panic!("Native sync-file producer failed: {native_result:?}");
    }
    if let Err(error) = work.signal_gate(gate) {
        work.quarantined = true;
        panic!("Sync-file producer gate release is unconfirmed: {error:?}");
    }
    let queue = work
        .compute
        .queue
        .as_any()
        .downcast_ref::<crate::vulkan::Queue>()
        .unwrap();
    let producer_retired = {
        let _host = queue.device.shared_queue_group.as_ref().unwrap().host[1].lock();
        unsafe { queue.device.raw.queue_wait_idle(queue.raw) }
    };
    if let Err(error) = producer_retired {
        let drained = work.retire();
        panic!("Sync-file producer retirement failed: {error:?}, drain={drained:?}");
    }
    let device = work
        .compute
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let extension = ash::khr::external_semaphore_fd::Device::new(
        &device.shared.instance.raw,
        &device.shared.raw,
    );
    let fd = unsafe {
        extension.get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(raw)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )
    }
    .unwrap();
    eprintln!(
        "SYNC_FD export payload: {}",
        if fd == -1 {
            "already-signaled sentinel"
        } else {
            "file descriptor"
        }
    );
    let imported = if fd == -1 {
        let device = work
            .graphics
            .device
            .as_any()
            .downcast_ref::<crate::vulkan::Device>()
            .unwrap();
        unsafe { crate::vulkan::external_sync::import_signaled_sync_file(device) }
    } else {
        assert!(fd >= 0, "Unexpected sync-file export result");
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        unsafe { sync::import_sync_file(&*work.graphics.device, fd.as_fd()) }
    }
    .unwrap();
    let weak = Arc::downgrade(&imported);
    let synchronization = SubmissionSync {
        waits: vec![SemaphoreValue {
            semaphore: imported,
            value: None,
        }],
        signals: Vec::new(),
    };
    let first = graphics(&work, consumer_fence, &synchronization);
    let first = work.retain(first);
    let second = graphics(&work, repeated_fence, &synchronization);
    let rejected_without_receipt = matches!(&second, Err(error) if error.receipt.is_none());
    let second = work.retain(second);
    drop(synchronization);
    let retained = weak.upgrade().is_some();
    work.retire().unwrap();
    work.receipts.clear();
    let released = weak.upgrade().is_none();
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_err() && rejected_without_receipt);
    assert!(retained && released);
    drop(work);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires shared Vulkan and Khronos validation"]
fn shared_submit_rejects_legacy_pending_sync_without_consuming_it() {
    let mut work = Work::new();
    let gate = work.gate_token();
    let signal = work.gate_token();
    let fence = work.fence();
    work.armed_gates.borrow_mut().push(gate);
    let queue = work
        .graphics
        .queue
        .as_any()
        .downcast_ref::<crate::vulkan::Queue>()
        .unwrap();
    let mut checks = Vec::new();
    let mut chain = vk::ProtectedSubmitInfo::default();
    for kind in 0..3 {
        match kind {
            0 => queue.add_wait_semaphore(
                work.gate(gate),
                Some(1),
                vk::PipelineStageFlags::ALL_COMMANDS,
            ),
            1 => queue.add_signal_semaphore(work.gate(signal), Some(1)),
            _ => unsafe {
                queue.set_next_submit_chain((&mut chain as *mut vk::ProtectedSubmitInfo<'_>).cast())
            },
        }
        let preflight = graphics(&work, fence, &SubmissionSync::default());
        let rejected = matches!(&preflight, Err(error) if error.receipt.is_none());
        // Exercise the second check that protects a setter racing preflight.
        let native = unsafe { queue.submit_impl(&[], &[], None, &[], &[], &[]) };
        let retained = match kind {
            0 => queue.remove_wait_semaphore(work.gate(gate)),
            1 => queue.remove_signal_semaphore(work.gate(signal)),
            _ => queue.next_submit_chain.lock().take().is_some(),
        };
        checks.push((preflight, rejected, native, retained));
    }
    let mut results = Vec::new();
    for (result, rejected, native, retained) in checks {
        results.push((work.retain(result), rejected, native, retained));
    }
    let clean = graphics(&work, fence, &SubmissionSync::default());
    let clean = work.retain(clean);
    work.retire().unwrap();
    for (result, rejected, native, retained) in results {
        assert!(result.is_err() && rejected && native.is_err() && retained);
    }
    assert!(clean.is_ok(), "{clean:?}");
    drop(work);
    assert_validation_clean();
}
