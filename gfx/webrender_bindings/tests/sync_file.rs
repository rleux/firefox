/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::{khr, vk};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;
use webrender::vulkan::{Device, Options, SharedTimeline, Submission, SyncFileWait};

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_sync_file_wait() {
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let consumer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let device = producer.raw_device();
    let raw = device.raw_device();
    let mut export =
        vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let signal = raw
        .create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut export), None)
        .unwrap();
    let mut send = Submission::new(&producer).unwrap();
    producer.queue().add_signal_semaphore(signal, None);
    let result = send.submit();
    producer.queue().remove_signal_semaphore(signal);
    result.unwrap();
    let fd = khr::external_semaphore_fd::Device::new(device.shared_instance().raw_instance(), raw)
        .get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(signal)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )
        .unwrap();
    assert!(fd >= 0);
    let fd = OwnedFd::from_raw_fd(fd);
    let wrong = SyncFileWait::import(&producer, fd.as_fd()).unwrap();
    let mut receive = Submission::new(&consumer).unwrap();
    assert!(receive.recording().unwrap().wait_sync_file(wrong).is_err());
    let wait = SyncFileWait::import(&consumer, fd.as_fd()).unwrap();
    assert!(fd.try_clone().is_ok());
    let gate = SharedTimeline::new(&producer).unwrap();
    let imported_gate = SharedTimeline::import(&consumer, &gate.export().unwrap()).unwrap();
    let complete = SharedTimeline::new(&consumer).unwrap();
    {
        let mut commands = receive.recording().unwrap();
        commands.wait_sync_file(wait).unwrap();
        commands.wait_timeline(&imported_gate, 1).unwrap();
        commands.signal_timeline(&complete, 1).unwrap();
    }
    receive.submit().unwrap();
    let premature = receive.wait(Some(Duration::from_millis(50))).unwrap();
    let mut release = Submission::new(&producer).unwrap();
    release.recording().unwrap().signal_timeline(&gate, 1).unwrap();
    release.submit().unwrap();
    assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
    assert!(release.wait(Some(Duration::from_secs(5))).unwrap());
    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    raw.destroy_semaphore(signal, None);
    assert!(!premature);
    let mut next = Submission::new(&consumer).unwrap();
    next.recording().unwrap().wait_timeline(&complete, 1).unwrap();
    next.submit().unwrap();
    assert!(next.wait(Some(Duration::from_secs(5))).unwrap());
}
