/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::{BufferPool, Device, Options, SubmissionQueue, TextureFilter};
use crate::device::vulkan::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntPoint, DeviceIntSize};
use ash::vk;
use std::sync::atomic::Ordering;

fn device() -> Rc<Device> {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    device
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(x, y), DeviceIntSize::new(w, h))
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mapped_buffer_copies_cover_color_formats_and_preserve_partial_updates() {
    let owner = device();
    for (format, bpp) in [
        (wgt::TextureFormat::R8Unorm, 1usize),
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
        let pool = Rc::new(BufferPool::new(&owner));
        let queue = SubmissionQueue::new(&pool, 2).unwrap();
        let texture = Texture::new(&owner, 4, 4, format, TextureFilter::Nearest, false).unwrap();
        let full = Texture::new(&owner, 2, 2, format, TextureFilter::Nearest, false).unwrap();
        let alignment = owner.capabilities.alignments.buffer_copy_pitch.get() as usize;
        let stride = (2 * bpp).div_ceil(alignment) * alignment;
        let offset = (owner.capabilities.alignments.buffer_copy_offset.get() as usize).max(bpp);
        let length = offset + stride + 2 * bpp;
        let data: Vec<_> = (0..4 * bpp).map(|i| (i * 13 + 7) as u8).collect();
        let source = pool
            .upload_with(length, wgt::BufferUses::COPY_SRC, |bytes| {
                bytes.fill(0xCC);
                bytes[offset..offset + 2 * bpp].copy_from_slice(&data[..2 * bpp]);
                bytes[offset + stride..].copy_from_slice(&data[2 * bpp..]);
                Ok(())
            })
            .unwrap();
        let pointer = Rc::as_ptr(&source);
        {
            let mut commands = queue.recording().unwrap();
            full.copy_from_buffer(
                &mut commands,
                &source,
                rect(0, 0, 2, 2),
                offset as u64,
                stride as u32,
            )
            .unwrap();
            texture
                .transition(&mut commands, wgt::TextureUses::COPY_DST)
                .unwrap();
            unsafe {
                owner.raw_device().raw_device().cmd_clear_color_image(
                    commands.encoder().raw_handle(),
                    texture.raw_texture().raw_handle(),
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
            texture
                .transition(&mut commands, wgt::TextureUses::RESOURCE)
                .unwrap();
            assert!(!texture.initialized());
            texture
                .copy_from_buffer(
                    &mut commands,
                    &source,
                    rect(1, 1, 2, 2),
                    offset as u64,
                    stride as u32,
                )
                .unwrap();
            texture
                .copy_from_buffer(
                    &mut commands,
                    &source,
                    rect(0, 0, 1, 1),
                    offset as u64,
                    stride as u32,
                )
                .unwrap();
        }
        pool.recycle(source);
        queue.wait().unwrap();
        assert_eq!(
            full.readback(rect(0, 0, 2, 2)).unwrap().wait().unwrap(),
            data
        );
        let mut expected = vec![0; 16 * bpp];
        expected[..bpp].copy_from_slice(&data[..bpp]);
        for y in 0..2 {
            let start = ((y + 1) * 4 + 1) * bpp;
            expected[start..start + 2 * bpp].copy_from_slice(&data[y * 2 * bpp..(y + 1) * 2 * bpp]);
        }
        assert_eq!(
            texture.readback(rect(0, 0, 4, 4)).unwrap().wait().unwrap(),
            expected
        );
        let reused = pool
            .upload_with(length, wgt::BufferUses::COPY_SRC, |bytes| {
                bytes.fill(0);
                Ok(())
            })
            .unwrap();
        assert_eq!(Rc::as_ptr(&reused), pointer);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn buffer_texture_copies_retain_sources_and_roll_back_mip_state() {
    let owner = device();
    let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&owner)), 2).unwrap();
    let texture = Texture::new(
        &owner,
        4,
        4,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let mip = texture.mip_view(1).unwrap();
    let stride = owner.capabilities.alignments.buffer_copy_pitch.get().max(4) as usize;
    let source = Buffer::new_with(&owner, stride + 4, wgt::BufferUses::COPY_SRC, |bytes| {
        bytes.fill(0);
        bytes[..4].copy_from_slice(&[255, 0, 0, 255]);
        bytes[stride..].copy_from_slice(&[0, 0, 255, 255]);
        Ok(())
    })
    .unwrap();
    mip.copy_from_buffer(
        &mut queue.recording().unwrap(),
        &source,
        rect(0, 0, 1, 2),
        0,
        stride as u32,
    )
    .unwrap();
    assert!(mip.initialized());
    assert!(!texture.initialized());
    queue.discard_recording();
    assert!(!mip.initialized());
    assert_eq!(source.current_usage(), wgt::BufferUses::MAP_WRITE);
    mip.copy_from_buffer(
        &mut queue.recording().unwrap(),
        &source,
        rect(0, 0, 1, 2),
        0,
        stride as u32,
    )
    .unwrap();
    let weak = Rc::downgrade(&source);
    drop(source);
    assert!(weak.upgrade().is_some());
    queue.wait().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(
        mip.readback(rect(0, 0, 2, 2)).unwrap().wait().unwrap(),
        [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0, 0]
    );
    assert!(!texture.initialized());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn buffer_texture_copies_reject_bad_layouts_and_conflicting_recordings() {
    let owner = device();
    let pool = Rc::new(BufferPool::new(&owner));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let other_queue = SubmissionQueue::new(&pool, 2).unwrap();
    let texture = Texture::new(
        &owner,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let stride = owner.capabilities.alignments.buffer_copy_pitch.get().max(8) as u32;
    let source = Buffer::new(
        &owner,
        &vec![31; stride as usize + 8],
        wgt::BufferUses::COPY_SRC,
    )
    .unwrap();
    let mut commands = queue.recording().unwrap();
    for (area, offset, pitch) in [
        (rect(-1, 0, 1, 1), 0, stride),
        (rect(0, 0, 0, 1), 0, stride),
        (rect(0, 0, 3, 2), 0, stride),
        (rect(0, 0, 2, 2), 1, stride),
        (rect(0, 0, 2, 2), 0, 0),
        (rect(0, 0, 2, 2), 0, stride + 1),
        (rect(0, 0, 2, 2), u64::MAX, stride),
        (rect(0, 0, 2, 2), u64::from(stride), stride),
    ] {
        assert!(texture
            .copy_from_buffer(&mut commands, &source, area, offset, pitch)
            .is_err());
    }
    let wrong_usage = Buffer::new(&owner, &[0; 16], wgt::BufferUses::VERTEX).unwrap();
    assert!(texture
        .copy_from_buffer(&mut commands, &wrong_usage, rect(0, 0, 1, 1), 0, stride)
        .is_err());
    let foreign = Buffer::new(&device(), &[0; 16], wgt::BufferUses::COPY_SRC).unwrap();
    assert!(texture
        .copy_from_buffer(&mut commands, &foreign, rect(0, 0, 1, 1), 0, stride)
        .is_err());
    let depth = Texture::new(
        &owner,
        2,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    assert!(depth
        .copy_from_buffer(&mut commands, &source, rect(0, 0, 2, 2), 0, stride)
        .is_err());
    assert!(!texture.initialized());
    assert_eq!(source.current_usage(), wgt::BufferUses::MAP_WRITE);
    texture
        .copy_from_buffer(&mut commands, &source, rect(0, 0, 2, 2), 0, stride)
        .unwrap();
    {
        let mut other = other_queue.recording().unwrap();
        assert!(texture
            .copy_from_buffer(&mut other, &source, rect(0, 0, 2, 2), 0, stride)
            .is_err());
        let fresh = Texture::new(
            &owner,
            2,
            2,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )
        .unwrap();
        assert!(fresh
            .copy_from_buffer(&mut other, &source, rect(0, 0, 2, 2), 0, stride)
            .is_err());
        assert!(!fresh.initialized());
    }
    drop(commands);
    queue.discard_recording();
    assert!(!texture.initialized());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
