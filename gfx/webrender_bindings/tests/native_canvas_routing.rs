/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::unified;
use std::{
    cell::RefCell,
    ffi::c_void,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use webrender::api::{units::*, *};
use webrender_bindings::bindings::DocumentHandle;
use wgpu_bindings::{
    canvas::{NativeCanvasAllocator, NativeCanvasFrame, NativeCanvases, SealedCanvasFrame},
    CanvasPresentationMode, NativeCanvasSnapshotKind,
};
use wgpu_core_remote::{global::Global, id::*};
use wgpu_core_remote_types::TextureDescriptor;
use wgpu_types as wgt;

const SIZE: DeviceIntSize = DeviceIntSize::new(3, 2);
const A: [[u8; 4]; 6] = super::native_canvas::PIXELS;
const B: [[u8; 4]; 6] = [
    [41, 13, 71, 255],
    [20, 50, 90, 128],
    [15, 3, 29, 64],
    [83, 137, 11, 192],
    [101, 23, 67, 255],
    [31, 11, 19, 64],
];
const BG: [u8; 4] = [23, 47, 89, 255];

fn descriptor(format: wgt::TextureFormat) -> TextureDescriptor<'static> {
    TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 3,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format,
        usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_DST,
        view_formats: vec![format.add_srgb_suffix()],
    }
}

fn bytes(pattern: &[[u8; 4]; 6], bgra: bool) -> Vec<u8> {
    pattern
        .iter()
        .flat_map(|pixel| {
            let mut pixel = *pixel;
            if bgra {
                pixel.swap(0, 2);
            }
            pixel
        })
        .collect()
}

fn poll(device: &Arc<wgpu_core::device::Device>, index: Option<u64>) {
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

fn write(queue: &Arc<wgpu_core::device::queue::Queue>, texture: &Arc<wgpu_core::resource::Texture>, data: &[u8]) {
    queue.write_texture(
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        data,
        &wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(12),
            rows_per_image: None,
        },
        &wgt::Extent3d {
            width: 3,
            height: 2,
            depth_or_array_layers: 1,
        },
    );
}

fn current_bytes(native: &NativeCanvases, global: &Global, owner: u64, buffer_id: BufferId) -> Vec<u8> {
    assert!(native.create_snapshot_buffer(global, owner, buffer_id, 512));
    let index = native.copy_current_to_buffer(global, owner, buffer_id, 256);
    assert!(index != 0);
    if global
        .device_poll(
            DeviceId::zip(1, 1),
            wgt::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(Duration::from_secs(30)),
            },
        )
        .is_err()
    {
        std::process::exit(88);
    }
    let buffer = global.resolve_buffer_id(buffer_id);
    let mapped = Arc::new(Mutex::new(None));
    let callback = mapped.clone();
    global.buffer_map_async(
        buffer_id,
        wgpu_core_remote_types::MapMode::READ,
        0,
        Some(512),
        Some(Box::new(move |result| *callback.lock().unwrap() = Some(result))),
    );
    if global
        .device_poll(
            DeviceId::zip(1, 1),
            wgt::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(Duration::from_secs(30)),
            },
        )
        .is_err()
    {
        std::process::exit(88);
    }
    assert!(mapped.lock().unwrap().take().unwrap().is_ok());
    let output = {
        let range = buffer.get_mapped_range(0, Some(512)).unwrap();
        let data = range.read_slice();
        [data[..12].to_vec(), data[256..268].to_vec()].concat()
    };
    global.buffer_unmap(buffer_id);
    buffer.destroy();
    global.buffer_remove(buffer_id);
    output
}

thread_local! {
    static CALLBACK_STATE: RefCell<Option<Rc<NativeCanvases>>> = const { RefCell::new(None) };
}

