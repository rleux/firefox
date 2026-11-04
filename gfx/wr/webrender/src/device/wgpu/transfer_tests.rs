/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::tests::{device, finish, read, VALIDATION_ERRORS};
use super::super::{PendingReadback, Readback};
use api::units::{DeviceIntPoint, DeviceIntSize};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

fn texture(
    device: &Device,
    format: wgt::TextureFormat,
    width: u32,
    height: u32,
    mips: u32,
) -> Arc<core::resource::Texture> {
    device
        .create_texture(&core::resource::TextureDescriptor {
            label: Some("core transfer test".into()),
            size: wgt::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format,
            usage: wgt::TextureUsages::COPY_SRC
                | wgt::TextureUsages::COPY_DST
                | wgt::TextureUsages::TEXTURE_BINDING,
            view_formats: vec![],
        })
        .unwrap()
}

fn capture(
    device: &Device,
    texture: &Arc<core::resource::Texture>,
    mip: u32,
    area: DeviceIntRect,
) -> Vec<u8> {
    let encoder = device.create_command_encoder("transfer capture").unwrap();
    let capture: Readback = device
        .encode_readback(&encoder, texture, mip, area, None)
        .unwrap();
    let index = device.submit(&[finish(&encoder)]).unwrap();
    capture.map(device, index).unwrap().wait(device).unwrap()
}

#[test]
fn transfer_layout_padding_and_limits() {
    let extent = |width, height| wgt::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    for bpp in [1, 2, 4, 16] {
        let layout = Layout::new(extent(3, 2), bpp, 512).unwrap();
        assert_eq!(
            (layout.row_bytes, layout.pitch, layout.size),
            (3 * bpp, 256, 512)
        );
        assert_eq!(layout.pixel_length(), 6 * bpp as usize);
    }
    for (width, height, bpp, limit) in [
        (0, 1, 4, u64::MAX),
        (1, 0, 4, u64::MAX),
        (1, 1, 0, u64::MAX),
        (u32::MAX, 1, 4, u64::MAX),
        (u32::MAX, 1, 1, u64::MAX),
        (u32::MAX - 255, u32::MAX, 1, u64::MAX),
        (3, 2, 4, 511),
    ] {
        assert!(Layout::new(extent(width, height), bpp, limit).is_err());
    }
}

