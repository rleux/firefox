/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{Device, Options, TextureFilter};
use super::super::tests::{validation_logging, ERRORS};
use api::{ImageBufferKind, units::DeviceIntRect};
use crate::device::{FenceStatus, Texture as TextureHandle};
use std::sync::atomic::Ordering;

fn setup() -> (UploadBuffers, TextureStore, SubmissionQueue) {
    validation_logging();
    let owner = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", owner.info());
    let pool = Rc::new(BufferPool::new(&owner));
    (
        UploadBuffers::new(&pool),
        TextureStore::new(&owner),
        SubmissionQueue::new(&pool, 2).unwrap(),
    )
}

fn texture(textures: &mut TextureStore, format: ImageFormat) -> TextureHandle {
    textures
        .create(
            ImageBufferKind::Texture2D,
            format,
            DeviceIntSize::new(2, 2),
            TextureFilter::Nearest,
            None,
        )
        .unwrap()
}

fn pointer(mapping: &UploadBufferMapping) -> NonNull<MaybeUninit<u8>> {
    match mapping {
        UploadBufferMapping::Persistent(p) | UploadBufferMapping::Transient(p) => *p,
        _ => panic!("unmapped"),
    }
}

fn fill(mapping: &UploadBufferMapping, offset: usize, stride: usize, pixel: &[u8]) {
    let row = pixel.repeat(2);
    for y in 0..2 {
        unsafe {
            std::ptr::copy_nonoverlapping(
                row.as_ptr(),
                pointer(mapping)
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset + y * stride),
                row.len(),
            );
        }
    }
}

fn chunk(
    texture: &TextureHandle,
    stride: usize,
    offset: usize,
    format_override: Option<ImageFormat>,
) -> UploadChunk<'_> {
    UploadChunk {
        texture,
        rect: DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
        stride: Some(stride as i32),
        offset,
        format_override,
    }
}