/// Private actor-helper proof; no DOM/IPC actor is fabricated or activated.
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_state() {
    for mode in [
        CanvasPresentationMode::Readback,
        CanvasPresentationMode::LegacyShared,
        CanvasPresentationMode::UnifiedNative,
    ] {
        let wire = bincode::serialize(&mode).unwrap();
        assert_eq!(bincode::deserialize::<CanvasPresentationMode>(&wire).unwrap(), mode);
    }
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    let queue = global.resolve_queue_id(QueueId::zip(1, 1));
    let native = Rc::new(NativeCanvases::new(owner.clone()));
    let desc = descriptor(wgt::TextureFormat::Rgba8Unorm);
    let info = native
        .configure(&global, 101, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
        .unwrap();
    native.configured(101, info.generation, true);
    assert_eq!(native.live_allocations(), 0);
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Blank);
    let blank = native.prepare_blank(101, 1).unwrap();
    assert!(blank.texture_id.is_none() && blank.frame.is_none());
    native.presented(101, blank, true);
    assert_eq!(native.live_allocations(), 0);
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Blank);

    let first_id = TextureId::zip(71, 1);
    native.create_current(&global, 101, DeviceId::zip(1, 1), first_id);
    let first = global.resolve_texture_id(first_id);
    assert_eq!(first.descriptor().usage, desc.usage);
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Writing);
    let too_large = device.limits().max_buffer_size.checked_add(4).unwrap();
    let oversized_id = BufferId::zip(701, 1);
    assert!(!native.create_snapshot_buffer(&global, 101, oversized_id, too_large));
    assert!(global.canvas_buffer_id_available(oversized_id));
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Writing);
    assert_eq!(native.live_allocations(), 1);
    assert_eq!(current_bytes(&native, &global, 101, BufferId::zip(702, 1)), vec![0; 24]);
    write(&queue, &first, &bytes(&A, false));
    assert_eq!(
        current_bytes(&native, &global, 101, BufferId::zip(703, 1)),
        bytes(&A, false)
    );
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Writing);
    assert!(native.destroy_texture(first_id));
    global.texture_destroy(first_id);
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Sealed);
    assert!(Arc::ptr_eq(&first, &global.resolve_texture_id(first_id)));
    let sealed_a = native.snapshot_frame(101).unwrap();
    poll(&device, Some(sealed_a.submission_index()));
    let publication = native.prepare_present(101, first_id, 2).unwrap();
    assert!(Arc::ptr_eq(publication.frame.as_ref().unwrap(), &sealed_a));
    native.presented(101, publication, true);

    let second_id = TextureId::zip(72, 1);
    native.create_current(&global, 101, DeviceId::zip(1, 1), second_id);
    let second = global.resolve_texture_id(second_id);
    write(&queue, &second, &bytes(&B, false));
    assert!(native.destroy_device(DeviceId::zip(1, 1)));
    let sealed_b = native.snapshot_frame(101).unwrap();
    poll(&device, Some(sealed_b.submission_index()));
    global.device_destroy(DeviceId::zip(1, 1));
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Published);
    assert!(native.snapshot_frame(101).is_none());
    let publication = native.prepare_present(101, second_id, 3).unwrap();
    assert!(Arc::ptr_eq(publication.frame.as_ref().unwrap(), &sealed_b));
    native.presented(101, publication, true);
    assert_eq!(native.snapshot_kind(101), NativeCanvasSnapshotKind::Published);
    native.drop_configuration(101);
    drop((sealed_a, sealed_b, first, second, queue, device, global, native));

    {
        let global = unified::actor(&owner, false);
        let device = global.resolve_device_id(DeviceId::zip(1, 1));
        let queue = global.resolve_queue_id(QueueId::zip(1, 1));
        let native = NativeCanvases::new(owner.clone());
        let info = native
            .configure(&global, 150, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
            .unwrap();
        native.configured(150, info.generation, true);
        let id = TextureId::zip(75, 1);
        native.create_current(&global, 150, DeviceId::zip(1, 1), id);
        write(&queue, &global.resolve_texture_id(id), &bytes(&A, false));
        drop((queue, device, global));
        assert_eq!(native.live_allocations(), 1);
        native.drop_configuration(150);
        assert_eq!(native.live_allocations(), 0);
    }

    // Config acknowledgements are modeled here; the separate C++ pipeline test
    // uses real Map registration, collision admission and readiness.
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    let queue = global.resolve_queue_id(QueueId::zip(1, 1));
    let native = Rc::new(NativeCanvases::new(owner.clone()));
    let observed = Arc::new(AtomicUsize::new(0));
    CALLBACK_STATE.with(|slot| *slot.borrow_mut() = Some(native.clone()));
    let count = observed.clone();
    device.on_uncaptured_error(Arc::new(move |_: wgt::error::Error| {
        CALLBACK_STATE.with(|slot| {
            assert_eq!(
                slot.borrow().as_ref().unwrap().snapshot_kind(201),
                NativeCanvasSnapshotKind::Unavailable
            );
        });
        count.fetch_add(1, Ordering::SeqCst);
    }));
    let mut empty_usage = desc.clone();
    empty_usage.usage = wgt::TextureUsages::empty();
    let mut zero_width = desc.clone();
    zero_width.size.width = 0;
    let mut oversized = desc.clone();
    oversized.size.width = device.limits().max_texture_dimension_2d + 1;
    for (index, invalid) in [empty_usage, zero_width, oversized].into_iter().enumerate() {
        let before = observed.load(Ordering::SeqCst);
        let info = native
            .configure(&global, 201, DeviceId::zip(1, 1), QueueId::zip(1, 1), &invalid, false)
            .unwrap();
        native.configured(201, info.generation, true);
        native.create_current(&global, 201, DeviceId::zip(1, 1), TextureId::zip(73 + index as u32, 1));
        assert!(observed.load(Ordering::SeqCst) >= before + 2);
        assert_eq!(native.snapshot_kind(201), NativeCanvasSnapshotKind::Error);
        assert!(!native.protocol_error());
        assert_eq!(native.live_allocations(), 0);
        native.drop_configuration(201);
    }
    CALLBACK_STATE.with(|slot| *slot.borrow_mut() = None);
    device.on_uncaptured_error(Arc::new(|error: wgt::error::Error| {
        panic!("Unexpected GPU error: {error}")
    }));

    for removed in 0..2 {
        let owner_id = 250 + removed;
        let texture_id = TextureId::zip(78 + removed as u32, 1);
        let info = native
            .configure(&global, owner_id, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
            .unwrap();
        native.configured(owner_id, info.generation, true);
        native.create_current(&global, owner_id, DeviceId::zip(1, 1), texture_id);
        write(&queue, &global.resolve_texture_id(texture_id), &bytes(&A, false));
        if removed != 0 {
            assert!(native.drop_texture(texture_id));
            global.texture_remove(texture_id);
        }
        native.drop_configuration(owner_id);
        poll(&device, None);
        assert_eq!(native.live_allocations(), 0);
    }

    for scenario in 0..4 {
        let rejected = NativeCanvases::new(owner.clone());
        let info = rejected
            .configure(&global, 270, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
            .unwrap();
        rejected.configured(270, info.generation, true);
        match scenario {
            0 => assert!(rejected
                .configure(&global, 270, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
                .is_none()),
            1 => rejected.create_current(&global, 270, DeviceId::zip(2, 1), TextureId::zip(790, 1)),
            2 => rejected.create_current(&global, 270, DeviceId::zip(1, 1), TextureId::zip(78, 1)),
            3 => {
                let publication = rejected.prepare_blank(270, 1).unwrap();
                rejected.presented(270, publication, false);
            },
            _ => unreachable!(),
        }
        assert!(rejected.protocol_error());
        assert_eq!(rejected.snapshot_kind(270), NativeCanvasSnapshotKind::Unavailable);
        assert_eq!(rejected.live_allocations(), 0);
    }

    let info = native
        .configure(&global, 301, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
        .unwrap();
    native.configured(301, info.generation, true);
    native.create_current(&global, 301, DeviceId::zip(1, 1), TextureId::zip(80, 1));
    let publication = native.prepare_present(301, TextureId::zip(80, 1), 4).unwrap();
    let old = publication.frame.as_ref().unwrap().clone();
    poll(&device, Some(old.submission_index()));
    native.presented(301, publication, true);
    native.drop_configuration(301);
    assert_eq!(native.live_allocations(), 1);
    for index in 0..127 {
        let owner_id = 400 + index;
        let info = native
            .configure(
                &global,
                owner_id,
                DeviceId::zip(1, 1),
                QueueId::zip(1, 1),
                &desc,
                index % 2 == 0,
            )
            .unwrap();
        native.configured(owner_id, info.generation, true);
        native.create_current(
            &global,
            owner_id,
            DeviceId::zip(1, 1),
            TextureId::zip(1000 + index as u32, 1),
        );
    }
    assert_eq!(native.live_allocations(), 128);
    let info = native
        .configure(&global, 600, DeviceId::zip(1, 1), QueueId::zip(1, 1), &desc, true)
        .unwrap();
    native.configured(600, info.generation, true);
    device.push_error_scope(wgt::error::ErrorFilter::OutOfMemory);
    native.create_current(&global, 600, DeviceId::zip(1, 1), TextureId::zip(1200, 1));
    assert!(device.pop_error_scope().unwrap().is_some());
    assert_eq!(native.snapshot_kind(600), NativeCanvasSnapshotKind::Error);
    let publication = native.prepare_present(600, TextureId::zip(1200, 1), 5).unwrap();
    assert!(publication.frame.is_none());
    native.presented(600, publication, true);
    assert_eq!(native.snapshot_kind(600), NativeCanvasSnapshotKind::Blank);
    assert!(native.destroy_device(DeviceId::zip(1, 1)));
    device.destroy();
    assert_eq!(native.snapshot_kind(600), NativeCanvasSnapshotKind::Blank);
    for index in 0..127 {
        native.drop_configuration(400 + index);
    }
    native.drop_configuration(600);
    drop(old);
    poll(&device, None);
    assert_eq!(native.live_allocations(), 0);
    drop((queue, device, global, native));
}

struct Notice(mpsc::Sender<()>);
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self(self.0.clone()))
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
        let _ = self.0.send(());
    }
}

extern "C" {
    fn wr_vulkan_external_images_new(renderer: &webrender::Renderer) -> *mut c_void;
    fn wr_vulkan_external_images_delete(images: *mut c_void);
    fn wr_test_native_canvas_routing_lock(id: u64, channel: u8, images: *mut c_void) -> u64;
    fn wr_test_native_canvas_routing_unlock(id: u64, images: *mut c_void);
}

struct Images(*mut c_void);
impl ExternalImageHandler for Images {
    fn lock(&mut self, id: ExternalImageId, channel: u8, _: bool) -> ExternalImage {
        let handle = unsafe { wr_test_native_canvas_routing_lock(id.0, channel, self.0) };
        ExternalImage {
            uv: TexelRect::new(0., 0., 3., 2.),
            source: if handle == 0 {
                ExternalImageSource::Invalid
            } else {
                ExternalImageSource::NativeTexture(ExternalTextureHandle(handle))
            },
        }
    }
    fn unlock(&mut self, id: ExternalImageId, _: u8) {
        unsafe { wr_test_native_canvas_routing_unlock(id.0, self.0) };
    }
}

pub struct RoutingFixture {
    renderer: Option<webrender::Renderer>,
    document: Option<Box<DocumentHandle>>,
    images: *mut c_void,
    ready: mpsc::Receiver<()>,
    frames: [Option<Arc<SealedCanvasFrame>>; 4],
    max_texture_size: i32,
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_new() -> *mut RoutingFixture {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    let queue = global.resolve_queue_id(QueueId::zip(1, 1));
    let allocator = NativeCanvasAllocator::new(owner.clone());
    let frames = std::array::from_fn(|case| {
        let bgra = case == 1 || case == 2;
        let desc = descriptor(if bgra {
            wgt::TextureFormat::Bgra8Unorm
        } else {
            wgt::TextureFormat::Rgba8Unorm
        });
        let (texture, canvas) = allocator.create_texture(&device, &desc).unwrap();
        write(&queue, &texture, &bytes(if case % 2 == 0 { &A } else { &B }, bgra));
        let sealed = unsafe { queue.seal_shared_canvas(canvas.unwrap()) }.unwrap();
        poll(&device, Some(sealed.submission_index()));
        Some(SealedCanvasFrame::new(owner.clone(), sealed).unwrap())
    });
    device.destroy();
    drop((allocator, queue, device, global));
    let graphics = unified::graphics(&owner);
    let (tx, ready) = mpsc::channel();
    let (mut renderer, sender) = webrender::create_webrender_instance(
        webrender::GpuBackendConfig::VulkanShared(graphics),
        Box::new(Notice(tx)),
        webrender::WebRenderOptions {
            enable_debugger: false,
            enable_subpixel_aa: false,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let max_texture_size = renderer.get_max_texture_size();
    let images = unsafe { wr_vulkan_external_images_new(&renderer) };
    assert!(!images.is_null());
    renderer.set_external_image_handler(Box::new(Images(images)));
    let document = Box::new(DocumentHandle::new(sender.create_api(), None, SIZE, 0));
    Box::into_raw(Box::new(RoutingFixture {
        renderer: Some(renderer),
        document: Some(document),
        images,
        ready,
        frames,
        max_texture_size,
    }))
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_take_document(fixture: &mut RoutingFixture) -> *mut DocumentHandle {
    Box::into_raw(fixture.document.take().unwrap())
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_renderer(fixture: &mut RoutingFixture) -> &mut webrender::Renderer {
    fixture.renderer.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_images(fixture: &RoutingFixture) -> *mut c_void {
    fixture.images
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_max_texture_size(fixture: &RoutingFixture) -> i32 {
    fixture.max_texture_size
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_frame(
    fixture: &mut RoutingFixture,
    case: u32,
) -> *mut NativeCanvasFrame {
    Box::into_raw(Box::new(NativeCanvasFrame::new(
        fixture.frames[case as usize].take().unwrap(),
    )))
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_render(fixture: &mut RoutingFixture, case: u32) {
    fixture.ready.recv_timeout(Duration::from_secs(30)).unwrap();
    let renderer = fixture.renderer.as_mut().unwrap();
    renderer.update();
    let result = renderer.render(SIZE, 0).unwrap();
    assert!(!result.external_images_pending);
    let pixels = renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(3, 2)));
    let opaque = case < 3;
    let step = case % 3;
    for (pixel, actual) in pixels.chunks_exact(4).enumerate() {
        let expected = if step == 1 {
            if opaque {
                [0, 0, 0, 255]
            } else {
                BG
            }
        } else {
            let source = if step == 0 { A[pixel] } else { B[pixel] };
            if opaque {
                [source[0], source[1], source[2], 255]
            } else {
                std::array::from_fn(|channel| {
                    if channel == 3 {
                        255
                    } else {
                        (source[channel] as f64 + BG[channel] as f64 * (1. - source[3] as f64 / 255.)).round() as u8
                    }
                })
            }
        };
        for channel in 0..4 {
            assert!(
                (actual[channel] as i32 - expected[channel] as i32).abs() <= 1,
                "case{case}/pixel{pixel}/channel{channel}: {} != {}",
                actual[channel],
                expected[channel]
            );
        }
    }
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_routing_deinit_renderer(fixture: &mut RoutingFixture) {
    let mut document = fixture.document.take();
    if let Some(document) = document.as_mut() {
        unsafe { webrender_bindings::bindings::wr_api_stop_render_backend(document) };
    }
    if let Some(renderer) = fixture.renderer.take() {
        renderer.deinit();
    }
    if let Some(mut document) = document {
        unsafe {
            webrender_bindings::bindings::wr_api_delete_document(&mut document);
            webrender_bindings::bindings::wr_api_shut_down(&mut document);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_native_canvas_routing_delete(fixture: *mut RoutingFixture) {
    if !fixture.is_null() {
        let fixture = unsafe { Box::from_raw(fixture) };
        assert!(fixture.renderer.is_none());
        assert!(fixture.document.is_none());
        unsafe {
            wr_vulkan_external_images_delete(fixture.images);
        }
    }
}