fn check_buffer_snapshots(device: &Device) {
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let mapping = if device
        .features()
        .contains(wgt::Features::MAPPABLE_PRIMARY_BUFFERS)
    {
        wgt::BufferUsages::MAP_WRITE
    } else {
        wgt::BufferUsages::empty()
    };
    for primary in [
        wgt::BufferUsages::VERTEX,
        wgt::BufferUsages::INDEX,
        wgt::BufferUsages::UNIFORM,
    ] {
        let usage = primary | wgt::BufferUsages::COPY_SRC;
        for length in 0..=9 {
            let mut bytes: Vec<_> = (0..length).map(|i| (i * 17 + 3) as u8).collect();
            let original = bytes.clone();
            let source = device
                .create_buffer_with_data("immutable snapshot", &bytes, usage)
                .unwrap();
            let size = source.size();
            assert_eq!(size, ((length as u64 + 3) / 4 * 4).max(4));
            assert_eq!(source.usage(), usage | mapping);
            assert!(source.get_mapped_range(0, Some(size)).is_err());
            let target = device
                .create_buffer(&core::resource::BufferDescriptor {
                    label: None,
                    size: 2 * size,
                    usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                })
                .unwrap();
            let encoder = device.create_command_encoder("snapshot versions").unwrap();
            encoder.copy_buffer_to_buffer(source.clone(), 0, target.clone(), 0, Some(size));
            bytes.fill(71);
            let next = device
                .create_buffer_with_data("next snapshot", &bytes, usage)
                .unwrap();
            assert_eq!(next.usage(), usage | mapping);
            assert!(next.get_mapped_range(0, Some(size)).is_err());
            encoder.copy_buffer_to_buffer(next.clone(), 0, target.clone(), size, Some(size));
            bytes.fill(0);
            drop(bytes);
            drop(source);
            drop(next);
            let index = device.submit(&[finish(&encoder)]).unwrap();
            device.wait_for(index).unwrap();
            let mut expected = vec![0; (2 * size) as usize];
            expected[..length].copy_from_slice(&original);
            expected[size as usize..size as usize + length].fill(71);
            assert_eq!(read(device, &target, 2 * size), expected);
        }
    }
    assert_eq!(
        device
            .create_buffer_with_data("invalid snapshot usage", &[1], wgt::BufferUsages::empty())
            .err()
            .unwrap()
            .kind,
        wgt::error::ErrorType::Validation
    );
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

fn snapshot_device(primary_mapping: bool) -> Device {
    let selected = device();
    let owner = Device::new(&super::super::Options {
        adapter_name: Some(selected.info().name),
        validation: true,
        primary_mapping: Some(primary_mapping),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        owner
            .features()
            .contains(wgt::Features::MAPPABLE_PRIMARY_BUFFERS),
        primary_mapping
    );
    owner
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_buffer_snapshots_pad_tails_without_changing_previous_versions() {
    check_buffer_snapshots(&device());
}

#[test]
#[ignore = "Requires Vulkan and supported primary mapping with the Khronos validation layer"]
fn native_buffer_snapshots_use_primary_mapping_when_enabled() {
    check_buffer_snapshots(&snapshot_device(true));
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_buffer_snapshots_keep_staging_fallback_without_primary_mapping() {
    check_buffer_snapshots(&snapshot_device(false));
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_queue_writes_accept_unpadded_texture_rows_and_validate_four_byte_buffers() {
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let owner = device();
    for (format, bpp) in [
        (wgt::TextureFormat::R8Unorm, 1),
        (wgt::TextureFormat::Rg8Unorm, 2),
    ] {
        for width in [1, 3, 5] {
            let target = texture(&owner, format, width, 2, 1);
            let row = width as usize * bpp;
            let data: Vec<_> = (0..2 * row).map(|i| (i * 13 + 5) as u8).collect();
            let mut source = vec![0xCC; 2 * (row + 1) + 1];
            source[1..1 + row].copy_from_slice(&data[..row]);
            source[row + 2..row * 2 + 2].copy_from_slice(&data[row..]);
            owner.queue.write_texture(
                wgt::TexelCopyTextureInfo {
                    texture: target.clone(),
                    mip_level: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &source,
                &wgt::TexelCopyBufferLayout {
                    offset: 1,
                    bytes_per_row: Some((row + 1) as u32),
                    rows_per_image: None,
                },
                &target.descriptor().size,
            );
            owner.check().unwrap();
            assert_eq!(
                capture(&owner, &target, 0, rect(0, 0, width as i32, 2)),
                data
            );
        }
    }
    let target = owner
        .create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 8,
            usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
        .unwrap();
    owner.queue.write_buffer(target.clone(), 0, &[1, 2, 3, 4]);
    owner.queue.write_buffer(target.clone(), 4, &[5, 6, 7, 8]);
    let index = owner.submit(&[]).unwrap();
    owner.wait_for(index).unwrap();
    assert_eq!(read(&owner, &target, 8), [1, 2, 3, 4, 5, 6, 7, 8]);
    for (offset, length) in [(0, 3), (1, 4)] {
        let owner = device();
        let target = owner
            .create_buffer(&core::resource::BufferDescriptor {
                label: None,
                size: 8,
                usage: wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
            .unwrap();
        owner.queue.write_buffer(target, offset, &vec![19; length]);
        assert_eq!(
            owner.failure().unwrap().kind,
            wgt::error::ErrorType::Validation
        );
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_ordered_partial_uploads_and_readbacks_preserve_neighbors_and_channel_rows() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ] {
        for source_format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
            let target = texture(&owner, format, 7, 5, 1);
            let encoder = owner.create_command_encoder("ordered updates").unwrap();
            let mut source = vec![0xCC; 27];
            let mut expected = vec![0; 7 * 5 * 4];
            for y in 0..2 {
                for x in 0..2 {
                    let mut pixel = [71 + x as u8, 33 + y as u8, 9, 127];
                    let pos = ((y + 1) * 7 + x + 2) * 4;
                    if format == wgt::TextureFormat::Bgra8Unorm {
                        pixel.swap(0, 2);
                    }
                    expected[pos..pos + 4].copy_from_slice(&pixel);
                    let mut pixel = [71 + x as u8, 33 + y as u8, 9, 127];
                    if source_format == ImageFormat::BGRA8 {
                        pixel.swap(0, 2);
                    }
                    source[3 + y * 12 + x * 4..7 + y * 12 + x * 4].copy_from_slice(&pixel);
                }
            }
            let original = source.clone();
            owner
                .upload_texture(
                    &encoder,
                    &target,
                    0,
                    rect(2, 1, 2, 2),
                    &source,
                    Some(12),
                    3,
                    Some(source_format),
                )
                .unwrap();
            assert_eq!(source, original);
            let first = owner
                .encode_readback(&encoder, &target, 0, rect(0, 0, 7, 5), None)
                .unwrap();
            owner
                .upload_texture(
                    &encoder,
                    &target,
                    0,
                    rect(6, 4, 1, 1),
                    &[29, 31, 37, 41],
                    None,
                    0,
                    None,
                )
                .unwrap();
            let second = owner
                .encode_readback(
                    &encoder,
                    &target,
                    0,
                    rect(1, 1, 6, 4),
                    Some(ImageFormat::RGBA8),
                )
                .unwrap();
            let index = owner.submit(&[finish(&encoder)]).unwrap();
            assert_eq!(
                first.map(&owner, index).unwrap().wait(&owner).unwrap(),
                expected
            );
            expected[136..140].copy_from_slice(&[29, 31, 37, 41]);
            let mut crop: Vec<_> = (1..5)
                .flat_map(|y| expected[(y * 7 + 1) * 4..(y * 7 + 7) * 4].iter().copied())
                .collect();
            if format == wgt::TextureFormat::Bgra8Unorm {
                for pixel in crop.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }
            assert_eq!(
                second.map(&owner, index).unwrap().wait(&owner).unwrap(),
                crop
            );
            assert_eq!(capture(&owner, &target, 0, rect(0, 0, 7, 5)), expected);
        }
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_texture_transfers_cover_color_formats_and_independent_mips() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
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
        if !owner.features().contains(format.required_features()) {
            continue;
        }
        let target = texture(&owner, format, 7, 5, 3);
        let encoder = owner
            .create_command_encoder("color and mip uploads")
            .unwrap();
        let data: Vec<_> = (0..7 * 5 * bpp).map(|i| (i * 17 + 3) as u8).collect();
        owner
            .upload_texture(&encoder, &target, 0, rect(0, 0, 7, 5), &data, None, 0, None)
            .unwrap();
        owner
            .upload_texture(
                &encoder,
                &target,
                1,
                rect(1, 0, 1, 2),
                &vec![31; 2 * bpp],
                None,
                0,
                None,
            )
            .unwrap();
        let index = owner.submit(&[finish(&encoder)]).unwrap();
        owner.wait_for(index).unwrap();
        assert_eq!(capture(&owner, &target, 0, rect(0, 0, 7, 5)), data);
        let mut expected = vec![0; 3 * 2 * bpp];
        expected[bpp..2 * bpp].fill(31);
        expected[4 * bpp..5 * bpp].fill(31);
        assert_eq!(capture(&owner, &target, 1, rect(0, 0, 3, 2)), expected);
        assert_eq!(capture(&owner, &target, 2, rect(0, 0, 1, 1)), vec![0; bpp]);
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_encoder_texture_copies_validate_padding_and_preserve_partial_destinations() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for (format, bpp) in [
        (wgt::TextureFormat::R8Unorm, 1),
        (wgt::TextureFormat::Rg8Unorm, 2),
    ] {
        let target = texture(&owner, format, 5, 4, 1);
        let mut bytes = vec![0xCC; 256 + 3 * bpp];
        bytes[..3 * bpp].fill(17);
        bytes[256..].fill(31);
        let source = owner
            .create_buffer_with_data("padded copy", &bytes, wgt::BufferUsages::COPY_SRC)
            .unwrap();
        let encoder = owner.create_command_encoder("padded texture copy").unwrap();
        let (destination, size, _) = region(&target, 0, rect(1, 1, 3, 2)).unwrap();
        encoder.copy_buffer_to_texture(
            &wgt::TexelCopyBufferInfo {
                buffer: source,
                layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: None,
                },
            },
            &destination,
            &size,
        );
        let copy = texture(&owner, format, 5, 4, 1);
        let (source, size, _) = region(&target, 0, rect(1, 1, 3, 2)).unwrap();
        let (destination, _, _) = region(&copy, 0, rect(0, 2, 3, 2)).unwrap();
        encoder.copy_texture_to_texture(&source, &destination, &size);
        let index = owner.submit(&[finish(&encoder)]).unwrap();
        owner.wait_for(index).unwrap();
        let mut expected = vec![0; 5 * 4 * bpp];
        expected[6 * bpp..9 * bpp].fill(17);
        expected[11 * bpp..14 * bpp].fill(31);
        assert_eq!(capture(&owner, &target, 0, rect(0, 0, 5, 4)), expected);
        let mut expected = vec![0; 5 * 4 * bpp];
        expected[10 * bpp..13 * bpp].fill(17);
        expected[15 * bpp..18 * bpp].fill(31);
        assert_eq!(capture(&owner, &copy, 0, rect(0, 0, 5, 4)), expected);
    }
    for (offset, pitch) in [(0, 6), (1, 256)] {
        let owner = device();
        let target = texture(&owner, wgt::TextureFormat::Rg8Unorm, 3, 2, 1);
        let source = owner
            .create_buffer_with_data("bad copy", &[0; 512], wgt::BufferUsages::COPY_SRC)
            .unwrap();
        let encoder = owner.create_command_encoder("bad copy layout").unwrap();
        let (destination, size, _) = region(&target, 0, rect(0, 0, 3, 2)).unwrap();
        encoder.copy_buffer_to_texture(
            &wgt::TexelCopyBufferInfo {
                buffer: source,
                layout: wgt::TexelCopyBufferLayout {
                    offset,
                    bytes_per_row: Some(pitch),
                    rows_per_image: None,
                },
            },
            &destination,
            &size,
        );
        drop(finish(&encoder));
        assert_eq!(
            owner.failure().unwrap().kind,
            wgt::error::ErrorType::Validation
        );
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_abandoned_uploads_do_not_initialize_or_change_mip_pixels() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let target = texture(&owner, wgt::TextureFormat::Rgba8Unorm, 4, 4, 3);
    for finished in [false, true] {
        let encoder = owner
            .create_command_encoder("abandoned texture upload")
            .unwrap();
        owner
            .upload_texture(
                &encoder,
                &target,
                1,
                rect(1, 1, 1, 1),
                &[255; 4],
                None,
                0,
                None,
            )
            .unwrap();
        if finished {
            drop(finish(&encoder));
        }
        drop(encoder);
        assert_eq!(capture(&owner, &target, 1, rect(0, 0, 2, 2)), [0; 16]);
    }
    let encoder = owner
        .create_command_encoder("partial after abandoned upload")
        .unwrap();
    owner
        .upload_texture(
            &encoder,
            &target,
            1,
            rect(0, 0, 1, 1),
            &[31; 4],
            None,
            0,
            None,
        )
        .unwrap();
    let index = owner.submit(&[finish(&encoder)]).unwrap();
    owner.wait_for(index).unwrap();
    let encoder = owner.create_command_encoder("abandoned overwrite").unwrap();
    owner
        .upload_texture(
            &encoder,
            &target,
            1,
            rect(0, 0, 2, 2),
            &[255; 16],
            None,
            0,
            None,
        )
        .unwrap();
    drop(finish(&encoder));
    drop(encoder);
    let mut expected = [0; 16];
    expected[..4].fill(31);
    assert_eq!(capture(&owner, &target, 1, rect(0, 0, 2, 2)), expected);
    assert_eq!(capture(&owner, &target, 0, rect(0, 0, 4, 4)), [0; 64]);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_transfer_validation_precedes_encoding_and_readback_wait_preserves_outputs() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let target = texture(&owner, wgt::TextureFormat::Rgba8Unorm, 3, 2, 1);
    let encoder = owner
        .create_command_encoder("invalid upload inputs")
        .unwrap();
    let extreme = DeviceIntRect::new(DeviceIntPoint::new(1, 0), DeviceIntPoint::new(i32::MIN, 1));
    for (area, stride, offset, length, format) in [
        (rect(-1, 0, 1, 1), None, 0, 4, None),
        (rect(0, -1, 1, 1), None, 0, 4, None),
        (rect(0, 0, 0, 1), None, 0, 4, None),
        (rect(0, 0, 4, 2), None, 0, 32, None),
        (extreme, None, 0, 4, None),
        (rect(0, 0, 1, 1), None, -1, 4, None),
        (rect(0, 0, 1, 1), Some(-1), 0, 4, None),
        (rect(0, 0, 1, 1), Some(3), 0, 4, None),
        (rect(0, 0, 1, 1), None, 0, 3, None),
        (rect(0, 0, 1, 1), None, 0, 4, Some(ImageFormat::R8)),
        (rect(0, 0, 1, 2), Some(i32::MAX), i32::MAX, 8, None),
    ] {
        assert_eq!(
            owner
                .upload_texture(
                    &encoder,
                    &target,
                    0,
                    area,
                    &vec![0; length],
                    stride,
                    offset,
                    format
                )
                .unwrap_err()
                .kind,
            wgt::error::ErrorType::Validation
        );
        assert!(owner.failure().is_none());
    }
    assert!(owner
        .upload_texture(
            &encoder,
            &target,
            1,
            rect(0, 0, 1, 1),
            &[0; 4],
            None,
            0,
            None
        )
        .is_err());
    assert!(owner
        .encode_readback(&encoder, &target, 0, rect(0, 0, 4, 2), None)
        .is_err());
    assert!(owner
        .encode_readback(
            &encoder,
            &target,
            0,
            rect(0, 0, 3, 2),
            Some(ImageFormat::RG8)
        )
        .is_err());
    let foreign = device();
    let foreign_encoder = foreign.create_command_encoder("foreign transfer").unwrap();
    assert!(foreign
        .upload_texture(
            &encoder,
            &target,
            0,
            rect(0, 0, 1, 1),
            &[0; 4],
            None,
            0,
            None
        )
        .is_err());
    assert!(owner
        .upload_texture(
            &foreign_encoder,
            &target,
            0,
            rect(0, 0, 1, 1),
            &[0; 4],
            None,
            0,
            None
        )
        .is_err());
    assert!(foreign
        .encode_readback(&foreign_encoder, &target, 0, rect(0, 0, 1, 1), None)
        .is_err());
    assert!(owner
        .encode_readback(&foreign_encoder, &target, 0, rect(0, 0, 1, 1), None)
        .is_err());
    assert!(foreign.failure().is_none());
    assert!(owner.failure().is_none());
    owner
        .upload_texture(
            &encoder,
            &target,
            0,
            rect(0, 0, 3, 2),
            &[31; 24],
            None,
            0,
            None,
        )
        .unwrap();
    let capture = owner
        .encode_readback(&encoder, &target, 0, rect(0, 0, 3, 2), None)
        .unwrap();
    let index = owner.submit(&[finish(&encoder)]).unwrap();
    let mut pending: PendingReadback = capture.map(&owner, index).unwrap();
    assert!(pending.poll(&foreign).is_err());
    assert!(pending.wait(&foreign).is_err());
    let mut wrong = [17; 23];
    assert!(pending.wait_into(&owner, &mut wrong).is_err());
    assert_eq!(wrong, [17; 23]);
    let mut pixels = [0; 24];
    pending.wait_into(&owner, &mut pixels).unwrap();
    assert_eq!(pixels, [31; 24]);
    assert_eq!(pending.poll(&owner).unwrap().unwrap(), [31; 24]);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_pending_readbacks_progress_while_idle_and_drop_safely_after_loss() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let target = texture(&owner, wgt::TextureFormat::R8Unorm, 3, 2, 1);
    let encoder = owner.create_command_encoder("idle readback").unwrap();
    owner
        .upload_texture(
            &encoder,
            &target,
            0,
            rect(0, 0, 3, 2),
            &[1, 2, 3, 4, 5, 6],
            None,
            0,
            None,
        )
        .unwrap();
    let abandoned = owner
        .encode_readback(&encoder, &target, 0, rect(0, 0, 3, 2), None)
        .unwrap();
    let capture = owner
        .encode_readback(&encoder, &target, 0, rect(0, 0, 3, 2), None)
        .unwrap();
    let index = owner.submit(&[finish(&encoder)]).unwrap();
    drop(abandoned.map(&owner, index).unwrap());
    let mut pending = capture.map(&owner, index).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(bytes) = pending.poll(&owner).unwrap() {
            assert_eq!(bytes, [1, 2, 3, 4, 5, 6]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Readback made no progress while idle"
        );
        std::thread::yield_now();
    }
    drop(pending);
    let encoder = owner.create_command_encoder("lost readback").unwrap();
    let capture = owner
        .encode_readback(&encoder, &target, 0, rect(0, 0, 3, 2), None)
        .unwrap();
    let index = owner.submit(&[finish(&encoder)]).unwrap();
    let mut pending = capture.map(&owner, index).unwrap();
    owner.device.destroy();
    assert_eq!(
        pending.wait(&owner).unwrap_err().kind,
        wgt::error::ErrorType::DeviceLost
    );
    drop(pending);
    assert_eq!(
        owner.poll().unwrap_err().kind,
        wgt::error::ErrorType::DeviceLost
    );
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_blocking_readback_waits_for_callback_delivery_after_gpu_completion() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let target = texture(&owner, wgt::TextureFormat::R8Unorm, 3, 2, 1);
    let encoder = owner
        .create_command_encoder("delayed readback callback")
        .unwrap();
    owner
        .upload_texture(
            &encoder,
            &target,
            0,
            rect(0, 0, 3, 2),
            &[1, 2, 3, 4, 5, 6],
            None,
            0,
            None,
        )
        .unwrap();
    let capture = owner
        .encode_readback(&encoder, &target, 0, rect(0, 0, 3, 2), None)
        .unwrap();
    let index = owner.submit(&[finish(&encoder)]).unwrap();
    let mut pending = capture.map(&owner, index).unwrap();
    owner.wait_for(index).unwrap();
    let status = pending.callback.recv().unwrap();
    // Withhold the actual result to model a concurrent poll between collection and delivery.
    let (tx, callback) = mpsc::channel();
    pending.callback = callback;
    let (started, start) = mpsc::channel();
    let (done, completion) = mpsc::channel();
    std::thread::scope(|scope| {
        let pending = &mut pending;
        let owner = &owner;
        scope.spawn(move || {
            started.send(()).unwrap();
            done.send(pending.wait(owner)).unwrap();
        });
        start.recv_timeout(Duration::from_secs(10)).unwrap();
        let premature = completion.recv_timeout(Duration::from_millis(100));
        let _ = tx.send(status);
        assert!(
            matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)),
            "Blocking readback returned before mapping callback delivery: {:?}",
            premature
        );
        assert_eq!(
            completion
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
    });

    let error = Failure::validation("delayed mapping failure");
    let (tx, callback) = mpsc::channel();
    pending.callback = callback;
    pending.status = None;
    tx.send(Err(error.clone())).unwrap();
    assert_eq!(pending.wait(&owner).unwrap_err(), error);
    let (tx, callback) = mpsc::channel();
    pending.callback = callback;
    pending.status = None;
    drop(tx);
    let error = pending.wait(&owner).unwrap_err();
    assert_eq!(error.kind, wgt::error::ErrorType::Internal);
    assert_eq!(error.message, "Vulkan readback mapping callback was lost");
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
