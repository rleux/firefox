/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::cell::Cell;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn failed_operations_discard_recording_and_preserve_the_first_error() {
    let mut device = device();
    assert!(device.failure().is_none());
    assert_eq!(device.operation(|_| Ok(17)), Some(17));
    device.operation(|device| device.begin_frame()).unwrap();
    let mut handle = device
        .operation(|device| {
            device.textures.create(
                ImageBufferKind::Texture2D,
                ImageFormat::RGBA8,
                DeviceIntSize::new(2, 1),
                TextureFilter::Nearest,
                None,
            )
        })
        .unwrap();
    let image = device.textures.image(&handle).unwrap();
    device
        .operation(|device| device.upload_texture_immediate(&handle, &[255, 0, 0, 255].repeat(2)))
        .unwrap();
    assert!(image.initialized());
    assert!(device
        .operation(|device| device.upload_texture_immediate(&handle, &[]))
        .is_none());
    assert!(!image.initialized());
    let failure = device.failure().unwrap().to_owned();
    assert!(failure.contains("source is too short"));
    let called = Cell::new(false);
    assert!(device
        .operation::<()>(|_| {
            called.set(true);
            Err("a later failure".into())
        })
        .is_none());
    assert!(!called.get());
    assert_eq!(device.failure(), Some(failure.as_str()));
    assert!(device.operation(|device| device.end_frame()).is_none());
    device.textures.delete(&mut handle).unwrap();
    assert_eq!(device.submissions.submit().unwrap(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn submitted_work_and_cleanup_survive_a_later_adapter_failure() {
    let mut device = device();
    device.operation(|device| device.begin_frame()).unwrap();
    let mut handle = device
        .operation(|device| {
            device.textures.create(
                ImageBufferKind::Texture2D,
                ImageFormat::RGBA8,
                DeviceIntSize::new(2, 1),
                TextureFilter::Nearest,
                None,
            )
        })
        .unwrap();
    device
        .operation(|device| device.upload_texture_immediate(&handle, &[255, 0, 0, 255].repeat(2)))
        .unwrap();
    let fence = device
        .operation(|device| device.submissions.create_fence())
        .unwrap();
    let image = device.textures.image(&handle).unwrap();
    let owner = image.raw.owner.clone();
    let weak = Rc::downgrade(&image);
    let mut readback = image
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 1)))
        .unwrap();
    device
        .operation(|device| device.upload_texture_immediate(&handle, &[0, 0, 255, 255].repeat(2)))
        .unwrap();
    assert!(device
        .operation(|device| device.copy_texture_sub_region(
            &handle,
            1,
            0,
            &handle,
            0,
            0,
            usize::MAX,
            1
        ))
        .is_none());
    assert!(device.failure().unwrap().contains("overflow"));
    assert!(image.initialized());
    assert!(!owner.is_lost());
    device.textures.delete(&mut handle).unwrap();
    drop(image);
    device.submissions.wait_for(fence.0 as u64).unwrap();
    assert_eq!(readback.wait().unwrap(), [255, 0, 0, 255].repeat(2));
    assert!(weak.upgrade().is_none());
    drop(device);
    let mut replacement = RenderDevice::new(&owner).unwrap();
    assert!(replacement
        .operation(|device| device.begin_frame())
        .is_some());
    assert!(replacement.operation(|device| device.end_frame()).is_some());
    replacement.submissions.wait().unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn active_window_pass_releases_its_image_before_failure_deinit_or_drop() {
    use crate::device::GpuBackend;
    use crate::device::wgpu::X11Window;

    validation_logging();
    for exit in 0..3 {
        let owner = Rc::new(Device::new(&Options {
            window: Some(Rc::new(unsafe { X11Window::new() })),
            validation: true,
            ..Default::default()
        }).unwrap());
        let mut device = RenderDevice::new(&owner).unwrap();
        device.begin_frame().unwrap();
        let mut desc = descriptor();
        desc.target = DrawTarget::new_default(DeviceIntSize::new(64, 48), true);
        device.begin_render_pass(&desc).unwrap();
        device.clear_target(Some([0.0, 1.0, 0.0, 1.0]), None, None).unwrap();
        assert!(device.passes.is_active());
        match exit {
            0 => {
                assert!(device.operation::<()>(|_| Err("injected failure".into())).is_none());
                assert_eq!(device.failure(), Some("injected failure"));
                assert!(!device.passes.is_active());
                GpuBackend::deinit(&mut device);
            }
            1 => {
                GpuBackend::deinit(&mut device);
                assert!(!device.passes.is_active());
            }
            _ => (),
        }
        drop(device);
        assert!(!owner.is_lost());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn skipped_window_pass_ignores_draws_and_recovers_next_frame() {
    use crate::device::{GpuBackend, PresentResult};
    use crate::device::wgpu::{X11Window, hal, surface_testing};

    validation_logging();
    let owner = Rc::new(Device::new(&Options {
        window: Some(Rc::new(unsafe { X11Window::new() })),
        validation: true,
        ..Default::default()
    }).unwrap());
    let mut device = RenderDevice::new(&owner).unwrap();
    let mut desc = descriptor();
    desc.target = DrawTarget::new_default(DeviceIntSize::new(64, 48), true);
    for skipped in [true, false] {
        if skipped {
            surface_testing::fail_acquire(hal::SurfaceError::Timeout);
        }
        device.begin_frame().unwrap();
        device.begin_render_pass(&desc).unwrap();
        if skipped {
            device.draw_instanced(0, 1).unwrap();
        } else {
            assert!(device.draw_instanced(0, 1).is_err());
        }
        device.clear_target(Some([0.0, 1.0, 0.0, 1.0]), None, None).unwrap();
        device.end_render_pass(StoreOp::Store).unwrap();
        device.end_frame().unwrap();
        assert_eq!(GpuBackend::present_result(&device), Some(if skipped {
            PresentResult::Retry
        } else {
            PresentResult::Presented
        }));
    }
    GpuBackend::deinit(&mut device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
