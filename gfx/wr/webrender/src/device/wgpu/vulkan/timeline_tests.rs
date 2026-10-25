/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{BufferPool, Options, Submission, SubmissionQueue};
use crate::device::wgpu::tests::{validation_logging, ERRORS};
use std::sync::atomic::Ordering;
use std::time::Duration;

fn device() -> Rc<Device> {
    validation_logging();
    Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn native<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Native timeline operation failed: {error:?}");
            std::process::abort();
        }
    }
}

fn complete(submission: &mut Submission) {
    if !native(submission.wait(Some(Duration::from_secs(5)))) {
        eprintln!("Native timeline submission did not retire");
        std::process::abort();
    }
}

struct UnblockOnFailure {
    timeline: Rc<SharedTimeline>,
    value: u64,
    fence: Option<Box<dyn hal::DynFence>>,
}

impl UnblockOnFailure {
    fn new(timeline: &Rc<SharedTimeline>, value: u64) -> Self {
        assert!(timeline.last_signal.is_some());
        let fence = native(unsafe { timeline.owner.open.device.create_fence() });
        Self {
            timeline: timeline.clone(),
            value,
            fence: Some(fence),
        }
    }
}

impl Drop for UnblockOnFailure {
    fn drop(&mut self) {
        let owner = &self.timeline.owner;
        if owner.is_lost() {
            std::process::abort();
        }
        if self.timeline.last_signal.as_ref().unwrap().get() < self.value {
            let sync = hal::external_sync::SubmissionSync {
                waits: Vec::new(),
                signals: vec![hal::external_sync::SemaphoreValue {
                    semaphore: self.timeline.semaphore.clone(),
                    value: Some(self.value),
                }],
            };
            native(unsafe {
                hal::external_sync::submit_legacy(
                    &*owner.open.queue,
                    &[],
                    &[],
                    (&**self.fence.as_ref().unwrap(), 1),
                    &sync,
                )
            });
        }
        native(unsafe { owner.open.queue.wait_for_idle() });
        if native(hal::external_sync::current_value(&*self.timeline.semaphore)) < self.value {
            std::process::abort();
        }
        unsafe { owner.open.device.destroy_fence(self.fence.take().unwrap()) };
    }
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_reuses_imports_and_waits_before_the_producer_submits() {
    {
        let producer = device();
        let consumer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let release = SharedTimeline::new(&consumer).unwrap();
        let handle = ready.export().unwrap();
        let (fd, device_uuid, driver_uuid) = handle.into_parts();
        let handle = unsafe { TimelineHandle::from_fd(fd, device_uuid, driver_uuid) };
        let imported_ready = SharedTimeline::import(&consumer, &handle).unwrap();
        assert!(handle.as_fd().try_clone_to_owned().is_ok());
        drop(handle);
        let imported_release =
            SharedTimeline::import(&producer, &release.export().unwrap()).unwrap();
        for value in 1..=3 {
            let mut send = Submission::new(&producer).unwrap();
            let mut receive = Submission::new(&consumer).unwrap();
            let _unblock = UnblockOnFailure::new(&ready, value);
            {
                let mut recording = receive.recording().unwrap();
                recording.wait_timeline(&imported_ready, value - 1).unwrap();
                recording.wait_timeline(&imported_ready, value).unwrap();
                recording.signal_timeline(&release, value).unwrap();
                recording.signal_timeline(&release, value).unwrap();
            }
            native(receive.submit());
            assert!(!native(receive.wait(Some(Duration::from_millis(20)))));
            send.recording()
                .unwrap()
                .signal_timeline(&ready, value)
                .unwrap();
            native(send.submit());
            let mut returned = Submission::new(&producer).unwrap();
            returned
                .recording()
                .unwrap()
                .wait_timeline(&imported_release, value)
                .unwrap();
            native(returned.submit());
            complete(&mut returned);
            complete(&mut receive);
            complete(&mut send);
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_rejects_wrong_devices_identities_and_out_of_order_signals() {
    {
        let owner = device();
        let other = device();
        let timeline = SharedTimeline::new(&owner).unwrap();
        let (fd, mut device_uuid, mut driver_uuid) = timeline.export().unwrap().into_parts();
        device_uuid[0] ^= 1;
        let wrong_device =
            unsafe { TimelineHandle::from_fd(fd.try_clone().unwrap(), device_uuid, driver_uuid) };
        assert!(SharedTimeline::import(&other, &wrong_device).is_err());
        device_uuid[0] ^= 1;
        driver_uuid[0] ^= 1;
        let wrong_driver =
            unsafe { TimelineHandle::from_fd(fd.try_clone().unwrap(), device_uuid, driver_uuid) };
        assert!(SharedTimeline::import(&other, &wrong_driver).is_err());
        driver_uuid[0] ^= 1;
        let handle = unsafe { TimelineHandle::from_fd(fd, device_uuid, driver_uuid) };
        let imported = SharedTimeline::import(&other, &handle).unwrap();
        assert!(imported.export().is_err());
        let mut foreign = Submission::new(&other).unwrap();
        assert!(foreign
            .recording()
            .unwrap()
            .wait_timeline(&timeline, 1)
            .is_err());
        assert!(foreign
            .recording()
            .unwrap()
            .signal_timeline(&imported, 1)
            .is_err());
        let mut older = Submission::new(&owner).unwrap();
        assert!(older
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 0)
            .is_err());
        older
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 1)
            .unwrap();
        let mut newer = Submission::new(&owner).unwrap();
        newer
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 2)
            .unwrap();
        native(newer.submit());
        assert!(older.submit().is_err());
        assert!(!owner.is_lost());
        complete(&mut newer);
        let mut future = Submission::new(&owner).unwrap();
        future
            .recording()
            .unwrap()
            .wait_timeline(&timeline, 3)
            .unwrap();
        assert!(future.submit().is_err());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_discard_reuse_and_exported_payload_lifetime() {
    {
        let owner = device();
        let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&owner)), 1).unwrap();
        let timeline = SharedTimeline::new(&owner).unwrap();
        let handle = timeline.export().unwrap();
        queue
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 1)
            .unwrap();
        queue.discard_recording();
        assert_eq!(timeline.current_value().unwrap(), 0);
        for value in 1..=3 {
            queue
                .recording()
                .unwrap()
                .signal_timeline(&timeline, value)
                .unwrap();
            native(queue.wait());
            assert_eq!(Rc::strong_count(&timeline), 1);
        }
        let weak = Rc::downgrade(&timeline);
        queue
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 4)
            .unwrap();
        drop(timeline);
        native(queue.submit());
        assert!(weak.upgrade().is_some());
        native(queue.wait());
        assert!(weak.upgrade().is_none());
        let weak_owner = Rc::downgrade(&owner);
        drop(queue);
        drop(owner);
        assert!(weak_owner.upgrade().is_none());
        let consumer = device();
        let imported = SharedTimeline::import(&consumer, &handle).unwrap();
        let mut receive = Submission::new(&consumer).unwrap();
        receive
            .recording()
            .unwrap()
            .wait_timeline(&imported, 4)
            .unwrap();
        native(receive.submit());
        complete(&mut receive);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_cancelled_staging_does_not_leak_to_another_submission() {
    {
        let producer = device();
        let owner = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let imported = SharedTimeline::import(&owner, &ready.export().unwrap()).unwrap();
        let cancelled_signal = SharedTimeline::new(&owner).unwrap();
        let mut unrelated = Submission::new(&owner).unwrap();
        let _unblock = UnblockOnFailure::new(&ready, 1);
        let mut sync = SubmissionSync::default();
        sync.wait(&owner, &imported, 1).unwrap();
        sync.signal(&owner, &cancelled_signal, 1).unwrap();
        sync.validate().unwrap();
        drop(sync.dependencies());
        native(unrelated.submit());
        complete(&mut unrelated);
        assert_eq!(ready.current_value().unwrap(), 0);
        assert_eq!(cancelled_signal.current_value().unwrap(), 0);
        assert!(!owner.is_lost());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_enforces_the_outstanding_value_limit() {
    {
        let owner = device();
        let mut timeline = SharedTimeline::new(&owner).unwrap();
        Rc::get_mut(&mut timeline).unwrap().max_difference = 1;
        let mut invalid = Submission::new(&owner).unwrap();
        invalid
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 2)
            .unwrap();
        assert!(invalid.submit().is_err());
        let mut valid = Submission::new(&owner).unwrap();
        valid
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 1)
            .unwrap();
        native(valid.submit());
        complete(&mut valid);
        native(invalid.submit());
        complete(&mut invalid);
        assert!(!owner.is_lost());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_failure_cleanup_releases_submitted_waits() {
    let producer = device();
    let consumer = device();
    let ready = SharedTimeline::new(&producer).unwrap();
    let imported = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
    let unblock = UnblockOnFailure::new(&ready, 1);
    let mut receive = Submission::new(&consumer).unwrap();
    receive.recording().unwrap().wait_timeline(&imported, 1).unwrap();
    receive.submit().unwrap();
    let premature = receive.wait(Some(Duration::from_millis(20)));
    eprintln!("Timeline cleanup: HAL legacy submit");
    drop(unblock);
    assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
    assert!(!premature.unwrap());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
