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

struct UnblockOnFailure(Rc<SharedTimeline>, u64);
impl Drop for UnblockOnFailure {
    fn drop(&mut self) {
        if self.0.last_signal.as_ref().unwrap().get() < self.1 {
            let owner = &self.0.semaphore.owner;
            let info = vk::SemaphoreSignalInfo::default()
                .semaphore(*self.0.semaphore)
                .value(self.1);
            unsafe {
                let extension = owner.native_cache.timeline.get_or_init(|| {
                    owner.raw_device().enabled_device_extensions().contains(&ash::khr::timeline_semaphore::NAME)
                        .then(|| ash::khr::timeline_semaphore::Device::new(
                            owner.raw_device().shared_instance().raw_instance(), owner.raw_device().raw_device(),
                        ))
                });
                let _ = if let Some(extension) = extension {
                    extension.signal_semaphore(&info)
                } else {
                    owner.raw_device().raw_device().signal_semaphore(&info)
                };
            }
        }
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
            let _unblock = UnblockOnFailure(ready.clone(), value);
            {
                let mut recording = receive.recording().unwrap();
                recording.wait_timeline(&imported_ready, value - 1).unwrap();
                recording.wait_timeline(&imported_ready, value).unwrap();
                recording.signal_timeline(&release, value).unwrap();
                recording.signal_timeline(&release, value).unwrap();
            }
            receive.submit().unwrap();
            assert!(!receive.wait(Some(Duration::from_millis(20))).unwrap());
            send.recording()
                .unwrap()
                .signal_timeline(&ready, value)
                .unwrap();
            send.submit().unwrap();
            let mut returned = Submission::new(&producer).unwrap();
            returned
                .recording()
                .unwrap()
                .wait_timeline(&imported_release, value)
                .unwrap();
            returned.submit().unwrap();
            assert!(returned.wait(Some(Duration::from_secs(5))).unwrap());
            assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
            assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
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
        let mut handle = timeline.export().unwrap();
        handle.device_uuid[0] ^= 1;
        assert!(SharedTimeline::import(&other, &handle).is_err());
        handle.device_uuid[0] ^= 1;
        handle.driver_uuid[0] ^= 1;
        assert!(SharedTimeline::import(&other, &handle).is_err());
        handle.driver_uuid[0] ^= 1;
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
        newer.submit().unwrap();
        assert!(older.submit().is_err());
        assert!(!owner.is_lost());
        assert!(newer.wait(Some(Duration::from_secs(5))).unwrap());
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
        assert_eq!(timeline.last_signal.as_ref().unwrap().get(), 0);
        for value in 1..=3 {
            queue
                .recording()
                .unwrap()
                .signal_timeline(&timeline, value)
                .unwrap();
            queue.wait().unwrap();
            assert_eq!(Rc::strong_count(&timeline), 1);
        }
        let weak = Rc::downgrade(&timeline);
        queue
            .recording()
            .unwrap()
            .signal_timeline(&timeline, 4)
            .unwrap();
        drop(timeline);
        queue.submit().unwrap();
        assert!(weak.upgrade().is_some());
        queue.wait().unwrap();
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
        receive.submit().unwrap();
        assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires external Vulkan timelines and validation"]
fn timeline_cancelled_staging_does_not_leak_to_another_submission() {
    {
        let owner = device();
        let timeline = SharedTimeline::new(&owner).unwrap();
        let mut send = Submission::new(&owner).unwrap();
        send.recording()
            .unwrap()
            .signal_timeline(&timeline, 1)
            .unwrap();
        send.submit().unwrap();
        let mut sync = SubmissionSync::default();
        sync.wait(&owner, &timeline, 1).unwrap();
        sync.signal(&owner, &timeline, 2).unwrap();
        sync.validate().unwrap();
        drop(sync.stage(owner.queue()));
        assert!(!owner.queue().remove_wait_semaphore(*timeline.semaphore));
        assert!(!owner.queue().remove_signal_semaphore(*timeline.semaphore));
        assert_eq!(timeline.last_signal.as_ref().unwrap().get(), 1);
        assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
        let mut unrelated = Submission::new(&owner).unwrap();
        unrelated.submit().unwrap();
        assert!(unrelated.wait(Some(Duration::from_secs(5))).unwrap());
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
        valid.submit().unwrap();
        assert!(valid.wait(Some(Duration::from_secs(5))).unwrap());
        invalid.submit().unwrap();
        assert!(invalid.wait(Some(Duration::from_secs(5))).unwrap());
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
    let unblock = UnblockOnFailure(ready.clone(), 1);
    let mut receive = Submission::new(&consumer).unwrap();
    receive.recording().unwrap().wait_timeline(&imported, 1).unwrap();
    receive.submit().unwrap();
    let premature = receive.wait(Some(Duration::from_millis(20)));
    eprintln!("Timeline cleanup: {}", if producer.raw_device().enabled_device_extensions().contains(&ash::khr::timeline_semaphore::NAME) { "KHR" } else { "core" });
    drop(unblock);
    assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
    assert!(!premature.unwrap());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
