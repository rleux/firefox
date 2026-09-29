/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use ash::vk;

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_copies_initialize_and_preserve_destinations() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    for (format, bpp) in [
        (wgt::TextureFormat::R8Unorm, 1),
        (wgt::TextureFormat::Rg8Unorm, 2),
        (wgt::TextureFormat::R16Unorm, 2),
        (wgt::TextureFormat::Rg16Unorm, 4),
        (wgt::TextureFormat::Rgba8Unorm, 4),
        (wgt::TextureFormat::Bgra8Unorm, 4),
        (wgt::TextureFormat::Rgba32Float, 16),
        (wgt::TextureFormat::Rgba32Sint, 16),
    ] {
        if !device.features().contains(format.required_features()) {
            continue;
        }
        let source = Texture::new(&device, 4, 3, format, TextureFilter::Nearest, false).unwrap();
        let destination =
            Texture::new(&device, 5, 4, format, TextureFilter::Nearest, false).unwrap();
        let data: Vec<_> = (0..4 * 3 * bpp).map(|i| (i % 251 + 1) as u8).collect();
        let queue = upload_queue(&device);
        source
            .upload(&queue, rect(0, 0, 4, 3), &data, None, 0, None)
            .unwrap();
        let mut commands = queue.recording().unwrap();
        // Seed undefined storage so zeroed allocations cannot hide a missing clear.
        destination
            .transition(&mut commands, wgt::TextureUses::COPY_DST)
            .unwrap();
        unsafe {
            device.raw_device().raw_device().cmd_clear_color_image(
                commands.encoder().as_any().downcast_ref::<hal::vulkan::CommandEncoder>().unwrap().raw_handle(),
                destination.raw_texture().as_any().downcast_ref::<hal::vulkan::Texture>().unwrap().raw_handle(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: [1.0; 4] },
                &[vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }],
            );
        }
        destination
            .transition(&mut commands, wgt::TextureUses::RESOURCE)
            .unwrap();
        assert!(!destination.initialized());
        destination
            .copy_from_texture(&mut commands, &source, rect(1, 1, 2, 2), rect(2, 1, 2, 2))
            .unwrap();
        drop(commands);
        queue.wait().unwrap();
        let mut expected = vec![0; 5 * 4 * bpp];
        let mut subregion = Vec::new();
        for y in 0..2 {
            let src = ((y + 1) * 4 + 1) * bpp;
            let dst = ((y + 1) * 5 + 2) * bpp;
            expected[dst..dst + 2 * bpp].copy_from_slice(&data[src..src + 2 * bpp]);
            subregion.extend_from_slice(&data[src..src + 2 * bpp]);
        }
        assert_eq!(
            super::upload::read_texture(&device, &destination, bpp),
            expected
        );
        let mut update_submission = Submission::new(&device).unwrap();
        let mut update = update_submission.recording().unwrap();
        destination
            .copy_from_texture(&mut update, &source, rect(0, 0, 1, 1), rect(0, 0, 1, 1))
            .unwrap();
        drop(update);
        update_submission.submit().unwrap();
        assert!(update_submission.wait(None).unwrap());
        expected[..bpp].copy_from_slice(&data[..bpp]);
        assert_eq!(
            super::upload::read_texture(&device, &destination, bpp),
            expected
        );

        let full = Texture::new(&device, 2, 2, format, TextureFilter::Nearest, false).unwrap();
        let mut commands_submission = Submission::new(&device).unwrap();
        let mut commands = commands_submission.recording().unwrap();
        full.copy_from_texture(&mut commands, &source, rect(1, 1, 2, 2), rect(0, 0, 2, 2))
            .unwrap();
        drop(commands);
        commands_submission.submit().unwrap();
        assert!(commands_submission.wait(None).unwrap());
        assert_eq!(super::upload::read_texture(&device, &full, bpp), subregion);

        let retried = Texture::new(&device, 5, 4, format, TextureFilter::Nearest, false).unwrap();
        let mut abandoned_submission = Submission::new(&device).unwrap();
        let mut abandoned = abandoned_submission.recording().unwrap();
        retried
            .copy_from_texture(&mut abandoned, &source, rect(0, 0, 1, 1), rect(0, 0, 1, 1))
            .unwrap();
        assert!(retried.initialized());
        drop(abandoned);
        drop(abandoned_submission);
        assert!(!retried.initialized());
        assert_eq!(retried.current_usage(), wgt::TextureUses::UNINITIALIZED);
        let mut commands_submission = Submission::new(&device).unwrap();
        let mut commands = commands_submission.recording().unwrap();
        retried
            .copy_from_texture(&mut commands, &source, rect(1, 1, 2, 2), rect(2, 1, 2, 2))
            .unwrap();
        drop(commands);
        commands_submission.submit().unwrap();
        assert!(commands_submission.wait(None).unwrap());
        expected[..bpp].fill(0);
        assert_eq!(
            super::upload::read_texture(&device, &retried, bpp),
            expected
        );
    }
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_copies_reject_invalid_requests() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let source = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let target = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let one = rect(0, 0, 1, 1);
    let mut commands_submission = Submission::new(&device).unwrap();
    let mut commands = commands_submission.recording().unwrap();
    assert!(target
        .copy_from_texture(&mut commands, &source, one, one)
        .is_err());
    drop(commands);
    drop(commands_submission);
    {
        let queue = upload_queue(&device);
        source
            .upload(&queue, rect(0, 0, 2, 2), &[17; 16], None, 0, None)
            .unwrap();
        queue.wait().unwrap();
    }
    let mut commands_submission = Submission::new(&device).unwrap();
    let mut commands = commands_submission.recording().unwrap();
    for (src, dst) in [
        (rect(-1, 0, 1, 1), one),
        (rect(0, 0, 0, 1), one),
        (rect(0, 0, 3, 1), one),
        (one, rect(0, 0, 1, 3)),
        (one, rect(0, 0, 2, 1)),
        (
            DeviceIntRect::new(DeviceIntPoint::new(1, 0), DeviceIntPoint::new(i32::MIN, 1)),
            one,
        ),
    ] {
        assert!(target
            .copy_from_texture(&mut commands, &source, src, dst)
            .is_err());
        assert!(!target.initialized());
    }
    assert!(source
        .copy_from_texture(&mut commands, &source, one, one)
        .is_err());
    let mismatched = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Bgra8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    assert!(mismatched
        .copy_from_texture(&mut commands, &source, one, one)
        .is_err());
    drop(mismatched);
    let depth = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    assert!(depth
        .copy_from_texture(&mut commands, &source, one, one)
        .is_err());
    drop(depth);
    let other = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let foreign = Texture::new(
        &other,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    assert!(foreign
        .copy_from_texture(&mut commands, &source, one, one)
        .is_err());
    drop(foreign);
    drop(other);
    for texture in [&source, &target] {
        let mut competing_submission = Submission::new(&device).unwrap();
        let mut competing = competing_submission.recording().unwrap();
        texture
            .transition(&mut competing, wgt::TextureUses::COPY_DST)
            .unwrap();
        assert!(target
            .copy_from_texture(&mut commands, &source, one, one)
            .is_err());
        drop(competing);
        drop(competing_submission);
    }
    target
        .copy_from_texture(&mut commands, &source, one, one)
        .unwrap();
    drop(commands);
    commands_submission.submit().unwrap();
    assert!(commands_submission.wait(None).unwrap());
    assert_eq!(
        super::upload::read_texture(&device, &target, 4),
        [17, 17, 17, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    drop(commands_submission);
    drop(source);
    drop(target);
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
