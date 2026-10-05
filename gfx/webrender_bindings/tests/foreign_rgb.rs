/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::vk;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::rc::Rc;
use webrender::vulkan::{Device, ForeignRgbLayout, Options, Texture, TextureFilter};

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
    drop(image);
    drop(alpha);
    drop(opaque);
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_rgb_delete(fixture: *mut Fixture) {
    drop(Box::from_raw(fixture));
}
