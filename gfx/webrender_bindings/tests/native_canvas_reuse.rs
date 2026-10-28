/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{native_canvas_map::Gate, unified};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use webrender::vulkan::{Device as GraphicsDevice, Submission};
use wgpu_bindings::{
    canvas::{NativeCanvases, SealedCanvasFrame},
    unified::ProcessOwner,
    NativeCanvasSnapshotKind,
};
use wgpu_core::{
    device::{queue::Queue, Device},
    resource::Texture,
};
use wgpu_core_remote::{global::Global, id::*};
use wgpu_core_remote_types::TextureDescriptor;
use wgpu_types as wgt;

fn device_id() -> DeviceId {
    DeviceId::zip(1, 1)
}
fn queue_id() -> QueueId {
    QueueId::zip(1, 1)
}
const CANVAS: u64 = 501;
const POISON: [u8; 24] = [
    17, 31, 53, 255, 71, 113, 19, 128, 3, 5, 7, 64, 29, 41, 61, 192, 83, 97, 101, 255, 11, 13, 23, 32,
];

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
        view_formats: vec![format.add_srgb_suffix()],
    }
}
fn poll(device: &Arc<Device>, index: Option<u64>) {
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
fn retire_graphics(graphics: &Rc<GraphicsDevice>) {
    let mut tail = Submission::new(graphics).unwrap();
    drop(tail.recording().unwrap());
    if tail.submit().is_err() || !matches!(tail.wait(Some(Duration::from_secs(30))), Ok(true)) {
        std::process::exit(88);
    }
}
fn configure(native: &NativeCanvases, global: &Global, owner: u64, desc: &TextureDescriptor<'_>) {
    let info = native
        .configure(global, owner, device_id(), queue_id(), desc, false)
        .unwrap();
    native.configured(owner, info.generation, true);
}
fn write(queue: &Arc<Queue>, texture: &Arc<Texture>, bytes: &[u8], partial: bool) {
    let size = if partial {
        wgt::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        }
    } else {
        texture.descriptor().size
    };
    queue.write_texture(
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: if partial {
                wgt::Origin3d { x: 1, y: 1, z: 0 }
            } else {
                wgt::Origin3d::ZERO
            },
            aspect: wgt::TextureAspect::All,
        },
        bytes,
        &wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.width * 4),
            rows_per_image: None,
        },
        &size,
    );
}
fn current_bytes(native: &NativeCanvases, global: &Global, owner: u64, id: BufferId, width: usize) -> Vec<u8> {
    assert!(native.create_snapshot_buffer(global, owner, id, 512));
    let index = native.copy_current_to_buffer(global, owner, id, 256);
    assert_ne!(index, 0);
    let device = global.resolve_device_id(device_id());
    poll(&device, Some(index));
    let buffer = global.resolve_buffer_id(id);
    let result = Arc::new(Mutex::new(None));
    let callback = result.clone();
    global.buffer_map_async(
        id,
        wgpu_core_remote_types::MapMode::READ,
        0,
        Some(512),
        Some(Box::new(move |value| *callback.lock().unwrap() = Some(value))),
    );
    poll(&device, Some(index));
    assert!(result.lock().unwrap().take().unwrap().is_ok());
    let bytes = {
        let mapped = buffer.get_mapped_range(0, Some(512)).unwrap();
        let data = mapped.read_slice();
        [data[..width * 4].to_vec(), data[256..256 + width * 4].to_vec()].concat()
    };
    global.buffer_unmap(id);
    buffer.destroy();
    global.buffer_remove(id);
    bytes
}
fn ordinary_copy_rejected(device: &Arc<Device>, queue: &Arc<Queue>, texture: &Arc<Texture>) {
    let buffer = device.create_buffer(&wgpu_core::resource::BufferDescriptor {
        label: None,
        size: 512,
        usage: wgt::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
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
            layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: None,
            },
        },
        &texture.descriptor().size,
    );
    queue.submit(&[encoder.finish(&Default::default())]);
    assert!(device.pop_error_scope().unwrap().is_some());
    buffer.destroy();
}
fn publish(native: &NativeCanvases, id: TextureId, remote: u64) -> Arc<SealedCanvasFrame> {
    let publication = native.prepare_present(CANVAS, id, remote).unwrap();
    let frame = publication.frame.as_ref().unwrap().clone();
    native.presented(CANVAS, publication, true);
    frame
}
fn returned(native: &NativeCanvases, graphics: &Rc<GraphicsDevice>) {
    native.maintain_returns();
    retire_graphics(graphics);
    native.maintain_returns();
}
fn finish(owner: Arc<ProcessOwner>, native: NativeCanvases, global: Global, graphics: Rc<GraphicsDevice>) {
    native.drop_configuration(CANVAS);
    poll(&global.resolve_device_id(device_id()), None);
    native.shutdown_returns();
    drop((native, global, graphics));
    owner.begin_shutdown();
    if owner.context().drain().is_err() {
        std::process::exit(88);
    }
}
fn roundtrip(format: wgt::TextureFormat, render: bool, partial: bool) {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let queue = global.resolve_queue_id(queue_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    let desc = descriptor(format, render, 3);
    configure(&native, &global, CANVAS, &desc);
    let first_id = TextureId::zip(900, 1);
    native.create_current(&global, CANVAS, device_id(), first_id);
    let first = global.resolve_texture_id(first_id);
    write(&queue, &first, &POISON, false);
    assert_eq!(
        current_bytes(&native, &global, CANVAS, BufferId::zip(900, 1), 3),
        POISON
    );
    let frame = publish(&native, first_id, 1);
    poll(&device, Some(frame.submission_index()));
    let epoch = frame.epoch().clone();
    let before = owner.graphics().device.get_internal_counters().texture_memory.read();
    let weak = Arc::downgrade(&frame);
    drop(frame);
    global.texture_remove(first_id);
    drop(first);
    returned(&native, &graphics);
    assert!(weak.upgrade().is_none());
    assert!(epoch.is_closed());
    assert_eq!(native.live_allocations(), 1);
    let second_id = TextureId::zip(901, 1);
    native.create_current(&global, CANVAS, device_id(), second_id);
    let second = global.resolve_texture_id(second_id);
    assert_eq!(second.descriptor().usage, desc.usage);
    assert!(!second.descriptor().usage.contains(wgt::TextureUsages::COPY_SRC));
    ordinary_copy_rejected(&device, &queue, &second);
    let mut expected = vec![0; 24];
    if partial {
        write(&queue, &second, &[7, 11, 19, 31], true);
        expected[16..20].copy_from_slice(&[7, 11, 19, 31]);
    }
    assert_eq!(
        current_bytes(&native, &global, CANVAS, BufferId::zip(901, 1), 3),
        expected
    );
    assert_eq!(native.snapshot_kind(CANVAS), NativeCanvasSnapshotKind::Writing);
    let next = publish(&native, second_id, 2);
    poll(&device, Some(next.submission_index()));
    assert!(!Arc::ptr_eq(&epoch, next.epoch()));
    assert!(epoch.ensure_open().is_err());
    assert!(next.epoch().ensure_open().is_ok());
    assert!(Arc::ptr_eq(epoch.lease(), next.epoch().lease()));
    assert_eq!(native.live_allocations(), 1);
    assert_eq!(
        owner.graphics().device.get_internal_counters().texture_memory.read(),
        before
    );
    drop((next, epoch, second, queue, device));
    global.texture_remove(second_id);
    finish(owner, native, global, graphics);
}
fn held_references() {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    configure(
        &native,
        &global,
        CANVAS,
        &descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3),
    );
    let id = TextureId::zip(902, 1);
    native.create_current(&global, CANVAS, device_id(), id);
    assert!(native.destroy_texture(id));
    let snapshot = native.snapshot_frame(CANVAS).unwrap();
    let latest = publish(&native, id, 1);
    let epoch = latest.epoch().clone();
    let payload = epoch.acquire_payload().unwrap();
    let weak = Arc::downgrade(&latest);
    native.maintain_returns();
    assert!(!epoch.is_closed());
    drop(latest);
    native.maintain_returns();
    assert!(!epoch.is_closed());
    drop(snapshot);
    assert!(weak.upgrade().is_none());
    native.maintain_returns();
    assert!(!epoch.is_closed());
    drop(payload);
    returned(&native, &graphics);
    assert!(epoch.is_closed());
    assert!(epoch.acquire_payload().is_err());
    global.texture_remove(id);
    drop(epoch);
    finish(owner, native, global, graphics);
}
fn gated_never_sampled() {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    configure(
        &native,
        &global,
        CANVAS,
        &descriptor(wgt::TextureFormat::Bgra8Unorm, false, 3),
    );
    let mut gate = Gate::new(&owner);
    let id = TextureId::zip(903, 1);
    native.create_current(&global, CANVAS, device_id(), id);
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frame = publish(&native, id, 1);
        let epoch = frame.epoch().clone();
        let index = frame.submission_index();
        let weak = Arc::downgrade(&frame);
        drop(frame);
        native.maintain_returns();
        let closed = epoch.is_closed();
        let mut probe = Submission::new(&graphics).unwrap();
        drop(probe.recording().unwrap());
        if probe.submit().is_err() {
            std::process::exit(88);
        }
        let blocked = probe.wait(Some(Duration::from_millis(20)));
        let no_frame = weak.upgrade().is_none();
        let no_return = global
            .resolve_queue_id(queue_id())
            .try_take_returned_shared_image(&epoch);
        (
            epoch,
            index,
            closed,
            no_return,
            native.live_allocations(),
            probe,
            blocked,
            no_frame,
        )
    }));
    let explicit = gate.release();
    assert!(explicit, "Native reuse gate required fallback release");
    let (epoch, index, closed, no_return, allocations, mut probe, blocked, no_frame) = match observed {
        Ok(observed) => observed,
        Err(error) => std::panic::resume_unwind(error),
    };
    poll(&device, Some(index));
    if !matches!(probe.wait(Some(Duration::from_secs(30))), Ok(true)) {
        std::process::exit(88);
    }
    returned(&native, &graphics);
    assert!(matches!(blocked, Ok(false)));
    assert!(closed && no_frame);
    assert!(matches!(no_return, Ok(None)));
    assert_eq!(allocations, 1);
    let next_id = TextureId::zip(904, 1);
    native.create_current(&global, CANVAS, device_id(), next_id);
    assert_eq!(
        current_bytes(&native, &global, CANVAS, BufferId::zip(904, 1), 3),
        vec![0; 24]
    );
    let next = publish(&native, next_id, 2);
    poll(&device, Some(next.submission_index()));
    assert!(Arc::ptr_eq(epoch.lease(), next.epoch().lease()));
    global.texture_remove(id);
    global.texture_remove(next_id);
    drop((next, epoch, device, gate, probe));
    finish(owner, native, global, graphics);
}
fn different_logical_producer() {
    let owner = unified::owner();
    let first = unified::actor(&owner, false);
    let second = unified::actor(&owner, false);
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    let desc = descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3);
    configure(&native, &first, CANVAS, &desc);
    let id = TextureId::zip(950, 1);
    native.create_current(&first, CANVAS, device_id(), id);
    let frame = publish(&native, id, 1);
    poll(&first.resolve_device_id(device_id()), Some(frame.submission_index()));
    let epoch = frame.epoch().clone();
    drop(frame);
    first.texture_remove(id);
    returned(&native, &graphics);
    configure(&native, &second, 502, &desc);
    native.create_current(&second, 502, device_id(), id);
    assert_eq!(
        current_bytes(&native, &second, 502, BufferId::zip(950, 1), 3),
        vec![0; 24]
    );
    let publication = native.prepare_present(502, id, 1).unwrap();
    let frame = publication.frame.as_ref().unwrap().clone();
    native.presented(502, publication, true);
    poll(&second.resolve_device_id(device_id()), Some(frame.submission_index()));
    assert!(!Arc::ptr_eq(epoch.lease(), frame.epoch().lease()));
    assert_eq!(native.live_allocations(), 2);
    native.drop_configuration(CANVAS);
    drop(epoch);
    assert_eq!(native.live_allocations(), 1);
    native.drop_configuration(502);
    drop(frame);
    second.texture_remove(id);
    poll(&second.resolve_device_id(device_id()), None);
    native.shutdown_returns();
    assert_eq!(native.live_allocations(), 0);
    drop(second);
    finish(owner, native, first, graphics);
}
fn incompatible_owners_and_quota() {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    let desc = descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3);
    configure(&native, &global, CANVAS, &desc);
    let first_id = TextureId::zip(1000, 1);
    native.create_current(&global, CANVAS, device_id(), first_id);
    let frame = publish(&native, first_id, 1);
    poll(&device, Some(frame.submission_index()));
    let old = Arc::downgrade(frame.epoch().lease());
    drop(frame);
    global.texture_remove(first_id);
    returned(&native, &graphics);
    assert!(old.upgrade().is_some());
    for index in 0..127u32 {
        let id = 1000 + u64::from(index);
        configure(&native, &global, id, &desc);
        native.create_current(&global, id, device_id(), TextureId::zip(1100 + index, 1));
    }
    assert_eq!(native.live_allocations(), 128);
    assert!(old.upgrade().is_some());
    let different = descriptor(wgt::TextureFormat::Bgra8Unorm, false, 4);
    configure(&native, &global, 2000, &different);
    native.create_current(&global, 2000, device_id(), TextureId::zip(1300, 1));
    assert_eq!(native.snapshot_kind(2000), NativeCanvasSnapshotKind::Writing);
    assert_eq!(native.live_allocations(), 128);
    assert!(old.upgrade().is_none(), "Incompatible Ready allocation was not evicted");
    configure(&native, &global, 2001, &desc);
    device.push_error_scope(wgt::error::ErrorFilter::OutOfMemory);
    native.create_current(&global, 2001, device_id(), TextureId::zip(1301, 1));
    assert!(device.pop_error_scope().unwrap().is_some());
    assert_eq!(native.snapshot_kind(2001), NativeCanvasSnapshotKind::Error);
    assert_eq!(native.live_allocations(), 128);
    let independent = NativeCanvases::new(owner.clone());
    configure(&independent, &global, 3000, &desc);
    independent.create_current(&global, 3000, device_id(), TextureId::zip(1400, 1));
    assert_eq!(independent.live_allocations(), 1);
    assert_eq!(native.live_allocations(), 128);
    independent.drop_configuration(3000);
    poll(&device, None);
    independent.shutdown_returns();
    assert_eq!(independent.live_allocations(), 0);
    assert_eq!(native.live_allocations(), 128);
    drop(independent);
    for index in 0..127u32 {
        native.drop_configuration(1000 + u64::from(index));
        global.texture_remove(TextureId::zip(1100 + index, 1));
    }
    for (id, texture) in [(2000, 1300), (2001, 1301)] {
        native.drop_configuration(id);
        global.texture_remove(TextureId::zip(texture, 1));
    }
    global.texture_remove(TextureId::zip(1400, 1));
    poll(&device, None);
    native.shutdown_returns();
    assert_eq!(native.live_allocations(), 0);
    drop(device);
    finish(owner, native, global, graphics);
}
fn destroyed_logical_device() {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    configure(
        &native,
        &global,
        CANVAS,
        &descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3),
    );
    let id = TextureId::zip(1500, 1);
    native.create_current(&global, CANVAS, device_id(), id);
    let frame = publish(&native, id, 1);
    poll(&device, Some(frame.submission_index()));
    let epoch = frame.epoch().clone();
    drop(frame);
    global.texture_remove(id);
    returned(&native, &graphics);
    assert!(epoch.is_closed());
    device.destroy();
    let before = device.get_hal_counters().textures.read();
    assert!(global
        .resolve_queue_id(queue_id())
        .try_take_returned_shared_image(&epoch)
        .is_err());
    assert_eq!(device.get_hal_counters().textures.read(), before);
    assert!(!device.is_valid());
    assert!(owner.is_ready());
    native.drop_configuration(CANVAS);
    native.shutdown_returns();
    drop((epoch, device));
    finish(owner, native, global, graphics);
}
fn expired_producer() {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    configure(
        &native,
        &global,
        CANVAS,
        &descriptor(wgt::TextureFormat::Bgra8Unorm, true, 3),
    );
    let id = TextureId::zip(1501, 1);
    native.create_current(&global, CANVAS, device_id(), id);
    let frame = publish(&native, id, 1);
    poll(&device, Some(frame.submission_index()));
    let epoch = frame.epoch().clone();
    let producer = Arc::downgrade(&device);
    drop(frame);
    global.texture_remove(id);
    drop((device, global));
    assert!(producer.upgrade().is_none());
    native.maintain_returns();
    retire_graphics(&graphics);
    native.maintain_returns();
    let endpoint = owner.graphics();
    assert!(matches!(
        epoch.try_take_returned(
            endpoint.device.as_ref(),
            endpoint.queue.as_ref(),
            endpoint.queue.as_ref()
        ),
        Err(wgpu_hal::shared_image::ImageLeaseError::ProducerGone)
    ));
    native.drop_configuration(CANVAS);
    native.shutdown_returns();
    drop((epoch, endpoint));
    assert_eq!(native.live_allocations(), 0);
    drop((native, graphics));
    owner.begin_shutdown();
    if owner.context().drain().is_err() {
        std::process::exit(88);
    }
}
fn closed_generation() {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    let desc = descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3);
    configure(&native, &global, CANVAS, &desc);
    let id = TextureId::zip(1502, 1);
    native.create_current(&global, CANVAS, device_id(), id);
    let frame = publish(&native, id, 1);
    poll(&device, Some(frame.submission_index()));
    let epoch = frame.epoch().clone();
    drop(frame);
    global.texture_remove(id);
    returned(&native, &graphics);
    if owner.context().drain().is_err() {
        std::process::exit(88);
    }
    assert!(device.is_valid());
    assert!(!owner.is_ready());
    let endpoint = owner.graphics();
    assert!(matches!(
        epoch.try_take_returned(
            endpoint.device.as_ref(),
            endpoint.queue.as_ref(),
            endpoint.queue.as_ref()
        ),
        Err(wgpu_hal::shared_image::ImageLeaseError::Closed)
    ));
    let before = device.get_hal_counters().textures.read();
    let allocator = wgpu_bindings::canvas::NativeCanvasAllocator::new(owner.clone());
    assert!(allocator.create_texture(&device, &desc).is_err());
    assert_eq!(device.get_hal_counters().textures.read(), before);
    assert_eq!(allocator.live_allocations(), 0);
    native.drop_configuration(CANVAS);
    native.shutdown_returns();
    drop((epoch, endpoint, device, allocator));
    finish(owner, native, global, graphics);
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_reuse() {
    for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm] {
        for render in [false, true] {
            for partial in [false, true] {
                roundtrip(format, render, partial);
            }
        }
    }
    held_references();
    gated_never_sampled();
    different_logical_producer();
    incompatible_owners_and_quota();
    destroyed_logical_device();
    expired_producer();
    closed_generation();
}

