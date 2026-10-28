/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{
    native_canvas::{DrawContext, BACKGROUND, PIXELS},
    unified,
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use webrender::api::{units::*, ExternalTextureHandle, ImageFormat};
use webrender::vulkan::{Submission, Texture, TextureFilter};
use wgpu_bindings::{
    canvas::{NativeCanvasAllocator, NativeCanvasFrame, SealedCanvasFrame},
    unified::ProcessOwner,
};
use wgpu_core::{
    device::{queue::Queue, Device},
    resource::{Buffer, SharedCanvasTexture, Texture as CoreTexture, TextureDescriptor},
};
use wgpu_core_remote::{global::Global, id::*};
use wgpu_hal::shared_image::{AllocationBudget, ImageLease};
use wgpu_types as wgt;

const PITCH: u32 = 256;
const BUFFER_BYTES: u64 = 512;

fn descriptor(format: wgt::TextureFormat, render: bool, width: u32) -> TextureDescriptor<'static> {
    TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format,
        usage: wgt::TextureUsages::COPY_DST
            | if render {
                wgt::TextureUsages::RENDER_ATTACHMENT
            } else {
                wgt::TextureUsages::empty()
            },
        view_formats: Vec::new(),
    }
}

fn counts(owner: &Arc<ProcessOwner>) -> (isize, isize) {
    let graphics = owner.graphics();
    let counters = graphics.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

fn poll(device: &Arc<Device>, index: Option<wgpu_core::SubmissionIndex>) {
    if device
        .poll(wgt::PollType::Wait {
            submission_index: index,
            timeout: Some(Duration::from_secs(30)),
        })
        .is_err()
    {
        std::process::exit(88);
    }
}

fn readback(device: &Arc<Device>, bytes: u64, usage: wgt::BufferUsages) -> Arc<Buffer> {
    device.create_buffer(&wgpu_core::resource::BufferDescriptor {
        label: None,
        size: bytes,
        usage,
        mapped_at_creation: false,
    })
}

fn layout() -> wgt::TexelCopyBufferLayout {
    wgt::TexelCopyBufferLayout {
        offset: 0,
        bytes_per_row: Some(PITCH),
        rows_per_image: None,
    }
}

fn copied_bytes(device: &Arc<Device>, queue: &Arc<Queue>, canvas: &SharedCanvasTexture) -> Vec<u8> {
    let buffer = readback(
        device,
        BUFFER_BYTES,
        wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
    );
    let index = unsafe { queue.copy_shared_canvas_to_buffer(canvas, &buffer, layout()) }.unwrap();
    poll(device, Some(index));
    let mapped = Arc::new(Mutex::new(None));
    let callback = mapped.clone();
    buffer.map_async(
        wgt::MapMode::Read,
        0,
        Some(BUFFER_BYTES),
        Some(Box::new(move |result| *callback.lock().unwrap() = Some(result))),
    );
    poll(device, Some(index));
    assert!(mapped.lock().unwrap().take().unwrap().is_ok());
    let output = {
        let mapping = buffer.get_mapped_range(0, Some(BUFFER_BYTES)).unwrap();
        let bytes = mapping.read_slice();
        [
            bytes[..12].to_vec(),
            bytes[PITCH as usize..PITCH as usize + 12].to_vec(),
        ]
        .concat()
    };
    buffer.unmap();
    buffer.destroy();
    output
}

fn write(queue: &Arc<Queue>, texture: &Arc<CoreTexture>, pixels: &[u8], origin: wgt::Origin3d, size: wgt::Extent3d) {
    queue.write_texture(
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin,
            aspect: wgt::TextureAspect::All,
        },
        pixels,
        &wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.width * 4),
            rows_per_image: None,
        },
        &size,
    );
}

fn ordinary_copy_rejected(device: &Arc<Device>, queue: &Arc<Queue>, texture: &Arc<CoreTexture>) {
    let buffer = readback(
        device,
        BUFFER_BYTES,
        wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
    );
    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    let encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        &wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &wgt::TexelCopyBufferInfo {
            buffer: buffer.clone(),
            layout: layout(),
        },
        &texture.descriptor().size,
    );
    queue.submit(&[encoder.finish(&Default::default())]);
    assert!(device.pop_error_scope().unwrap().is_some());
    buffer.destroy();
}

