/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn read(texture: &Rc<Texture>) -> Vec<u8> {
    let size = texture.size();
    texture
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(
            size.width as i32,
            size.height as i32,
        )))
        .unwrap()
        .wait()
        .unwrap()
}

fn near(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), expected.len());
    for (&a, &b) in actual.iter().zip(expected) {
        assert!(a.abs_diff(b) <= 1, "{} != {}", a, b);
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn cpu_uploads_refresh_mip_chains_after_full_and_partial_writes() {
    let mut device = device();
    for format in [ImageFormat::RGBA8, ImageFormat::BGRA8, ImageFormat::R8] {
        device.begin_frame().unwrap();
        let mut handle = device
            .textures
            .create(
                ImageBufferKind::Texture2D,
                format,
                DeviceIntSize::new(4, 4),
                TextureFilter::Trilinear,
                None,
            )
            .unwrap();
        let pixel = if format == ImageFormat::R8 {
            vec![200]
        } else {
            vec![200, 200, 200, 255]
        };
        device
            .upload_texture_immediate(&handle, &pixel.repeat(16))
            .unwrap();
        let image = device.textures.image(&handle).unwrap();
        assert!(image.sample_initialized());
        let serial = device.end_frame().unwrap();
        device.submissions.wait_for(serial).unwrap();
        near(&read(&image.mip_view(2).unwrap()), &pixel);
        device.begin_frame().unwrap();
        let black = if format == ImageFormat::R8 {
            vec![0]
        } else {
            vec![0, 0, 0, 255]
        };
        device
            .upload_texture_region(
                &handle,
                DeviceIntRect::from_size(DeviceIntSize::new(2, 4)),
                None,
                None,
                &black.repeat(8),
            )
            .unwrap();
        let serial = device.end_frame().unwrap();
        device.submissions.wait_for(serial).unwrap();
        near(
            &read(&image.mip_view(1).unwrap()),
            &[black.clone(), pixel.clone(), black, pixel].concat(),
        );
        let expected = if format == ImageFormat::R8 {
            vec![100]
        } else {
            vec![100, 100, 100, 255]
        };
        near(&read(&image.mip_view(2).unwrap()), &expected);
        device.textures.delete(&mut handle).unwrap();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mapped_chunk_flush_regenerates_mips_from_all_destination_updates() {
    let mut device = device();
    device.begin_frame().unwrap();
    let mut handle = device
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(4, 2),
            TextureFilter::Trilinear,
            None,
        )
        .unwrap();
    let (size, stride) = device
        .uploads
        .layout(DeviceIntSize::new(2, 2), ImageFormat::RGBA8)
        .unwrap();
    let mut buffer = device.uploads.create().unwrap();
    let mapping = device
        .uploads
        .allocate(&mut buffer, size * 2, false)
        .unwrap();
    let pointer = match mapping {
        UploadBufferMapping::Transient(p) => p,
        _ => unreachable!(),
    };
    for (index, value) in [0u8, 200].iter().enumerate() {
        let row = [*value, *value, *value, 255].repeat(2);
        for y in 0..2 {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    row.as_ptr(),
                    pointer.as_ptr().cast::<u8>().add(index * size + y * stride),
                    row.len(),
                );
            }
        }
    }
    let chunks: Vec<_> = (0..2)
        .map(|index| UploadChunk {
            texture: &handle,
            rect: DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(index as i32 * 2, 0),
                DeviceIntSize::new(2, 2),
            ),
            stride: Some(stride as i32),
            offset: index * size,
            format_override: None,
        })
        .collect();
    device
        .flush_upload_buffer(&buffer, &mapping, size * 2, &chunks)
        .unwrap();
    let image = device.textures.image(&handle).unwrap();
    assert!(image.sample_initialized());
    device.uploads.delete(&mut buffer).unwrap();
    device.textures.delete(&mut handle).unwrap();
    let serial = device.end_frame().unwrap();
    device.submissions.wait_for(serial).unwrap();
    near(
        &read(&image.mip_view(1).unwrap()),
        &[0, 0, 0, 255, 200, 200, 200, 255],
    );
    near(&read(&image.mip_view(2).unwrap()), &[100, 100, 100, 255]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn nonmip_uploads_keep_integer_data_and_conversion_behavior() {
    let mut device = device();
    device.begin_frame().unwrap();
    let mut integer = device
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBAI32,
            DeviceIntSize::new(2, 2),
            TextureFilter::Nearest,
            None,
        )
        .unwrap();
    let bytes: Vec<_> = (0..16i32)
        .flat_map(|n| (n * 101 - 500).to_ne_bytes())
        .collect();
    assert!(device
        .upload_texture_immediate(&integer, &bytes[..32])
        .is_err());
    assert!(!device.textures.image(&integer).unwrap().initialized());
    device.upload_texture_immediate(&integer, &bytes).unwrap();
    let mut color = device
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::BGRA8,
            DeviceIntSize::new(2, 2),
            TextureFilter::Linear,
            None,
        )
        .unwrap();
    device
        .upload_texture_region(
            &color,
            DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
            None,
            Some(ImageFormat::RGBA8),
            &[13, 29, 71, 255].repeat(4),
        )
        .unwrap();
    let serial = device.end_frame().unwrap();
    device.submissions.wait_for(serial).unwrap();
    assert_eq!(read(&device.textures.image(&integer).unwrap()), bytes);
    assert_eq!(
        read(&device.textures.image(&color).unwrap()),
        [71, 29, 13, 255].repeat(4)
    );
    assert!(device.blitter.is_empty());
    device.textures.delete(&mut integer).unwrap();
    device.textures.delete(&mut color).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn alternating_blit_formats_retain_their_pipelines() {
    let mut device = device();
    let owner = device.quad.raw.owner.clone();
    for _ in 0..3 {
        for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm, wgt::TextureFormat::R8Unorm] {
            let target = Texture::new(&owner, 2, 2, format, TextureFilter::Nearest, true).unwrap();
            device.prepare_blitter(&target).unwrap();
        }
    }
    assert_eq!(device.blitter.len(), 3);
    assert_eq!(owner.prepared_shaders.borrow().values().map(|shader| shader.strong_count()).sum::<usize>(), 3);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
