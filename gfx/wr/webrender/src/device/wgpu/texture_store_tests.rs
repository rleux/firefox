/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{BufferPool, Options, SubmissionQueue};
use super::super::draw::DrawPass;
use super::super::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntPoint, DeviceIntRect};
use crate::device::DrawTarget;
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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_texture_handles_match_native_images_and_metadata() {
    let device = device();
    let mut store = TextureStore::new(&device);
    let frame = GpuFrameId::new(7);
    store.begin_frame(frame);
    let size = DeviceIntSize::new(2, 3);
    let formats = [
        (ImageFormat::RGBA8, wgt::TextureFormat::Rgba8Unorm),
        (ImageFormat::BGRA8, wgt::TextureFormat::Bgra8Unorm),
        (ImageFormat::R8, wgt::TextureFormat::R8Unorm),
        (ImageFormat::RG8, wgt::TextureFormat::Rg8Unorm),
        (ImageFormat::R16, wgt::TextureFormat::R16Unorm),
        (ImageFormat::RG16, wgt::TextureFormat::Rg16Unorm),
        (ImageFormat::RGBAF32, wgt::TextureFormat::Rgba32Float),
        (ImageFormat::RGBAI32, wgt::TextureFormat::Rgba32Sint),
    ];
    let mut count = 0;
    let mut last_id = 0;
    for kind in [ImageBufferKind::Texture2D, ImageBufferKind::TextureRect] {
        for (format, native) in formats {
            let result = store.create(kind, format, size, TextureFilter::Nearest, None);
            if !device.features().contains(native.required_features()) {
                assert!(result.is_err());
                continue;
            }
            let mut handle = result.unwrap();
            count += 1;
            assert!(handle.id > last_id);
            last_id = handle.id;
            assert_eq!(handle.get_dimensions(), size);
            assert_eq!(handle.get_format(), format);
            assert_eq!(handle.get_target(), kind);
            assert_eq!(handle.get_filter(), TextureFilter::Nearest);
            assert_eq!(handle.last_frame_used(), frame);
            assert!(!handle.supports_depth());
            assert!(handle.flags().is_empty());
            assert_eq!(
                handle.size_in_bytes(),
                6 * format.bytes_per_pixel() as usize
            );
            let image = store.image(&handle).unwrap();
            assert_eq!(image.format(), native);
            assert_eq!((image.size().width, image.size().height), (2, 3));
            let weak = Rc::downgrade(&image);
            store.delete(&mut handle).unwrap();
            assert_eq!(handle.id, 0);
            assert!(store.image(&handle).is_err());
            assert!(weak.upgrade().is_some());
            drop(image);
            assert!(weak.upgrade().is_none());
            store.delete(&mut handle).unwrap();
        }
    }
    assert!(store.entries.is_empty());
    assert_eq!(store.created(), count);
    assert_eq!(store.deleted(), count);
    assert_eq!(store.depth_bytes(), 0);
    store.begin_frame(GpuFrameId::new(8));
    assert_eq!((store.created(), store.deleted()), (0, 0));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn render_target_handles_toggle_depth_and_retain_pending_images() {
    let device = device();
    let mut store = TextureStore::new(&device);
    store.begin_frame(GpuFrameId::new(1));
    let mut handle = store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(2, 2),
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: false }),
        )
        .unwrap();
    let id = handle.target_id;
    assert_eq!(store.created(), 1);
    assert!(store.render_target(id, true).is_err());
    let color = store.image(&handle).unwrap();
    store.begin_frame(GpuFrameId::new(2));
    store
        .reuse_render_target(&mut handle, RenderTargetInfo { has_depth: true })
        .unwrap();
    assert!(handle.supports_depth());
    assert_eq!(handle.last_frame_used(), GpuFrameId::new(2));
    assert_eq!(store.created(), 0);
    assert_eq!(store.depth_bytes(), 16);
    let descriptor = DrawTarget::from_texture(&handle, true);
    let (render_color, depth) = match descriptor {
        DrawTarget::Texture {
            texture,
            with_depth,
            ..
        } => store.render_target(texture, with_depth).unwrap(),
        _ => unreachable!(),
    };
    assert!(Rc::ptr_eq(&color, &render_color));
    let depth = depth.unwrap();
    assert_eq!(depth.format(), wgt::TextureFormat::Depth32Float);
    assert_eq!(depth.size(), color.size());
    store
        .reuse_render_target(&mut handle, RenderTargetInfo { has_depth: true })
        .unwrap();
    assert!(Rc::ptr_eq(
        &depth,
        &store.render_target(id, true).unwrap().1.unwrap()
    ));
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
    DrawPass {
            viewport: None,
        target: &render_color,
        origin: DeviceIntPoint::zero(),
        depth: Some(&depth),
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    }
    .clear_rect(
        &mut queue.recording().unwrap(),
        rect,
        Some([0.0, 1.0, 0.0, 1.0]),
        Some(0.25),
    )
    .unwrap();
    let weak_depth = Rc::downgrade(&depth);
    drop(depth);
    drop(render_color);
    store
        .reuse_render_target(&mut handle, RenderTargetInfo { has_depth: false })
        .unwrap();
    assert!(!handle.supports_depth());
    assert!(store.render_target(id, true).is_err());
    assert_eq!(store.depth_bytes(), 0);
    assert!(weak_depth.upgrade().is_some());
    store
        .reuse_render_target(&mut handle, RenderTargetInfo { has_depth: true })
        .unwrap();
    let new_depth = store.render_target(id, true).unwrap().1.unwrap();
    assert!(!Rc::ptr_eq(&new_depth, &weak_depth.upgrade().unwrap()));
    assert!(!new_depth.initialized());
    store.delete(&mut handle).unwrap();
    assert_eq!(store.deleted(), 1);
    assert_eq!(store.depth_bytes(), 0);
    assert!(store.render_target(id, false).is_err());
    queue.wait().unwrap();
    assert!(weak_depth.upgrade().is_none());
    assert_eq!(
        color.readback(rect).unwrap().wait().unwrap(),
        [0, 255, 0, 255].repeat(4)
    );
    let mut replacement = store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(2, 2),
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: true }),
        )
        .unwrap();
    assert_ne!(replacement.target_id, id);
    assert!(store.render_target(id, false).is_err());
    store.delete(&mut replacement).unwrap();
    assert_eq!((store.created(), store.deleted()), (1, 2));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_handles_reject_invalid_requests_without_changing_accounting() {
    let device = device();
    let mut store = TextureStore::new(&device);
    for size in [
        DeviceIntSize::new(0, 1),
        DeviceIntSize::new(1, -1),
        DeviceIntSize::new(i32::MAX, 1),
    ] {
        assert!(store
            .create(
                ImageBufferKind::Texture2D,
                ImageFormat::RGBA8,
                size,
                TextureFilter::Nearest,
                None
            )
            .is_err());
    }
    assert!(store
        .create(
            ImageBufferKind::TextureExternal,
            ImageFormat::RGBA8,
            DeviceIntSize::new(1, 1),
            TextureFilter::Nearest,
            None
        )
        .is_err());
    assert_eq!(store.last_id, 0);
    assert_eq!((store.created(), store.deleted()), (0, 0));
    let mut handle = store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(4, 4),
            TextureFilter::Trilinear,
            None,
        )
        .unwrap();
    assert_eq!(store.image(&handle).unwrap().mip_count(), 3);
    assert!(store.image(&handle).unwrap().target_view().is_some());
    assert!(store.render_target(handle.target_id, false).is_err());
    assert!(store
        .reuse_render_target(&mut handle, RenderTargetInfo { has_depth: true })
        .is_err());
    assert!(handle.render_target.is_none());
    assert!(store.render_target(TextureId(u64::MAX), false).is_err());
    assert!(store.render_target(TextureId(0), false).is_err());
    let saved = handle.target_id;
    handle.target_id = TextureId(0);
    assert!(store.image(&handle).is_err());
    assert!(store.delete(&mut handle).is_err());
    handle.target_id = saved;
    store.last_id = u32::MAX;
    assert!(store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(1, 1),
            TextureFilter::Nearest,
            None
        )
        .is_err());
    assert_eq!((store.created(), store.deleted()), (1, 0));
    store.delete(&mut handle).unwrap();
    assert_eq!((store.created(), store.deleted()), (1, 1));
    assert!(store.entries.is_empty());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