fn invalid_copies(device: &Arc<Device>, queue: &Arc<Queue>, canvas: &SharedCanvasTexture) {
    let wrong_usage = readback(device, BUFFER_BYTES, wgt::BufferUsages::COPY_SRC);
    let short = readback(device, 4, wgt::BufferUsages::COPY_DST);
    let valid = readback(device, BUFFER_BYTES, wgt::BufferUsages::COPY_DST);
    assert!(unsafe { queue.copy_shared_canvas_to_buffer(canvas, &wrong_usage, layout()) }.is_err());
    assert!(unsafe { queue.copy_shared_canvas_to_buffer(canvas, &short, layout()) }.is_err());
    for invalid in [
        wgt::TexelCopyBufferLayout {
            bytes_per_row: Some(8),
            ..layout()
        },
        wgt::TexelCopyBufferLayout {
            offset: u64::MAX,
            ..layout()
        },
    ] {
        assert!(unsafe { queue.copy_shared_canvas_to_buffer(canvas, &valid, invalid) }.is_err());
    }
    wrong_usage.destroy();
    short.destroy();
    valid.destroy();
}

fn budget_proof(owner: &Arc<ProcessOwner>) {
    let first = unified::actor(owner, false);
    let second = unified::actor(owner, false);
    let devices = [
        first.resolve_device_id(DeviceId::zip(1, 1)),
        second.resolve_device_id(DeviceId::zip(1, 1)),
    ];
    let queues = [
        first.resolve_queue_id(QueueId::zip(1, 1)),
        second.resolve_queue_id(QueueId::zip(1, 1)),
    ];
    let baseline = counts(owner);
    let budget = Arc::new(AllocationBudget::new(2));
    assert_eq!(budget.capacity(), 2);
    let mut allocations = Vec::new();
    for (index, format) in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm]
        .iter()
        .copied()
        .enumerate()
    {
        let (texture, canvas) = unsafe {
            devices[index].create_shared_canvas_texture(
                &descriptor(format, index == 0, 3),
                owner.graphics(),
                budget.clone(),
            )
        };
        allocations.push((texture, canvas.unwrap()));
    }
    assert_eq!(budget.live_allocations(), 2);
    assert_eq!(counts(owner).0, baseline.0 + 2);
    assert!(counts(owner).1 > baseline.1);
    for attempt in 0..24 {
        let device = &devices[attempt % 2];
        let before = counts(owner);
        device.push_error_scope(wgt::error::ErrorFilter::OutOfMemory);
        let (invalid, canvas) = unsafe {
            device.create_shared_canvas_texture(
                &descriptor(
                    if attempt % 2 == 0 {
                        wgt::TextureFormat::Rgba8Unorm
                    } else {
                        wgt::TextureFormat::Bgra8Unorm
                    },
                    attempt % 3 == 0,
                    1 + attempt as u32,
                ),
                owner.graphics(),
                budget.clone(),
            )
        };
        assert!(canvas.is_none());
        assert!(device.pop_error_scope().unwrap().is_some());
        assert!(device.is_valid());
        drop(invalid);
        assert_eq!(counts(owner), before);
        assert_eq!(budget.live_allocations(), 2);
    }
    let (texture, canvas) = allocations.remove(0);
    let sealed = unsafe { queues[0].seal_shared_canvas(canvas) }.unwrap();
    poll(&devices[0], Some(sealed.submission_index()));
    let frame = SealedCanvasFrame::new(owner.clone(), sealed).unwrap();
    let weak = Arc::downgrade(frame.epoch().lease());
    let graphics = owner.graphics();
    let alias = unsafe {
        wgpu_hal::shared_image::create_alias(
            frame.epoch().lease().clone(),
            graphics.device.as_ref(),
            frame.native_descriptor(),
        )
    }
    .unwrap();
    let alias_count = counts(owner);
    drop(texture);
    drop(frame);
    poll(&devices[0], None);
    assert!(weak.upgrade().is_some());
    assert_eq!(budget.live_allocations(), 2);
    assert_eq!(counts(owner), alias_count);
    unsafe {
        graphics.device.destroy_texture(alias);
    }
    assert!(weak.upgrade().is_none());
    assert_eq!(budget.live_allocations(), 1);
    let (texture, canvas) = allocations.pop().unwrap();
    let sealed = unsafe { queues[1].seal_shared_canvas(canvas) }.unwrap();
    poll(&devices[1], Some(sealed.submission_index()));
    let frame = SealedCanvasFrame::new(owner.clone(), sealed).unwrap();
    let weak = Arc::downgrade(frame.epoch().lease());
    let wr = unified::graphics(owner);
    let alias = unsafe {
        Texture::from_shared_image(
            &wr,
            frame.epoch().clone(),
            wgt::TextureFormat::Bgra8Unorm,
            TextureFilter::Nearest,
            false,
        )
    }
    .unwrap();
    let mut submission = Submission::new(&wr).unwrap();
    submission.recording().unwrap().keep(&alias);
    submission.submit().unwrap();
    drop(alias);
    drop(texture);
    drop(frame);
    poll(&devices[1], None);
    assert!(weak.upgrade().is_some());
    assert_eq!(budget.live_allocations(), 1);
    if !submission.wait(Some(Duration::from_secs(30))).unwrap_or(false) {
        std::process::exit(88);
    }
    drop(submission);
    drop(wr);
    assert!(weak.upgrade().is_none());
    assert_eq!(budget.live_allocations(), 0);
    assert!(!budget.is_poisoned());
    assert_eq!(counts(owner), baseline);

    let actor = NativeCanvasAllocator::new(owner.clone());
    let other = NativeCanvasAllocator::new(owner.clone());
    let mut retained = Vec::new();
    for index in 0..128 {
        let device = &devices[index % 2];
        let desc = descriptor(
            if index % 2 == 0 {
                wgt::TextureFormat::Rgba8Unorm
            } else {
                wgt::TextureFormat::Bgra8Unorm
            },
            index % 3 == 0,
            1 + (index % 3) as u32,
        );
        let (texture, canvas) = actor.create_texture(device, &desc).unwrap();
        retained.push((texture, canvas.unwrap()));
    }
    assert_eq!(actor.live_allocations(), 128);
    assert_eq!(counts(owner).0, baseline.0 + 128);
    devices[0].push_error_scope(wgt::error::ErrorFilter::OutOfMemory);
    let (invalid, canvas) = actor
        .create_texture(&devices[0], &descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3))
        .unwrap();
    assert!(canvas.is_none());
    assert!(devices[0].pop_error_scope().unwrap().is_some());
    drop(invalid);
    let mut independent = Vec::new();
    for index in 0..2 {
        let (texture, canvas) = other
            .create_texture(&devices[index], &descriptor(wgt::TextureFormat::Rgba8Unorm, false, 2))
            .unwrap();
        independent.push((texture, canvas.unwrap()));
    }
    assert_eq!(actor.live_allocations(), 128);
    assert_eq!(other.live_allocations(), 2);
    let textures = retained
        .into_iter()
        .map(|(texture, canvas)| {
            drop(canvas);
            texture
        })
        .collect::<Vec<_>>();
    assert_eq!(actor.live_allocations(), 128);
    for texture in textures {
        texture.destroy();
    }
    for device in &devices {
        poll(device, None);
    }
    assert_eq!(actor.live_allocations(), 0);
    assert_eq!(other.live_allocations(), 2);
    assert_eq!(counts(owner).0, baseline.0 + 2);
    for (texture, canvas) in independent {
        drop(canvas);
        texture.destroy();
    }
    for device in &devices {
        poll(device, None);
    }
    assert_eq!(other.live_allocations(), 0);
    assert_eq!(counts(owner), baseline);
    drop((queues, devices, first, second));
}

