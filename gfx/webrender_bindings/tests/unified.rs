/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use webrender::api::{units::*, *};
use webrender::vulkan::{
    BufferPool, Device, GraphicsRecorderFactory, SharedDeviceOptions, Submission, SubmissionQueue, Texture,
    TextureFilter,
};
use wgpu_bindings::unified::ProcessOwner;
use wgpu_core_remote::{global::Global, id::*};
use wgpu_core_remote_types::{Label, RequestAdapterOptions};
use wgpu_types as wgt;

type Descriptor = wgt::DeviceDescriptor<Label<'static>>;

const PIXELS: [u8; 16] = [13, 29, 79, 255, 201, 37, 11, 128, 7, 113, 211, 64, 91, 19, 157, 192];

unsafe extern "C" fn fatal() -> ! {
    extern "C" {
        fn _exit(status: i32) -> !;
    }
    unsafe { _exit(88) }
}

pub(super) fn owner() -> Arc<ProcessOwner> {
    let flags = (wgt::InstanceFlags::DEBUG
        | wgt::InstanceFlags::VALIDATION
        | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION
        | wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
        | wgt::InstanceFlags::DISCARD_HAL_LABELS)
        .with_env();
    let owner = unsafe {
        ProcessOwner::create(
            wgt::InstanceDescriptor {
                backends: wgt::Backends::VULKAN,
                flags,
                backend_options: Default::default(),
                memory_budget_thresholds: wgt::MemoryBudgetThresholds {
                    for_resource_creation: Some(95),
                    for_device_loss: Some(99),
                },
                display: None,
            },
            Arc::new(GraphicsRecorderFactory),
            fatal,
            true,
        )
    }
    .unwrap();
    assert!(owner.is_ready());
    assert_eq!(owner.flags(), flags);
    owner
}

pub(super) fn actor(owner: &Arc<ProcessOwner>, buckets: bool) -> Global {
    let global = owner.new_actor_global().unwrap();
    let options = RequestAdapterOptions {
        power_preference: wgt::PowerPreference::None,
        force_fallback_adapter: false,
    };
    global
        .request_adapter(&options, buckets, wgt::Backends::VULKAN, AdapterId::zip(1, 1))
        .unwrap();
    let advertised = global.adapter_features(AdapterId::zip(1, 1));
    assert!(owner.native_features().contains(advertised));
    let mut descriptor: Descriptor = Default::default();
    descriptor.required_features = advertised & wgt::Features::TIMESTAMP_QUERY;
    owner
        .create_device(
            &global,
            AdapterId::zip(1, 1),
            &mut descriptor,
            DeviceId::zip(1, 1),
            QueueId::zip(1, 1),
        )
        .unwrap();
    global
}

fn core_work(global: &Global) {
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    let queue = global.resolve_queue_id(QueueId::zip(1, 1));
    assert!(device.is_valid());
    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    let texture = device.create_texture(&wgpu_core::resource::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
        view_formats: Vec::new(),
    });
    let readback = device.create_buffer(&wgpu_core::resource::BufferDescriptor {
        label: None,
        size: 4,
        usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    queue.write_buffer(readback.clone(), 0, &[7, 13, 29, 255]);
    let encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        &wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &wgt::TexelCopyBufferInfo {
            buffer: readback.clone(),
            layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: None,
                rows_per_image: None,
            },
        },
        &wgt::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    let submission = queue.submit(&[encoder.finish(&Default::default())])
        .expect("Valid test copy must be submitted");
    let mapped = Arc::new(Mutex::new(None));
    let callback = mapped.clone();
    readback.map_async(
        wgt::MapMode::Read,
        0,
        Some(4),
        Some(Box::new(move |result| *callback.lock().unwrap() = Some(result))),
    );
    if device
        .poll(wgt::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(30)),
        })
        .is_err()
    {
        std::process::exit(88);
    }
    assert!(mapped.lock().unwrap().take().unwrap().is_ok());
    assert_eq!(
        readback.get_mapped_range(0, Some(4)).unwrap().read_slice().to_vec(),
        [0; 4]
    );
    readback.unmap();
    texture.destroy();
    readback.destroy();
    assert!(device.pop_error_scope().unwrap().is_none());
}

pub(super) fn graphics(owner: &Arc<ProcessOwner>) -> Rc<Device> {
    let (instance, adapter) = owner.clone_graphics_instance().unwrap();
    let graphics = owner.graphics();
    let device = Rc::new(
        unsafe {
            Device::from_shared_endpoint(
                graphics.clone(),
                instance,
                adapter,
                SharedDeviceOptions {
                    context: owner.context(),
                    recover: Rc::new(|_| std::process::exit(88)),
                    fatal_process: fatal,
                },
            )
        }
        .unwrap(),
    );
    assert!(Arc::ptr_eq(&device.hal_owner(), &graphics));
    device
}

