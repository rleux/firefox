/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn readback_crops_preserve_formats_and_submission_order() {
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
        let texture = Texture::new(&device, 7, 5, format, TextureFilter::Nearest, false).unwrap();
        let bytes: Vec<u8> = (0..7 * 5 * bpp)
            .map(|i| ((i * 17 + 9) % 251) as u8)
            .collect();
        let upload = upload_queue(&device);
        texture
            .upload(&upload, rect(0, 0, 7, 5), &bytes, None, 0, None)
            .unwrap();
        upload.submit().unwrap();
        let previous = texture.current_usage();
        let mut first = texture.readback(rect(1, 2, 3, 2)).unwrap();
        assert_eq!(texture.current_usage(), previous);
        let overwrite = upload_queue(&device);
        texture
            .upload(
                &overwrite,
                rect(0, 0, 7, 5),
                &vec![0x51; bytes.len()],
                None,
                0,
                None,
            )
            .unwrap();
        overwrite.submit().unwrap();
        let mut second = texture.readback(rect(1, 2, 3, 2)).unwrap();
        let weak = Rc::downgrade(&texture);
        drop(texture);
        assert!(weak.upgrade().is_some());
        let expected: Vec<u8> = (2..4)
            .flat_map(|y| bytes[(y * 7 + 1) * bpp..(y * 7 + 4) * bpp].iter().copied())
            .collect();
        let result = match first.poll().unwrap() {
            Some(bytes) => bytes,
            None => first.wait().unwrap(),
        };
        assert_eq!(result, expected, "{format:?}");
        assert_eq!(second.wait().unwrap(), vec![0x51; 3 * 2 * bpp]);
        assert_eq!(first.poll().unwrap().unwrap(), expected);
        upload.wait().unwrap();
        overwrite.wait().unwrap();
        assert!(weak.upgrade().is_none());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn readback_rejects_invalid_sources_and_retains_device() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let texture = Texture::new(
        &device,
        3,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let full = rect(0, 0, 3, 2);
    assert!(texture.readback(full).is_err());
    let abandoned = upload_queue(&device);
    texture
        .upload(&abandoned, full, &[19; 24], None, 0, None)
        .unwrap();
    assert!(texture.readback(full).is_err());
    drop(abandoned);
    assert!(!texture.initialized());
    let upload = upload_queue(&device);
    texture
        .upload(&upload, full, &[71; 24], None, 0, None)
        .unwrap();
    upload.submit().unwrap();
    for invalid in [
        rect(-1, 0, 1, 1),
        rect(0, -1, 1, 1),
        rect(0, 0, 0, 1),
        rect(0, 0, 1, 0),
        rect(0, 0, 4, 2),
        rect(0, 0, 3, 3),
    ] {
        assert!(texture.readback(invalid).is_err());
    }
    let depth = Texture::new(
        &device,
        3,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    assert!(depth.readback(full).is_err());
    drop(depth);
    let mut pending_submission = Submission::new(&device).unwrap();
    let mut pending = pending_submission.recording().unwrap();
    texture
        .transition(&mut pending, wgt::TextureUses::COPY_SRC)
        .unwrap();
    assert!(texture.readback(full).is_err());
    drop(pending);
    drop(pending_submission);
    drop(texture.readback(full).unwrap());
    let mut readback = texture.readback(full).unwrap();
    let weak = Rc::downgrade(&device);
    drop(upload);
    drop(texture);
    drop(device);
    assert!(weak.upgrade().is_some());
    assert_eq!(readback.wait().unwrap(), vec![71; 24]);
    drop(readback);
    assert!(weak.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
