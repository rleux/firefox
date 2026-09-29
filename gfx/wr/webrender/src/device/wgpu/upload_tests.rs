/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::{
    ImageFormat,
    units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize},
};

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

pub(super) fn read_texture(_device: &Rc<Device>, texture: &Rc<Texture>, bpp: usize) -> Vec<u8> {
    let size = texture.size();
    let pixels = texture
        .readback(rect(0, 0, size.width as i32, size.height as i32))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(pixels.len(), size.width as usize * size.height as usize * bpp);
    pixels
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_uploads_reuse_staging_and_clear_recycled_contents() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 3).unwrap();
    let texture = Texture::new(
        &device,
        3,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let alignment = device.capabilities().alignments.buffer_copy_pitch.get() as usize;
    let packed_size = 12usize.div_ceil(alignment) * alignment * 2;
    let seed = pool
        .upload(&vec![0xa5; packed_size], wgt::BufferUses::COPY_SRC)
        .unwrap();
    let seed_id = Rc::as_ptr(&seed);
    let allocation_size = seed.size();
    pool.recycle(seed);
    texture
        .upload(&queue, rect(1, 0, 1, 1), &[17; 4], None, 0, None)
        .unwrap();
    assert_eq!(pool.bytes(), 0);
    assert!(texture.initialized());
    queue.discard_recording();
    assert!(!texture.initialized());
    assert_eq!(pool.bytes(), allocation_size);
    texture
        .upload(&queue, rect(1, 0, 1, 1), &[23; 4], None, 0, None)
        .unwrap();
    texture
        .upload(&queue, rect(2, 1, 1, 1), &[31; 4], None, 0, None)
        .unwrap();
    assert_eq!(pool.bytes(), 0);
    queue.wait().unwrap();
    assert!(pool.bytes() > allocation_size);
    let mut expected = vec![0; 24];
    expected[4..8].fill(23);
    expected[20..24].fill(31);
    assert_eq!(read_texture(&device, &texture, 4), expected);
    let reused = pool
        .upload(&vec![0; packed_size], wgt::BufferUses::COPY_SRC)
        .unwrap();
    assert_eq!(Rc::as_ptr(&reused), seed_id);
    drop(reused);
    let foreign = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let foreign_queue = upload_queue(&foreign);
    assert!(texture
        .upload(&foreign_queue, rect(0, 0, 1, 1), &[0; 4], None, 0, None)
        .is_err());
    assert_eq!(read_texture(&device, &texture, 4), expected);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn partial_uploads_preserve_pixels_and_discard_initialization() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ] {
        for source_format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
            let texture =
                Texture::new(&device, 7, 5, format, TextureFilter::Nearest, false).unwrap();
            let abandoned = upload_queue(&device);
            texture
                .upload(&abandoned, rect(6, 4, 1, 1), &[255; 4], None, 0, None)
                .unwrap();
            assert!(texture.initialized());
            drop(abandoned);
            assert!(!texture.initialized());
            assert!(!texture.sample_initialized());
            assert_eq!(texture.current_usage(), wgt::TextureUses::UNINITIALIZED);

            let rgba = [71, 33, 9, 127];
            let bgra = [9, 33, 71, 127];
            let source_pixel = if source_format == ImageFormat::RGBA8 {
                rgba
            } else {
                bgra
            };
            let native_pixel = if format == wgt::TextureFormat::Rgba8Unorm {
                rgba
            } else {
                bgra
            };
            let mut source = vec![0xa5; 27];
            for y in 0..2 {
                for x in 0..2 {
                    source[3 + y * 12 + x * 4..3 + y * 12 + x * 4 + 4]
                        .copy_from_slice(&source_pixel);
                }
            }
            let original = source.clone();
            let commands = upload_queue(&device);
            texture
                .upload(
                    &commands,
                    rect(2, 1, 2, 2),
                    &source,
                    Some(12),
                    3,
                    Some(source_format),
                )
                .unwrap();
            texture
                .upload(
                    &commands,
                    rect(0, 0, 1, 1),
                    &[13, 17, 19, 23],
                    None,
                    0,
                    None,
                )
                .unwrap();
            assert!(texture.initialized());
            commands.submit().unwrap();
            commands.wait().unwrap();
            assert_eq!(source, original);
            let mut expected = vec![0; 7 * 5 * 4];
            for y in 1..3 {
                for x in 2..4 {
                    expected[(y * 7 + x) * 4..(y * 7 + x) * 4 + 4].copy_from_slice(&native_pixel);
                }
            }
            expected[..4].copy_from_slice(&[13, 17, 19, 23]);
            assert_eq!(read_texture(&device, &texture, 4), expected);
            let update = upload_queue(&device);
            texture
                .upload(
                    &update,
                    rect(6, 4, 1, 1),
                    &[29, 31, 37, 41],
                    None,
                    0,
                    None,
                )
                .unwrap();
            update.submit().unwrap();
            update.wait().unwrap();
            expected[136..140].copy_from_slice(&[29, 31, 37, 41]);
            assert_eq!(read_texture(&device, &texture, 4), expected);
            assert!(texture.sample_initialized());
        }
    }
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn upload_formats_and_input_validation() {
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
        let texture = Texture::new(&device, 3, 2, format, TextureFilter::Nearest, false).unwrap();
        let data: Vec<_> = (0..6 * bpp).map(|i| (i * 13) as u8).collect();
        let commands = upload_queue(&device);
        texture
            .upload(&commands, rect(0, 0, 3, 2), &data, None, 0, None)
            .unwrap();
        commands.submit().unwrap();
        commands.wait().unwrap();
        assert_eq!(read_texture(&device, &texture, bpp), data);
    }
    let width = (device.capabilities().alignments.buffer_copy_pitch.get() / 4).max(1) as i32;
    let contiguous = Texture::new(
        &device,
        width as u32,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let pixels: Vec<_> = (0..width as usize * 8).map(|i| (i * 17) as u8).collect();
    let mut source = vec![0xa5; 7];
    source.extend_from_slice(&pixels);
    let commands = upload_queue(&device);
    contiguous
        .upload(&commands, rect(0, 0, width, 2), &source, None, 7, None)
        .unwrap();
    commands.submit().unwrap();
    commands.wait().unwrap();
    assert_eq!(read_texture(&device, &contiguous, 4), pixels);
    drop(commands);
    drop(contiguous);
    let texture = Texture::new(
        &device,
        3,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let commands = upload_queue(&device);
    let extreme = DeviceIntRect::new(DeviceIntPoint::new(1, 0), DeviceIntPoint::new(i32::MIN, 1));
    for (region, stride, offset, length, format) in [
        (rect(-1, 0, 1, 1), None, 0, 4, None),
        (rect(0, 0, 0, 1), None, 0, 4, None),
        (rect(0, 0, 4, 2), None, 0, 32, None),
        (extreme, None, 0, 4, None),
        (rect(0, 0, 1, 1), None, -1, 4, None),
        (rect(0, 0, 1, 1), Some(-1), 0, 4, None),
        (rect(0, 0, 1, 1), Some(3), 0, 4, None),
        (rect(0, 0, 1, 1), None, 0, 3, None),
        (rect(0, 0, 1, 2), Some(i32::MAX), i32::MAX, 8, None),
        (rect(0, 0, 1, 1), None, 0, 4, Some(ImageFormat::R8)),
    ] {
        assert!(texture
            .upload(
                &commands,
                region,
                &vec![0; length],
                stride,
                offset,
                format
            )
            .is_err());
        assert!(!texture.initialized());
        assert_eq!(texture.current_usage(), wgt::TextureUses::UNINITIALIZED);
    }
    drop(commands);
    drop(texture);
    let depth = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let commands = upload_queue(&device);
    assert!(depth
        .upload(&commands, rect(0, 0, 1, 1), &[0; 4], None, 0, None)
        .is_err());
    drop(commands);
    drop(depth);
    let mipmapped = Texture::new(
        &device,
        3,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let commands = upload_queue(&device);
    mipmapped
        .upload(&commands, rect(0, 0, 3, 2), &[17; 24], None, 0, None)
        .unwrap();
    assert!(mipmapped.initialized());
    assert!(!mipmapped.sample_initialized());
    commands.submit().unwrap();
    commands.wait().unwrap();
    drop(commands);
    drop(mipmapped);
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