fn renderer(device: Rc<Device>) -> webrender::Renderer {
    struct Notice;
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self)
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    }
    webrender::create_webrender_instance(
        webrender::GpuBackendConfig::VulkanShared(device),
        Box::new(Notice),
        webrender::WebRenderOptions {
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .0
}

fn graphics_work(texture: &Rc<Texture>) {
    let result = texture
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
        .and_then(|mut readback| readback.wait());
    match result {
        Ok(bytes) => assert_eq!(bytes, PIXELS),
        Err(error) => {
            eprintln!("Unified owner graphics retirement unconfirmed: {error}");
            std::process::exit(88);
        },
    }
}

#[no_mangle]
pub extern "C" fn wr_test_unified_owner_bootstrap() {
    let owner = owner();
    let identity = owner.identity();
    let first = actor(&owner, false);
    let second = actor(&owner, true);
    assert!(!Arc::ptr_eq(first.instance(), second.instance()));
    assert!(!Arc::ptr_eq(
        &first.resolve_device_id(DeviceId::zip(1, 1)),
        &second.resolve_device_id(DeviceId::zip(1, 1))
    ));
    assert!(!Arc::ptr_eq(
        &first.resolve_queue_id(QueueId::zip(1, 1)),
        &second.resolve_queue_id(QueueId::zip(1, 1))
    ));
    let mut options = RequestAdapterOptions {
        power_preference: wgt::PowerPreference::None,
        force_fallback_adapter: false,
    };
    assert!(first
        .request_adapter(&options, false, wgt::Backends::DX12, AdapterId::zip(2, 1))
        .is_err());
    options.force_fallback_adapter = true;
    assert!(first
        .request_adapter(&options, false, wgt::Backends::VULKAN, AdapterId::zip(2, 1))
        .is_err());
    let mut invalid: Descriptor = Default::default();
    invalid.required_features = wgt::Features::all();
    assert!(!owner.native_features().contains(invalid.required_features));
    assert!(owner
        .create_device(
            &first,
            AdapterId::zip(1, 1),
            &mut invalid,
            DeviceId::zip(2, 1),
            QueueId::zip(2, 1)
        )
        .is_err());
    assert!(owner.is_ready());
    core_work(&first);
    let graphics = graphics(&owner);
    let renderer = renderer(graphics.clone());
    assert!(Arc::ptr_eq(
        &renderer.wgpu_external_textures().unwrap().device().hal_owner(),
        &owner.graphics()
    ));
    let texture = Texture::new(
        &graphics,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let pool = Rc::new(BufferPool::new(&graphics));
    let uploads = SubmissionQueue::new(&pool, 1).unwrap();
    texture
        .upload(
            &uploads,
            DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
            &PIXELS,
            None,
            0,
            Some(ImageFormat::RGBA8),
        )
        .unwrap();
    let serial = uploads.submit().unwrap_or_else(|_| std::process::exit(88));
    if uploads.wait_for(serial).is_err() {
        std::process::exit(88);
    }
    drop((uploads, pool));
    graphics_work(&texture);
    first.resolve_device_id(DeviceId::zip(1, 1)).destroy();
    drop(first);
    assert!(owner.is_ready());
    assert_eq!(owner.identity(), identity);
    core_work(&second);
    graphics_work(&texture);
    let replacement = self::owner();
    assert_ne!(replacement.identity(), identity);
    let mut descriptor: Descriptor = Default::default();
    assert!(replacement
        .create_device(
            &second,
            AdapterId::zip(1, 1),
            &mut descriptor,
            DeviceId::zip(2, 1),
            QueueId::zip(2, 1)
        )
        .is_err());
    assert!(replacement.is_ready());
    let queued_device = second.resolve_device_id(DeviceId::zip(1, 1));
    let queued_queue = second.resolve_queue_id(QueueId::zip(1, 1));
    let queued_core = queued_queue.submit(&[])
        .expect("Retained Core work must be accepted before owner shutdown");
    let mut queued_graphics = Submission::new(&graphics).unwrap();
    drop(queued_graphics.recording().unwrap());
    queued_graphics.submit().unwrap();
    owner.begin_shutdown();
    assert!(!owner.is_ready());
    assert!(owner.new_actor_global().is_err());
    assert!(owner.clone_graphics_instance().is_err());
    assert!(owner
        .create_device(
            &second,
            AdapterId::zip(1, 1),
            &mut descriptor,
            DeviceId::zip(2, 1),
            QueueId::zip(2, 1)
        )
        .is_err());
    if queued_device
        .poll(wgt::PollType::Wait {
            submission_index: Some(queued_core),
            timeout: Some(Duration::from_secs(30)),
        })
        .is_err()
    {
        std::process::exit(88);
    }
    match queued_graphics.wait(Some(Duration::from_secs(30))) {
        Ok(true) => {},
        _ => std::process::exit(88),
    }
    drop((queued_graphics, queued_queue, queued_device));
    let fresh = actor(&replacement, false);
    core_work(&fresh);
    renderer.deinit();
    drop((texture, graphics, second));
    let context = owner.context();
    if context.drain().is_err() {
        std::process::exit(88);
    }
    drop(context);
    let weak = Arc::downgrade(&owner);
    drop(owner);
    assert!(weak.upgrade().is_none());
    replacement.begin_shutdown();
    drop(fresh);
    if replacement.context().drain().is_err() {
        std::process::exit(88);
    }
}
