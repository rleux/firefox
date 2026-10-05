/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::{khr, vk};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;
use webrender::vulkan::{
    Device, ForeignReleaseStatus, ForeignRgbLayout, Options, SharedTimeline, Submission,
    SyncFileWait, Texture, TextureFilter,
};

unsafe fn ready_fence(owner: &Rc<Device>) -> OwnedFd {
    let device = owner.raw_device();
    let raw = device.raw_device();
    let mut export = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let semaphore = raw
        .create_semaphore(
            &vk::SemaphoreCreateInfo::default().push_next(&mut export),
            None,
        )
        .unwrap();
    let mut send = Submission::new(owner).unwrap();
    owner.queue().add_signal_semaphore(semaphore, None);
    let result = send.submit();
    owner.queue().remove_signal_semaphore(semaphore);
    result.unwrap();
    let fd = khr::external_semaphore_fd::Device::new(device.shared_instance().raw_instance(), raw)
        .get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(semaphore)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )
        .unwrap();
    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    raw.destroy_semaphore(semaphore, None);
    assert!(fd >= 0);
    OwnedFd::from_raw_fd(fd)
}

pub struct Fixture {
    device: Rc<Device>,
    _drm: File,
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_rgb_new(drm_fd: &mut i32) -> *mut Fixture {
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let hal = device.raw_device();
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    hal.shared_instance()
        .raw_instance()
        .get_physical_device_properties2(
            hal.raw_physical_device(),
            &mut vk::PhysicalDeviceProperties2::default().push_next(&mut drm),
        );
    assert_eq!(drm.has_render, vk::TRUE);
    let info = std::fs::read_to_string(format!(
        "/sys/dev/char/{}:{}/uevent",
        drm.render_major, drm.render_minor
    ))
    .unwrap();
    let node = info
        .lines()
        .find_map(|line| line.strip_prefix("DEVNAME="))
        .unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/{node}"))
        .unwrap();
    *drm_fd = file.as_raw_fd();
    Box::into_raw(Box::new(Fixture { device, _drm: file }))
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_rgb_import(
    fixture: &Fixture,
    fd: i32,
    fourcc: u32,
    stride: u64,
) {
    let layout = ForeignRgbLayout::new([17, 9], fourcc, 0, stride, 0).unwrap();
    assert!(ForeignRgbLayout::new([17, 9], fourcc, 1, stride, 0).is_err());
    assert!(ForeignRgbLayout::new([17, 9], fourcc, 0, 67, 0).is_err());
    assert!(ForeignRgbLayout::new([0, 9], fourcc, 0, stride, 0).is_err());
    assert!(ForeignRgbLayout::new([17, 9], fourcc, 0, stride, u64::MAX - 3).is_err());
    assert!(ForeignRgbLayout::new([17, 9], u32::from_le_bytes(*b"NV12"), 0, stride, 0).is_err());
    let image = fixture
        .device
        .import_foreign_rgb(BorrowedFd::borrow_raw(fd), layout)
        .unwrap();
    assert_eq!(image.layout().size(), [17, 9]);
    assert!(layout.validate_allocation(stride * 8 + 67).is_err());
    assert!(layout.validate_allocation(stride * 8 + 68).is_ok());
    let alpha = Texture::from_foreign_rgb(&image, TextureFilter::Linear, false).unwrap();
    let opaque = Texture::from_foreign_rgb(&image, TextureFilter::Nearest, true).unwrap();
    assert!(Texture::from_foreign_rgb(&image, TextureFilter::Trilinear, false).is_err());
    let ready = ready_fence(&fixture.device);
    let released = SharedTimeline::new(&fixture.device).unwrap();
    let clone = image.clone();
    let discarded = {
        let mut abandoned = Submission::new(&fixture.device).unwrap();
        let mut commands = abandoned.recording().unwrap();
        assert!(image.release(&mut commands, &released, 1).is_err());
        image
            .acquire(
                &mut commands,
                SyncFileWait::import(&fixture.device, ready.as_fd()).unwrap(),
            )
            .unwrap();
        assert!(clone
            .acquire(
                &mut commands,
                SyncFileWait::import(&fixture.device, ready.as_fd()).unwrap()
            )
            .is_err());
        drop(commands);
        let mut other = Submission::new(&fixture.device).unwrap();
        assert!(clone
            .release(&mut other.recording().unwrap(), &released, 1)
            .is_err());
        let receipt = clone
            .release(&mut abandoned.recording().unwrap(), &released, 1)
            .unwrap();
        assert_eq!(receipt.status(), ForeignReleaseStatus::Pending);
        receipt
    };
    assert_eq!(discarded.status(), ForeignReleaseStatus::Abandoned);
    let gate_owner = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let gate = SharedTimeline::new(&gate_owner).unwrap();
    let imported_gate = SharedTimeline::import(&fixture.device, &gate.export().unwrap()).unwrap();
    for value in 1..=2 {
        let mut submission = Submission::new(&fixture.device).unwrap();
        let receipt = {
            let mut commands = submission.recording().unwrap();
            commands.wait_timeline(&imported_gate, value).unwrap();
            image
                .acquire(
                    &mut commands,
                    SyncFileWait::import(&fixture.device, ready.as_fd()).unwrap(),
                )
                .unwrap();
            let receipt = clone.release(&mut commands, &released, value).unwrap();
            assert!(image.release(&mut commands, &released, value).is_err());
            receipt
        };
        assert_eq!(receipt.status(), ForeignReleaseStatus::Pending);
        submission.submit().unwrap();
        assert!(!submission.wait(Some(Duration::from_millis(50))).unwrap());
        assert_eq!(receipt.status(), ForeignReleaseStatus::Pending);
        let mut open = Submission::new(&gate_owner).unwrap();
        open.recording()
            .unwrap()
            .signal_timeline(&gate, value)
            .unwrap();
        open.submit().unwrap();
        assert!(submission.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(open.wait(Some(Duration::from_secs(5))).unwrap());
        assert_eq!(receipt.status(), ForeignReleaseStatus::Complete);
        assert_eq!(receipt.status(), ForeignReleaseStatus::Complete);
    }
    drop(image);
    drop(alpha);
    drop(opaque);
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_rgb_delete(fixture: *mut Fixture) {
    drop(Box::from_raw(fixture));
}
