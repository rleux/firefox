/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(test, feature = "native-vulkan", target_os = "linux"))]

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    os::fd::BorrowedFd,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use webrender::{api::*, wgpu::*};
use wgpu_core_remote_types::id;

#[link(name = "freetype")]
extern "C" {}

#[path = "../../src/canvas.rs"]
mod canvas;
mod server {
    use super::*;
    pub struct Global {
        pub global: wgpu_core_remote::global::Global,
        pub shared: Option<Arc<wgpu_shared::NativeContext>>,
        pub direct_targets: Mutex<HashMap<id::TextureId, Arc<canvas::Allocation>>>,
    }
    impl std::ops::Deref for Global {
        type Target = wgpu_core_remote::global::Global;
        fn deref(&self) -> &Self::Target {
            &self.global
        }
    }
}
thread_local! { static CAPS: RefCell<Option<DmaBufCapabilities>> = const { RefCell::new(None) }; }
fn descriptor(info: &canvas::CanvasExportInfo) -> DmaBufImageDescriptor {
    DmaBufImageDescriptor {
        size: [info.width, info.height],
        format: if info.bgra {
            wgt::TextureFormat::Bgra8Unorm
        } else {
            wgt::TextureFormat::Rgba8Unorm
        },
        usage: wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST,
        modifier: info.modifier,
        offset: info.offset,
        row_pitch: info.stride,
        device_uuid: info.device_uuid,
        driver_uuid: info.driver_uuid,
    }
}
#[no_mangle]
extern "C" fn wgpu_parent_native_canvas_supported(
    info: &canvas::CanvasExportInfo,
    opaque: bool,
) -> bool {
    CAPS.with(|caps| {
        caps.borrow().as_ref().is_some_and(|caps| {
            caps.supports(&descriptor(info)) && (!opaque || caps.supports_opaque())
        })
    })
}
static ERRORS: AtomicUsize = AtomicUsize::new(0);
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() == log::Level::Error
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            eprintln!("{}: {}", record.target(), record.args());
        }
    }
    fn flush(&self) {}
}
struct Notice;
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Notice)
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
}
fn exercise_canvas_publications(require_direct: bool) {
    static LOGGER: std::sync::Once = std::sync::Once::new();
    LOGGER.call_once(|| {
        log::set_logger(&Logger).unwrap();
        log::set_max_level(log::LevelFilter::Error);
    });
    let errors_before = ERRORS.load(Ordering::Relaxed);
    let (renderer, sender) = webrender::create_webrender_instance(
        webrender::GpuBackendConfig::Vulkan(Options {
            validation: true,
            shared_webgpu: true,
            ..Default::default()
        }),
        Box::new(Notice),
        webrender::WebRenderOptions::default(),
        None,
    )
    .unwrap();
    let registry_option = if require_direct {
        renderer.wgpu_external_textures()
    } else {
        None
    };
    if require_direct && registry_option.is_none() {
        drop(sender);
        renderer.deinit();
        panic!("Direct canvas publication requires a Vulkan DMA-BUF capable adapter");
    }
    CAPS.with(|caps| {
        *caps.borrow_mut() = registry_option
            .as_ref()
            .map(|registry| registry.device().dma_buf_capabilities().unwrap())
    });
    let context = wgpu_shared::NativeContext::active().unwrap();
    let instance = context.instance_with_policy(
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
                | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION,
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    );
    let adapter = instance
        .enumerate_adapters(wgt::Backends::VULKAN, false)
        .into_iter()
        .find(|adapter| context.matches_adapter(adapter))
        .unwrap();
    let (device, queue) = context.open(&adapter, &Default::default()).unwrap();
    let device_id = id::DeviceId::zip(1, 1);
    let queue_id = id::QueueId::zip(1, 1);
    let global = server::Global {
        global: wgpu_core_remote::global::Global::from_instance(instance),
        shared: Some(context),
        direct_targets: Mutex::new(HashMap::new()),
    };
    global.import_device(device.clone(), device_id);
    global.import_queue(queue.clone(), queue_id);
    let Some(registry) = registry_option.as_ref() else {
        eprintln!("Consumer declines native admission; checking the regular texture path");
        for (index, (format, color, expected)) in [
            (wgt::TextureFormat::Rgba8Unorm, None, [0u8, 0, 0, 0]),
            (
                wgt::TextureFormat::Rgba8Unorm,
                Some(wgt::Color::RED),
                [255, 0, 0, 255],
            ),
            (
                wgt::TextureFormat::Bgra8Unorm,
                Some(wgt::Color::BLUE),
                [0, 0, 255, 255],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let texture_id = id::TextureId::zip(index as u32 + 1, 1);
            let desc = wgpu_core_remote_types::TextureDescriptor {
                label: None,
                size: wgt::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
                view_formats: vec![],
            };
            assert!(!canvas::create_target(
                &global, device_id, queue_id, texture_id, &desc
            ));
            assert!(global.direct_targets.lock().unwrap().is_empty());
            global.device_create_texture(device_id, &desc, texture_id);
            let texture = global.resolve_texture_id(texture_id);
            let encoder = device.create_command_encoder(&Default::default());
            if let Some(color) = color {
                let mut pass =
                    encoder.begin_render_pass(wgc::command::ResolvedRenderPassDescriptor {
                        color_attachments: Cow::Owned(vec![Some(
                            wgc::command::RenderPassColorAttachment {
                                view: texture.create_view(&Default::default()),
                                depth_slice: None,
                                resolve_target: None,
                                load_op: wgt::LoadOp::Clear(color),
                                store_op: wgt::StoreOp::Store,
                            },
                        )]),
                        ..Default::default()
                    });
                pass.end();
            }
            let output = device.create_buffer(&wgc::resource::BufferDescriptor {
                label: None,
                size: 1024,
                usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                &wgt::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &wgt::TexelCopyBufferInfo {
                    buffer: output.clone(),
                    layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(4),
                    },
                },
                &desc.size,
            );
            assert!(queue
                .submit(&[encoder.finish(&Default::default())])
                .is_some());
            let (tx, rx) = std::sync::mpsc::channel();
            output.map_async(
                wgt::MapMode::Read,
                0,
                Some(1024),
                Some(Box::new(move |result| {
                    tx.send(result).unwrap();
                })),
            );
            device
                .poll(wgt::PollType::Wait {
                    submission_index: None,
                    timeout: Some(Duration::from_secs(10)),
                })
                .unwrap();
            rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            let bytes = output
                .get_mapped_range(0, Some(1024))
                .unwrap()
                .read_slice()
                .to_vec();
            let mut pixels: Vec<u8> = bytes
                .chunks_exact(256)
                .flat_map(|row| row[..16].iter().copied())
                .collect();
            if format == wgt::TextureFormat::Bgra8Unorm {
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }
            assert_eq!(pixels, expected.repeat(16));
            output.unmap();
            global.texture_remove(texture_id);
            println!("PASS rejected-direct fallback={format:?} rgba={expected:?}");
        }
        assert_eq!(ERRORS.load(Ordering::Relaxed), errors_before);
        drop(global);
        drop(sender);
        renderer.deinit();
        return;
    };
    for (index, (direct, format, color, expected)) in [
        (true, wgt::TextureFormat::Rgba8Unorm, None, [0, 0, 0, 0]),
        (
            true,
            wgt::TextureFormat::Rgba8Unorm,
            Some(wgt::Color::RED),
            [255, 0, 0, 255],
        ),
        (
            true,
            wgt::TextureFormat::Bgra8Unorm,
            Some(wgt::Color::BLUE),
            [0, 0, 255, 255],
        ),
        (
            false,
            wgt::TextureFormat::Rgba8Unorm,
            Some(wgt::Color::GREEN),
            [0, 255, 0, 255],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let texture_id = id::TextureId::zip(index as u32 + 1, 1);
        let desc = wgpu_core_remote_types::TextureDescriptor {
            label: None,
            size: wgt::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format,
            usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
            view_formats: vec![],
        };
        if direct {
            assert!(canvas::create_target(
                &global, device_id, queue_id, texture_id, &desc
            ));
        } else {
            global.device_create_texture(device_id, &desc, texture_id);
        }
        let source = global.resolve_texture_id(texture_id);
        if let Some(color) = color {
            let encoder = device.create_command_encoder(&Default::default());
            let mut pass = encoder.begin_render_pass(wgc::command::ResolvedRenderPassDescriptor {
                color_attachments: Cow::Owned(vec![Some(
                    wgc::command::RenderPassColorAttachment {
                        view: source.create_view(&Default::default()),
                        depth_slice: None,
                        resolve_target: None,
                        load_op: wgt::LoadOp::Clear(color),
                        store_op: wgt::StoreOp::Store,
                    },
                )]),
                ..Default::default()
            });
            pass.end();
            assert!(queue
                .submit(&[encoder.finish(&Default::default())])
                .is_some());
        }
        let mut info = canvas::CanvasExportInfo::default();
        let lease =
            canvas::wgpu_canvas_export(&global, device_id, queue_id, texture_id, false, &mut info);
        assert!(!lease.is_null(), "Native canvas publication {index} failed");
        assert_eq!(info.direct, direct);
        assert_eq!(info.ready_value, if direct { 2 } else { 1 });
        let allocation = unsafe {
            registry
                .device()
                .import_dma_buf(BorrowedFd::borrow_raw(info.memory_fd), descriptor(&info))
        }
        .unwrap();
        let image = registry.import(allocation).unwrap();
        let fd = unsafe { BorrowedFd::borrow_raw(info.ready_fd) }
            .try_clone_to_owned()
            .unwrap();
        let handle = unsafe { TimelineHandle::from_fd(fd, info.device_uuid, info.driver_uuid) };
        let ready = SharedTimeline::import(registry.device(), &handle).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !unsafe { registry.acquire(&image, &ready, info.ready_value) }.unwrap() {
            device.poll(wgt::PollType::Poll).unwrap();
            registry.flush_idle().unwrap();
            registry.poll().unwrap();
            assert!(Instant::now() < deadline, "Consumer acquisition timed out");
            std::thread::yield_now();
        }
        let mut pixels = vec![0; 64];
        registry.readback(&image, &mut pixels).unwrap();
        if info.bgra {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        assert_eq!(
            pixels,
            expected.repeat(16),
            "Canvas pixels differ: direct={direct} format={format:?}"
        );
        let receipt = registry.release(&image).unwrap();
        registry.flush_idle().unwrap();
        while receipt.status() != ExternalReleaseStatus::Complete {
            registry.poll().unwrap();
            assert!(Instant::now() < deadline, "Consumer retirement timed out");
        }
        device
            .poll(wgt::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(10)),
            })
            .unwrap();
        unsafe {
            canvas::wgpu_canvas_export_delete(lease);
        }
        global.texture_remove(texture_id);
        println!(
            "PASS direct={direct} format={format:?} rgba={expected:?} ready={}",
            info.ready_value
        );
    }
    assert_eq!(
        ERRORS.load(Ordering::Relaxed),
        errors_before,
        "Vulkan validation errors"
    );
    drop(global);
    drop(registry_option);
    drop(sender);
    renderer.deinit();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn rejected_native_admission_preserves_canvas_pixels() {
    exercise_canvas_publications(false);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF, external timelines and the Khronos validation layer"]
fn direct_native_publication_preserves_canvas_pixels() {
    exercise_canvas_publications(true);
}