fn pixels(textures: &TextureStore, handle: &TextureHandle) -> Vec<u8> {
    textures
        .image(handle)
        .unwrap()
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
        .unwrap()
        .wait()
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mixed_format_chunks_keep_subsequent_texels_aligned() {
    let (mut uploads, mut textures, queue) = setup();
    let size = DeviceIntSize::new(1, 1);
    let formats = [ImageFormat::R8, ImageFormat::BGRA8, ImageFormat::RGBAF32];
    let data = [
        vec![53],
        vec![10, 20, 30, 255],
        [0.25f32, -0.5, 2.0, 1.0]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect(),
    ];
    let handles: Vec<_> = formats
        .iter()
        .map(|&format| {
            textures
                .create(
                    ImageBufferKind::Texture2D,
                    format,
                    size,
                    TextureFilter::Nearest,
                    None,
                )
                .unwrap()
        })
        .collect();
    let mut used = 0;
    let mut layouts = Vec::new();
    for &format in &formats {
        let (length, stride) = uploads.layout(size, format).unwrap();
        assert_eq!(used % format.bytes_per_pixel() as usize, 0);
        layouts.push((used, stride));
        used += length;
    }
    let mut handle = uploads.create().unwrap();
    let mapping = uploads.allocate(&mut handle, used, false).unwrap();
    for ((offset, _), bytes) in layouts.iter().zip(&data) {
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                pointer(&mapping).as_ptr().cast::<u8>().add(*offset),
                bytes.len(),
            );
        }
    }
    let chunks: Vec<_> = handles
        .iter()
        .zip(&layouts)
        .map(|(texture, &(offset, stride))| UploadChunk {
            texture,
            rect: DeviceIntRect::from_size(size),
            stride: Some(stride as i32),
            offset,
            format_override: None,
        })
        .collect();
    uploads
        .flush(&handle, &mapping, used, &chunks, &textures, &queue)
        .unwrap();
    queue.wait().unwrap();
    for (texture, expected) in handles.iter().zip(data) {
        let actual = textures
            .image(texture)
            .unwrap()
            .readback(DeviceIntRect::from_size(size))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(actual, expected);
    }
    uploads.delete(&mut handle).unwrap();
    drop(chunks);
    for mut texture in handles {
        textures.delete(&mut texture).unwrap();
    }
    drop(uploads);
    drop(textures);
    drop(queue);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn transient_upload_mappings_flush_multiple_chunks_and_remap_after_fences() {
    let (mut uploads, mut textures, queue) = setup();
    let (color_size, color_stride) = uploads
        .layout(DeviceIntSize::new(2, 2), ImageFormat::RGBA8)
        .unwrap();
    let (gray_size, gray_stride) = uploads
        .layout(DeviceIntSize::new(2, 2), ImageFormat::R8)
        .unwrap();
    let mut color = texture(&mut textures, ImageFormat::RGBA8);
    let mut gray = texture(&mut textures, ImageFormat::R8);
    let mut handle = uploads.create().unwrap();
    let mapping = uploads
        .allocate(&mut handle, color_size + gray_size, false)
        .unwrap();
    let address = pointer(&mapping);
    fill(&mapping, 0, color_stride, &[255, 0, 0, 255]);
    fill(&mapping, color_size, gray_stride, &[53]);
    uploads
        .flush(
            &handle,
            &mapping,
            color_size + gray_size,
            &[
                chunk(&color, color_stride, 0, None),
                chunk(&gray, gray_stride, color_size, None),
            ],
            &textures,
            &queue,
        )
        .unwrap();
    assert!(uploads.map(&handle).is_err());
    assert!(uploads
        .flush(&handle, &mapping, 0, &[], &textures, &queue)
        .is_err());
    let fence = queue.create_fence().unwrap();
    queue.wait_for(fence.0 as u64).unwrap();
    assert_eq!(queue.poll_fence(&fence), FenceStatus::Signaled);
    assert_eq!(pixels(&textures, &color), [255, 0, 0, 255].repeat(4));
    assert_eq!(pixels(&textures, &gray), [53; 4]);
    let mapping = UploadBufferMapping::Transient(uploads.map(&handle).unwrap());
    assert_eq!(pointer(&mapping), address);
    fill(&mapping, 0, color_stride, &[0, 0, 255, 255]);
    uploads
        .flush(
            &handle,
            &mapping,
            color_size,
            &[chunk(&color, color_stride, 0, None)],
            &textures,
            &queue,
        )
        .unwrap();
    uploads.delete(&mut handle).unwrap();
    queue.wait().unwrap();
    assert_eq!(pixels(&textures, &color), [0, 0, 255, 255].repeat(4));
    textures.delete(&mut color).unwrap();
    textures.delete(&mut gray).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn persistent_upload_mappings_support_direct_and_converted_chunks() {
    let (mut uploads, mut textures, queue) = setup();
    let (size, stride) = uploads
        .layout(DeviceIntSize::new(2, 2), ImageFormat::RGBA8)
        .unwrap();
    let mut native = texture(&mut textures, ImageFormat::RGBA8);
    let mut converted = texture(&mut textures, ImageFormat::BGRA8);
    let mut handle = uploads.create().unwrap();
    let mapping = uploads.allocate(&mut handle, size, true).unwrap();
    assert!(uploads.map(&handle).is_err());
    for pixel in [[31, 71, 113, 255], [17, 47, 157, 255]] {
        fill(&mapping, 0, stride, &pixel);
        uploads
            .flush(
                &handle,
                &mapping,
                size,
                &[
                    chunk(&native, stride, 0, None),
                    chunk(&converted, stride, 0, Some(ImageFormat::RGBA8)),
                ],
                &textures,
                &queue,
            )
            .unwrap();
        let fence = queue.create_fence().unwrap();
        queue.wait_for(fence.0 as u64).unwrap();
        assert_eq!(queue.poll_fence(&fence), FenceStatus::Signaled);
        assert_eq!(pixels(&textures, &native), pixel.repeat(4));
        assert_eq!(
            pixels(&textures, &converted),
            [pixel[2], pixel[1], pixel[0], pixel[3]].repeat(4)
        );
    }
    assert!(matches!(
        uploads.entries[&handle.id].mapping,
        UploadBufferMapping::Persistent(_)
    ));
    uploads.delete(&mut handle).unwrap();
    textures.delete(&mut native).unwrap();
    textures.delete(&mut converted).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn orphaned_upload_storage_survives_pending_copies_and_returns_to_common_pool() {
    let (mut uploads, mut textures, queue) = setup();
    let pool = uploads.pool.clone();
    let (size, stride) = uploads
        .layout(DeviceIntSize::new(2, 2), ImageFormat::RGBA8)
        .unwrap();
    let mut a = texture(&mut textures, ImageFormat::RGBA8);
    let mut b = texture(&mut textures, ImageFormat::RGBA8);
    let mut handle = uploads.create().unwrap();
    let old = uploads.allocate(&mut handle, size, true).unwrap();
    let allocation = Rc::as_ptr(uploads.entries[&handle.id].buffer.as_ref().unwrap());
    fill(&old, 0, stride, &[255, 0, 0, 255]);
    uploads
        .flush(
            &handle,
            &old,
            size,
            &[chunk(&a, stride, 0, None)],
            &textures,
            &queue,
        )
        .unwrap();
    uploads.orphan(&mut handle).unwrap();
    assert_eq!(handle.get_reserved_size(), 0);
    assert!(uploads.map(&handle).is_err());
    let new = uploads.allocate(&mut handle, size, false).unwrap();
    assert_ne!(pointer(&old), pointer(&new));
    assert!(uploads
        .flush(&handle, &old, size, &[], &textures, &queue)
        .is_err());
    fill(&new, 0, stride, &[0, 0, 255, 255]);
    uploads
        .flush(
            &handle,
            &new,
            size,
            &[chunk(&b, stride, 0, None)],
            &textures,
            &queue,
        )
        .unwrap();
    uploads.delete(&mut handle).unwrap();
    drop(uploads);
    queue.wait().unwrap();
    assert_eq!(pixels(&textures, &a), [255, 0, 0, 255].repeat(4));
    assert_eq!(pixels(&textures, &b), [0, 0, 255, 255].repeat(4));
    let reused = pool
        .upload_with(size, wgt::BufferUses::COPY_SRC, |_| Ok(()))
        .unwrap();
    assert_eq!(Rc::as_ptr(&reused), allocation);
    textures.delete(&mut a).unwrap();
    textures.delete(&mut b).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn upload_handles_validate_mapping_size_and_written_ranges() {
    let (mut uploads, mut textures, queue) = setup();
    for size in [
        DeviceIntSize::new(0, 1),
        DeviceIntSize::new(-1, 1),
        DeviceIntSize::new(i32::MAX, i32::MAX),
    ] {
        assert!(uploads.layout(size, ImageFormat::RGBAF32).is_err());
    }
    let mut handle = uploads.create().unwrap();
    assert!(uploads.map(&handle).is_err());
    assert!(uploads.allocate(&mut handle, usize::MAX, true).is_err());
    assert_eq!(handle.get_reserved_size(), 0);
    let mapping = uploads.allocate(&mut handle, 4, true).unwrap();
    let mut target = texture(&mut textures, ImageFormat::RGBA8);
    assert!(uploads
        .flush(
            &handle,
            &UploadBufferMapping::Unmapped,
            0,
            &[],
            &textures,
            &queue
        )
        .is_err());
    assert!(uploads
        .flush(
            &handle,
            &UploadBufferMapping::Transient(pointer(&mapping)),
            0,
            &[],
            &textures,
            &queue
        )
        .is_err());
    assert!(uploads
        .flush(
            &handle,
            &UploadBufferMapping::Persistent(NonNull::dangling()),
            0,
            &[],
            &textures,
            &queue
        )
        .is_err());
    assert!(uploads
        .flush(&handle, &mapping, 5, &[], &textures, &queue)
        .is_err());
    let one = || UploadChunk {
        texture: &target,
        rect: DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
        stride: None,
        offset: 0,
        format_override: None,
    };
    assert!(uploads
        .flush(&handle, &mapping, 0, &[one()], &textures, &queue)
        .is_err());
    let mut invalid = one();
    invalid.offset = usize::MAX;
    assert!(uploads
        .flush(&handle, &mapping, 4, &[invalid], &textures, &queue)
        .is_err());
    assert!(!textures.image(&target).unwrap().initialized());
    uploads.orphan(&mut handle).unwrap();
    let zero = uploads.allocate(&mut handle, 0, false).unwrap();
    uploads
        .flush(&handle, &zero, 0, &[], &textures, &queue)
        .unwrap();
    assert!(uploads.map(&handle).is_ok());
    uploads.delete(&mut handle).unwrap();
    assert!(uploads.map(&handle).is_err());
    uploads.delete(&mut handle).unwrap();
    uploads.last_id = u32::MAX;
    assert!(uploads.create().is_err());
    textures.delete(&mut target).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
fn mixed_format_layout_handles_small_driver_alignments() {
    for (pitch, offset, expected) in [
        (1, 1, [0, 16, 32]),
        (4, 4, [0, 16, 32]),
        (256, 4, [0, 256, 512]),
        (1, 256, [0, 256, 512]),
    ] {
        let mut used = 0;
        for (format, expected) in [ImageFormat::R8, ImageFormat::BGRA8, ImageFormat::RGBAF32]
            .iter()
            .copied()
            .zip(expected)
        {
            assert_eq!(used, expected);
            let (length, _) =
                upload_layout(DeviceIntSize::new(1, 1), format, pitch, offset, 65536).unwrap();
            used += length;
        }
    }
}
