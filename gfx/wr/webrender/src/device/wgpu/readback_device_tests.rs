/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::GpuBackend;
use crate::device::wgpu::{
    Options,
    tests::{validation_logging, ERRORS},
};
use api::{ImageBufferKind, units::DeviceIntPoint};
use std::sync::atomic::Ordering;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn owned_readback_preserves_rows_formats_and_renderer_after_errors() {
    validation_logging();
    let owner = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let mut device = RenderDevice::new(&owner).unwrap();
    let colors = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 32, 64, 96, 128, 255, 255, 0, 255, 0, 255,
        255, 255,
    ];
    for format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
        device.begin_frame().unwrap();
        let mut texture = device
            .textures
            .create(
                ImageBufferKind::Texture2D,
                format,
                DeviceIntSize::new(3, 2),
                TextureFilter::Nearest,
                Some(crate::internal_types::RenderTargetInfo { has_depth: false }),
            )
            .unwrap();
        let mut native = colors;
        if format == ImageFormat::BGRA8 {
            for pixel in native.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        device.upload_texture_immediate(&texture, &native).unwrap();
        let mut sampled = device
            .textures
            .create(
                ImageBufferKind::Texture2D,
                format,
                DeviceIntSize::new(3, 2),
                TextureFilter::Nearest,
                None,
            )
            .unwrap();
        device.upload_texture_immediate(&sampled, &native).unwrap();
        let mut sampled_output = [17; 24];
        GpuBackend::read_texture(
            &mut device,
            &sampled,
            ImageFormat::RGBA8,
            &mut sampled_output,
        );
        assert_eq!(sampled_output, colors);
        device.textures.delete(&mut sampled).unwrap();
        let mut target = ReadTarget::from_texture(&texture);
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(3, 2));
        let mut output = [17; 24];
        assert!(GpuBackend::read_pixels_into(
            &mut device,
            target,
            rect.cast_unit(),
            ImageFormat::RGBA8,
            &mut output
        ));
        assert_eq!(output, colors);
        assert!(GpuBackend::read_pixels_into(
            &mut device,
            target,
            rect.cast_unit(),
            ImageFormat::BGRA8,
            &mut output
        ));
        for pixel in output.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        assert_eq!(output, colors);
        let crop = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(1, 0),
            DeviceIntSize::new(2, 2),
        );
        let mut cropped = [0; 16];
        assert!(GpuBackend::read_pixels_into(
            &mut device,
            target,
            crop.cast_unit(),
            ImageFormat::RGBA8,
            &mut cropped
        ));
        assert_eq!(
            cropped.as_slice(),
            [&colors[4..12], &colors[16..24]].concat()
        );
        let retained_readback_bytes = device.readback.completed.as_ref().unwrap().size_in_bytes();
        assert!(retained_readback_bytes >= colors.len());
        assert_eq!(
            device.report_memory().texture_upload_pbos,
            device.submissions.upload_pool().bytes() as usize + retained_readback_bytes,
        );
        let mut pixel = [17; 4];
        assert!(GpuBackend::read_pixels_into(
            &mut device,
            target,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)).cast_unit(),
            ImageFormat::RGBA8,
            &mut pixel,
        ));
        assert_eq!(pixel, colors[..4]);
        assert!(retained_readback_bytes > pixel.len());
        assert_eq!(
            device.readback.completed.as_ref().unwrap().size_in_bytes(),
            retained_readback_bytes,
        );
        assert_eq!(
            device.report_memory().texture_upload_pbos,
            device.submissions.upload_pool().bytes() as usize + retained_readback_bytes,
        );
        for invalid in [
            DeviceIntRect::zero(),
            DeviceIntRect::from_size(DeviceIntSize::new(4, 2)),
            rect.translate(DeviceIntPoint::new(1, 0).to_vector()),
            rect.translate(DeviceIntPoint::new(-1, 0).to_vector()),
        ] {
            output.fill(17);
            assert!(!GpuBackend::read_pixels_into(
                &mut device,
                target,
                invalid.cast_unit(),
                ImageFormat::RGBA8,
                &mut output
            ));
            assert_eq!(output, [17; 24]);
            assert!(device.failure().is_none());
        }
        let mut wrong_format = [17; 6];
        assert!(!GpuBackend::read_pixels_into(
            &mut device,
            target,
            rect.cast_unit(),
            ImageFormat::R8,
            &mut wrong_format
        ));
        assert_eq!(wrong_format, [17; 6]);
        assert!(GpuBackend::read_pixels_into(
            &mut device,
            target,
            rect.cast_unit(),
            ImageFormat::RGBA8,
            &mut output
        ));
        assert_eq!(output, colors);
        target = ReadTarget::Default;
        output.fill(17);
        assert!(!GpuBackend::read_pixels_into(
            &mut device,
            target,
            rect.cast_unit(),
            ImageFormat::RGBA8,
            &mut output
        ));
        assert_eq!(output, [17; 24]);
        device.textures.delete(&mut texture).unwrap();
        device.end_frame().unwrap();
        assert!(device.failure().is_none());
    }
    device.submissions.wait().unwrap();
    device.readback.completed.take();
    assert_eq!(
        device.report_memory().texture_upload_pbos,
        device.submissions.upload_pool().bytes() as usize,
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn window_readback_is_on_demand_and_survives_present_resize_and_retry() {
    use crate::device::{LoadOp, PresentResult};
    use crate::device::wgpu::{X11Window, surface_testing, hal};

    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let owner = Rc::new(
        Device::new(&Options {
            validation: true,
            window: Some(window.clone()),
            ..Default::default()
        })
        .unwrap(),
    );
    let mut device = RenderDevice::new(&owner).unwrap();
    let mut previous = None;
    for (request, retry, width, height) in [
        (Some(false), false, 64, 48),
        (Some(true), false, 64, 48),
        (None, false, 64, 48),
        (Some(true), false, 40, 32),
        (Some(true), true, 40, 32),
        (Some(true), false, 40, 32),
        (Some(false), false, 40, 32),
    ] {
        window.resize([width, height]);
        let capture = request == Some(true);
        if let Some(enabled) = request {
            device.prepare_readback(enabled);
        }
        if retry {
            surface_testing::fail_acquire(hal::SurfaceError::Timeout);
        }
        device.begin_frame().unwrap();
        let size = DeviceIntSize::new(width as i32, height as i32);
        device
            .begin_render_pass(&RenderPassDescriptor {
                target: DrawTarget::new_default(size, true),
                render_area: None,
                color_load: LoadOp::Clear([0.0, 0.0, 1.0, 1.0]),
                depth_load: LoadOp::Load,
            })
            .unwrap();
        device
            .clear_target(
                Some([1.0, 0.0, 0.0, 1.0]),
                None,
                Some(
                    DeviceIntRect::from_size(DeviceIntSize::new(width as i32, height as i32 / 2))
                        .cast_unit(),
                ),
            )
            .unwrap();
        device.end_render_pass(StoreOp::Store).unwrap();
        device.end_frame().unwrap();
        if let Some(weak) = previous.take() {
            assert!(std::rc::Weak::upgrade(&weak).is_none());
        }
        let swapchain = device.swapchain.as_ref().unwrap();
        assert_eq!(
            swapchain.present_result(),
            Some(if retry {
                PresentResult::Retry
            } else {
                PresentResult::Presented
            })
        );
        assert!(swapchain.current_target().is_none());
        assert!(device.textures.output().is_none());
        assert_eq!(device.readback.window.is_some(), capture && !retry);
        let mut output = vec![17; width as usize * height as usize * 4];
        assert_eq!(
            GpuBackend::read_pixels_into(
                &mut device,
                ReadTarget::Default,
                DeviceIntRect::from_size(size).cast_unit(),
                ImageFormat::RGBA8,
                &mut output
            ),
            capture && !retry
        );
        if capture && !retry {
            for (index, pixel) in output.chunks_exact(4).enumerate() {
                assert_eq!(
                    pixel,
                    if index / (width as usize) < height as usize / 2 {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    }
                );
            }
            previous = device.readback.window.as_ref().map(Rc::downgrade);
        } else {
            assert!(output.iter().all(|&byte| byte == 17));
        }
        assert!(device.failure().is_none());
        device.submissions.wait().unwrap();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn registered_raw_snapshot_rejects_invalid_requests_and_releases_cached_source() {
    validation_logging();
    let owner = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let mut device = RenderDevice::new(&owner).unwrap();
    let registry = device.textures.external_textures();
    let full = DeviceIntRect::from_size(DeviceIntSize::new(3, 2));
    for handle in [
        api::ExternalTextureHandle(0),
        api::ExternalTextureHandle(u64::MAX),
    ] {
        assert!(GpuBackend::wgpu_snapshot_shared_image(&mut device, handle, full).is_err());
    }
    let unsupported = Texture::new(
        &owner,
        3,
        2,
        wgt::TextureFormat::R8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let unsupported_handle = registry.register(&unsupported).unwrap();
    assert!(GpuBackend::wgpu_snapshot_shared_image(&mut device, unsupported_handle, full).is_err());
    assert!(!device.has_cached_readback());
    let source = Texture::new(
        &owner,
        3,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let pixels = [
        17, 31, 61, 0, 73, 101, 137, 64, 151, 173, 199, 128, 211, 223, 239, 255, 13, 29, 43, 127,
        59, 83, 107, 191,
    ];
    source
        .upload(&device.submissions, full, &pixels, None, 0, None)
        .unwrap();
    let handle = registry.register(&source).unwrap();
    let invalid =
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(-1, 0), DeviceIntSize::new(2, 2));
    assert!(GpuBackend::wgpu_snapshot_shared_image(&mut device, handle, invalid).is_err());
    assert!(device.failure().is_none());
    let (format, snapshot) =
        GpuBackend::wgpu_snapshot_shared_image(&mut device, handle, full).unwrap();
    assert_eq!(format, ImageFormat::RGBA8);
    assert_eq!(snapshot, pixels);
    assert!(device.has_cached_readback());
    device.submissions.wait().unwrap();
    let weak = Rc::downgrade(&source);
    registry.unregister(handle).unwrap();
    drop(source);
    assert!(weak.upgrade().is_none());
    assert!(device.has_cached_readback());
    registry.unregister(unsupported_handle).unwrap();
    drop((unsupported, registry, device, owner));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