pub struct Fixture {
    owner: Arc<ProcessOwner>,
    global: Option<Global>,
    draw: Option<DrawContext>,
    budget: Arc<AllocationBudget>,
    weak_lease: Option<Weak<dyn ImageLease>>,
    weak_frame: Option<Weak<SealedCanvasFrame>>,
    bgra: bool,
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_new() -> *mut Fixture {
    let owner = unified::owner();
    budget_proof(&owner);
    Box::into_raw(Box::new(Fixture {
        owner,
        global: None,
        draw: None,
        budget: Arc::new(AllocationBudget::new(2)),
        weak_lease: None,
        weak_frame: None,
        bgra: false,
    }))
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_prepare(
    fixture: &mut Fixture,
    bgra: bool,
    render: bool,
) -> *mut NativeCanvasFrame {
    assert!(fixture.global.is_none() && fixture.draw.is_none());
    assert_eq!(fixture.budget.live_allocations(), 0);
    let global = unified::actor(&fixture.owner, false);
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    let queue = global.resolve_queue_id(QueueId::zip(1, 1));
    let format = if bgra {
        wgt::TextureFormat::Bgra8Unorm
    } else {
        wgt::TextureFormat::Rgba8Unorm
    };
    let desc = descriptor(format, render, 3);
    let (texture, canvas) =
        unsafe { device.create_shared_canvas_texture(&desc, fixture.owner.graphics(), fixture.budget.clone()) };
    let canvas = canvas.unwrap();
    assert!(Arc::ptr_eq(canvas.texture(), &texture));
    assert_eq!(texture.descriptor().usage, desc.usage);
    assert!(!texture.descriptor().usage.contains(wgt::TextureUsages::COPY_SRC));
    ordinary_copy_rejected(&device, &queue, &texture);
    invalid_copies(&device, &queue, &canvas);
    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    assert_eq!(copied_bytes(&device, &queue, &canvas), [0; 24]);
    let mut partial = PIXELS[1];
    if bgra {
        partial.swap(0, 2);
    }
    write(
        &queue,
        &texture,
        &partial,
        wgt::Origin3d { x: 1, y: 0, z: 0 },
        wgt::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    let mut expected = vec![0; 24];
    expected[4..8].copy_from_slice(&partial);
    assert_eq!(copied_bytes(&device, &queue, &canvas), expected);
    let bytes = PIXELS
        .iter()
        .flat_map(|pixel| {
            let mut pixel = *pixel;
            if bgra {
                pixel.swap(0, 2);
            }
            pixel
        })
        .collect::<Vec<_>>();
    write(&queue, &texture, &bytes, wgt::Origin3d::ZERO, desc.size);
    let sealed = unsafe { queue.seal_shared_canvas(canvas) }.unwrap();
    poll(&device, Some(sealed.submission_index()));
    assert!(device.pop_error_scope().unwrap().is_none());
    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    write(&queue, &texture, &bytes, wgt::Origin3d::ZERO, desc.size);
    assert!(device.pop_error_scope().unwrap().is_some());
    let frame = SealedCanvasFrame::new(fixture.owner.clone(), sealed).unwrap();
    assert_eq!(frame.logical_descriptor().usage, desc.usage);
    assert!(frame
        .native_descriptor()
        .usage
        .contains(wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC));
    fixture.weak_lease = Some(Arc::downgrade(frame.epoch().lease()));
    fixture.weak_frame = Some(Arc::downgrade(&frame));
    fixture.global = Some(global);
    fixture.draw = Some(DrawContext::new(&fixture.owner));
    fixture.bgra = bgra;
    assert_eq!(fixture.budget.live_allocations(), 1);
    Box::into_raw(Box::new(NativeCanvasFrame::new(frame)))
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_native_canvas_authority_release_frame(frame: *mut NativeCanvasFrame) {
    unsafe { wgpu_bindings::canvas::wgpu_native_canvas_frame_delete(frame) };
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_drop_actor(fixture: &mut Fixture, close_owner: bool) {
    let device = fixture.global.as_ref().unwrap().resolve_device_id(DeviceId::zip(1, 1));
    if close_owner {
        let allocator = NativeCanvasAllocator::new(fixture.owner.clone());
        let before = counts(&fixture.owner);
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        fixture.owner.begin_shutdown();
        assert!(allocator
            .create_texture(&device, &descriptor(wgt::TextureFormat::Rgba8Unorm, true, 0))
            .is_err());
        assert!(device.pop_error_scope().unwrap().is_none());
        assert!(device.is_valid());
        assert_eq!(allocator.live_allocations(), 0);
        assert_eq!(counts(&fixture.owner), before);
    }
    device.destroy();
    drop(device);
    drop(fixture.global.take());
    assert_eq!(fixture.owner.is_ready(), !close_owner);
    assert!(fixture.weak_frame.as_ref().unwrap().upgrade().is_some());
    assert_eq!(fixture.budget.live_allocations(), 1);
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_renderer(fixture: &mut Fixture) -> &mut webrender::Renderer {
    fixture.draw.as_mut().unwrap().renderer.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_record(fixture: &mut Fixture, normal: u64, opaque: u64) {
    fixture.draw.as_mut().unwrap().render(
        [ExternalTextureHandle(normal), ExternalTextureHandle(opaque)],
        if fixture.bgra {
            ImageFormat::BGRA8
        } else {
            ImageFormat::RGBA8
        },
    );
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_verify(fixture: &mut Fixture) {
    assert_eq!(fixture.budget.live_allocations(), 1);
    assert!(fixture.weak_lease.as_ref().unwrap().upgrade().is_some());
    let bytes = fixture.draw.as_mut().unwrap().pixels();
    for row in 0..2 {
        for panel in 0..2 {
            for column in 0..3 {
                let pixel = PIXELS[row * 3 + column];
                let offset = (row * 6 + panel * 3 + column) * 4;
                for channel in 0..3 {
                    let background = if panel == 0 {
                        BACKGROUND[channel] as f64 * (255 - pixel[3]) as f64 / 255.
                    } else {
                        0.
                    };
                    let expected = (pixel[channel] as f64 + background).round() as i16;
                    let tolerance = if panel == 0 && pixel[3] != 255 { 1 } else { 0 };
                    assert!((bytes[offset + channel] as i16 - expected).abs() <= tolerance);
                }
                assert_eq!(bytes[offset + 3], 255);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_authority_finish(fixture: &mut Fixture) {
    assert!(fixture.global.is_none());
    fixture.draw.take().unwrap().finish();
    assert!(fixture.weak_frame.take().unwrap().upgrade().is_none());
    assert!(fixture.weak_lease.take().unwrap().upgrade().is_none());
    assert_eq!(fixture.budget.live_allocations(), 0);
    assert!(!fixture.budget.is_poisoned());
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_native_canvas_authority_delete(fixture: *mut Fixture) {
    let mut fixture = Box::from_raw(fixture);
    if let Some(draw) = fixture.draw.take() {
        draw.finish();
    }
    drop(fixture.global.take());
    fixture.owner.begin_shutdown();
    if fixture.owner.context().drain().is_err() {
        std::process::exit(88);
    }
}
