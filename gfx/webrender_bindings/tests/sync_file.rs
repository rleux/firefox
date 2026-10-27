/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::os::fd::{AsFd, AsRawFd};
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
    for (kind, fd) in [
        ("Vulkan export", wgpu_hal::dma_buf::testing::signal_sync_file(producer.hal_owner()).unwrap()),
        ("completed payload", wgpu_hal::dma_buf::testing::signaled_sync_file(producer.hal_owner()).unwrap()),
    ] {
        eprintln!("SYNC_FD fixture: {kind}");
        assert!(fd.as_raw_fd() >= 0);
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
        let mut release = Submission::new(&producer).unwrap();
        release.recording().unwrap().signal_timeline(&gate, 1).unwrap();
        receive.submit().unwrap();
        let premature = receive.wait(Some(Duration::from_millis(50)));
        if release.submit().is_err() {
            std::process::exit(88);
        }
        assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(release.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(!premature.unwrap());
        let mut next = Submission::new(&consumer).unwrap();
        next.recording().unwrap().wait_timeline(&complete, 1).unwrap();
        next.submit().unwrap();
        assert!(next.wait(Some(Duration::from_secs(5))).unwrap());
    }
}
