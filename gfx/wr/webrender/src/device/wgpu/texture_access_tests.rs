/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{Buffer, Device, Options, Submission, TextureFilter};
use crate::device::wgpu::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use std::sync::atomic::Ordering;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn checked_copy_destination_preserves_rollback_and_device_checks() {
    validation_logging();
    let owner = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let foreign = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let texture = Texture::new(&owner, 3, 2, wgt::TextureFormat::Rgba8Unorm, TextureFilter::Nearest, false).unwrap();
    let source = Buffer::new(&owner, &[1, 2, 3, 4], wgt::BufferUses::COPY_SRC).unwrap();
    let rect = DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(1, 0), DeviceIntSize::new(1, 1));
    let references = Rc::strong_count(&texture);
    let destination = texture.copy_destination().unwrap();
    let sampled = texture.sampled().unwrap();
    assert_eq!(Rc::strong_count(&texture), references);
    let stride = owner.capabilities.alignments.buffer_copy_pitch.get() as u32;
    let mut wrong_device = Submission::new(&foreign).unwrap();
    assert!(destination.copy_from_buffer(&mut wrong_device.recording().unwrap(), &source, rect, 0, stride).is_err());
    assert!(sampled.prepare(&mut wrong_device.recording().unwrap()).is_err());
    assert!(!texture.initialized());
    let mut abandoned = Submission::new(&owner).unwrap();
    destination.copy_from_buffer(&mut abandoned.recording().unwrap(), &source, rect, 0, stride).unwrap();
    assert!(texture.initialized());
    drop(abandoned);
    assert!(!texture.initialized());
    let mut submitted = Submission::new(&owner).unwrap();
    destination.copy_from_buffer(&mut submitted.recording().unwrap(), &source, rect, 0, stride).unwrap();
    sampled.prepare(&mut submitted.recording().unwrap()).unwrap();
    submitted.submit().unwrap();
    assert!(submitted.wait(None).unwrap());
    let full = DeviceIntRect::from_size(DeviceIntSize::new(3, 2));
    let mut expected = [0; 24];
    expected[4..8].copy_from_slice(&[1, 2, 3, 4]);
    assert_eq!(texture.readback(full).unwrap().wait().unwrap(), expected);
    let depth = Texture::new(&owner, 3, 2, wgt::TextureFormat::Depth32Float, TextureFilter::Nearest, true).unwrap();
    assert!(depth.sampled().is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