fn unretired_return_child() -> ! {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(device_id());
    let graphics = unified::graphics(&owner);
    let native = NativeCanvases::new(owner.clone());
    configure(
        &native,
        &global,
        CANVAS,
        &descriptor(wgt::TextureFormat::Rgba8Unorm, true, 3),
    );
    let id = TextureId::zip(1600, 1);
    native.create_current(&global, CANVAS, device_id(), id);
    let frame = publish(&native, id, 1);
    poll(&device, Some(frame.submission_index()));
    let epoch = frame.epoch().clone();
    drop(frame);
    global.texture_remove(id);
    native.maintain_returns();
    assert!(epoch.is_closed());
    retire_graphics(&graphics);
    assert_eq!(native.live_allocations(), 1);
    drop(epoch);
    eprintln!("NATIVE_CANVAS_UNPOLLED_MARKER_DROP: GPU complete; actor receipt unretired");
    drop(native);
    eprintln!("NATIVE_CANVAS_UNPOLLED_MARKER_DROP: unexpectedly returned");
    std::process::exit(87);
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_unretired_return() {
    const CHILD: &str = "WR_NATIVE_CANVAS_UNRETIRED_RETURN_CHILD";
    if std::env::var_os(CHILD).is_some() {
        unretired_return_child();
    }
    let path = std::env::temp_dir().join(format!("native-canvas-unretired-return-{}.log", std::process::id()));
    let log = std::fs::File::create(&path).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["-unittest", "--gtest_death_test_style=threadsafe"])
        .env("MOZ_RUN_GTEST", "1")
        .env(
            "GTEST_FILTER",
            "RenderExternalBuffer.DISABLED_VulkanNativeCanvasUnretiredReturn",
        )
        .env("GTEST_ALSO_RUN_DISABLED_TESTS", "1")
        .env("GTEST_SHUFFLE", "0")
        .env("GTEST_REPEAT", "1")
        .env_remove("GTEST_SHARD_INDEX")
        .env_remove("GTEST_TOTAL_SHARDS")
        .env_remove("GTEST_SHARD_STATUS_FILE")
        .env_remove("GTEST_OUTPUT")
        .env(CHILD, "1")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = std::fs::read_to_string(&path).unwrap();
    eprintln!(
        "Native canvas unretired-return child status {:?}; full log: {}",
        status.code(),
        path.display()
    );
    assert!(!timed_out, "Unretired return child exceeded bounded termination");
    assert_eq!(status.code(), Some(88));
    assert!(output.contains("NATIVE_CANVAS_UNPOLLED_MARKER_DROP: GPU complete; actor receipt unretired"));
    assert!(!output.contains("NATIVE_CANVAS_UNPOLLED_MARKER_DROP: unexpectedly returned"));
    for diagnostic in ["VUID-", "SYNC-HAZARD-", "Validation Error", "UNASSIGNED-"] {
        assert!(
            !output.contains(diagnostic),
            "Child native validation diagnostic: {diagnostic}"
        );
    }
}
