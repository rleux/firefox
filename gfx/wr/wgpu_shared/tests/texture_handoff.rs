/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use wgpu_core as core;
use wgpu_hal as hal;
use wgpu_types as wgt;

static VALIDATION_ERRORS: AtomicUsize = AtomicUsize::new(0);
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() == log::Level::Error
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            VALIDATION_ERRORS.fetch_add(1, Ordering::Relaxed);
            eprintln!("{}: {}", record.target(), record.args());
        }
    }
    fn flush(&self) {}
}

struct Fixture {
    _instance: Arc<core::instance::Instance>,
    producer: Arc<core::device::Device>,
    producer_queue: Arc<core::device::queue::Queue>,
    consumer: Arc<core::device::Device>,
    consumer_queue: Arc<core::device::queue::Queue>,
}

impl Fixture {
    fn new() -> Self {
        static START: std::sync::Once = std::sync::Once::new();
        START.call_once(|| {
            log::set_logger(&Logger).unwrap();
            log::set_max_level(log::LevelFilter::Error);
        });
        let instance = core::instance::Instance::new(
            "Native texture handoff test",
            wgt::InstanceDescriptor {
                backends: wgt::Backends::VULKAN,
                flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
                ..wgt::InstanceDescriptor::new_without_display_handle()
            },
            None,
        );
        let mut adapters = instance.enumerate_adapters(wgt::Backends::VULKAN, false);
        let index = adapters
            .iter()
            .position(|adapter| adapter.get_info().device_type == wgt::DeviceType::DiscreteGpu)
            .unwrap_or(0);
        let adapter = adapters.swap_remove(index);
        eprintln!("Native texture handoff adapter: {:?}", adapter.get_info());
        let descriptor = core::device::DeviceDescriptor {
            required_limits: adapter.limits(),
            ..Default::default()
        };
        let (consumer, consumer_queue) = adapter.request_device(&descriptor).unwrap();
        let (producer, producer_queue) =
            wgpu_shared::open_device(&adapter, &consumer, &consumer_queue, &descriptor).unwrap();
        for device in [&producer, &consumer] {
            device
                .on_uncaptured_error(Arc::new(|error| panic!("Unexpected core error: {error:?}")));
        }
        unsafe {
            let a = producer.clone().as_hal::<hal::api::Vulkan>().unwrap();
            let b = consumer.clone().as_hal::<hal::api::Vulkan>().unwrap();
            assert_eq!(a.raw_device().handle(), b.raw_device().handle());
        }
        Self {
            _instance: instance,
            producer,
            producer_queue,
            consumer,
            consumer_queue,
        }
    }
}

fn descriptor(usage: wgt::TextureUsages) -> core::resource::TextureDescriptor<'static> {
    core::resource::TextureDescriptor {
        label: Some("Trusted presentation allocation".into()),
        size: wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage,
        view_formats: vec![],
    }
}

fn wait(device: &Arc<core::device::Device>, index: Option<core::SubmissionIndex>) {
    device
        .poll(wgt::PollType::Wait {
            submission_index: index,
            timeout: Some(Duration::from_secs(10)),
        })
        .unwrap();
}

fn color_pass(
    encoder: &Arc<core::command::CommandEncoder>,
    view: Arc<core::resource::TextureView>,
    load_op: wgt::LoadOp<wgt::Color>,
    store_op: wgt::StoreOp,
) {
    let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
        color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            load_op,
            store_op,
        })]),
        ..Default::default()
    });
    pass.end();
}

fn sample_command(
    device: &Arc<core::device::Device>,
    texture: &Arc<core::resource::Texture>,
) -> (
    Arc<core::command::CommandBuffer>,
    Arc<core::resource::Buffer>,
) {
    let shader = device.create_shader_module(&core::pipeline::ShaderModuleDescriptor {
        label: None, runtime_checks: Default::default(),
    }, core::pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(r#"
        @group(0) @binding(0) var image: texture_2d<f32>;
        @group(0) @binding(1) var<storage, read_write> pixels: array<vec4<u32>>;
        @compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
            pixels[id.y * 2u + id.x] = vec4<u32>(round(textureLoad(image, vec2<i32>(id.xy), 0) * 255.0));
        }
    "#)));
    let pipeline = device.create_compute_pipeline(core::pipeline::ComputePipelineDescriptor {
        label: None,
        layout: None,
        cache: None,
        stage: core::pipeline::ProgrammableStageDescriptor {
            module: shader,
            entry_point: Some("main".into()),
            constants: Default::default(),
            zero_initialize_workgroup_memory: true,
        },
    });
    let buffer = |usage| {
        device.create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 64,
            usage,
            mapped_at_creation: false,
        })
    };
    let output = buffer(wgt::BufferUsages::STORAGE | wgt::BufferUsages::COPY_SRC);
    let readback = buffer(wgt::BufferUsages::MAP_READ | wgt::BufferUsages::COPY_DST);
    let group = device.create_bind_group(&core::binding_model::BindGroupDescriptor {
        label: None,
        layout: pipeline.get_bind_group_layout(0),
        entries: Cow::Owned(vec![
            core::binding_model::BindGroupEntry {
                binding: 0,
                resource: core::binding_model::BindingResource::TextureView(
                    texture.create_view(&Default::default()),
                ),
            },
            core::binding_model::BindGroupEntry {
                binding: 1,
                resource: core::binding_model::BindingResource::Buffer(
                    core::binding_model::BufferBinding {
                        buffer: output.clone(),
                        offset: 0,
                        size: None,
                    },
                ),
            },
        ]),
    });
    let encoder = device.create_command_encoder(&Default::default());
    let mut pass = encoder.begin_compute_pass(&core::command::ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, Some(group), &[]);
    pass.dispatch_workgroups(2, 2, 1);
    pass.end();
    encoder.copy_buffer_to_buffer(output, 0, readback.clone(), 0, Some(64));
    (encoder.finish(&Default::default()), readback)
}

fn read(device: &Arc<core::device::Device>, buffer: &Arc<core::resource::Buffer>) -> Vec<u32> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(
        wgt::MapMode::Read,
        0,
        Some(64),
        Some(Box::new(move |result| tx.send(result).unwrap())),
    );
    wait(device, None);
    rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    let result = buffer
        .get_mapped_range(0, Some(64))
        .unwrap()
        .read_slice()
        .chunks_exact(4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        .collect();
    buffer.unmap();
    result
}

#[test]
#[ignore = "Requires Vulkan and the Khronos synchronization validation layer; run serially"]
fn internal_sampling_preserves_public_usage_validation() {
    let fixture = Fixture::new();
    let desc = descriptor(wgt::TextureUsages::RENDER_ATTACHMENT);
    let texture = fixture
        .producer
        .create_texture_with_internal_sampling(&desc)
        .unwrap();
    assert_eq!(
        texture.descriptor().usage,
        wgt::TextureUsages::RENDER_ATTACHMENT
    );
    let errors = Arc::new(AtomicUsize::new(0));
    let callback_errors = errors.clone();
    fixture.producer.on_uncaptured_error(Arc::new(move |_| {
        callback_errors.fetch_add(1, Ordering::Relaxed);
    }));
    texture.create_view(&core::resource::TextureViewDescriptor {
        usage: Some(wgt::TextureUsages::TEXTURE_BINDING),
        ..Default::default()
    });
    assert_eq!(errors.load(Ordering::Relaxed), 1);
    let mut invalid = desc.clone();
    invalid.usage = wgt::TextureUsages::empty();
    assert!(fixture
        .producer
        .create_texture_with_internal_sampling(&invalid)
        .is_err());
    invalid = desc;
    invalid.usage |= wgt::TextureUsages::TRANSIENT_ATTACHMENT;
    assert!(fixture
        .producer
        .create_texture_with_internal_sampling(&invalid)
        .is_err());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos synchronization validation layer; run serially"]
fn retained_native_image_samples_initialized_contents_and_retires_once() {
    let fixture = Fixture::new();
    let before_errors = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for case in 0..4 {
        let producer_count = fixture.producer.get_hal_counters().textures.read();
        let consumer_count = fixture.consumer.get_hal_counters().textures.read();
        let desc = descriptor(wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC);
        let texture = fixture
            .producer
            .create_texture_with_internal_sampling(&desc)
            .unwrap();
        let image = unsafe {
            texture
                .clone()
                .as_hal::<hal::api::Vulkan>()
                .unwrap()
                .raw_handle()
        };
        let view = texture.create_view(&Default::default());
        let encoder = fixture.producer.create_command_encoder(&Default::default());
        if case != 0 {
            color_pass(
                &encoder,
                view.clone(),
                wgt::LoadOp::Clear(wgt::Color::RED),
                if case == 3 {
                    wgt::StoreOp::Store
                } else {
                    wgt::StoreOp::Discard
                },
            );
        }
        if case == 2 {
            let copy = fixture
                .producer
                .create_buffer(&core::resource::BufferDescriptor {
                    label: None,
                    size: 512,
                    usage: wgt::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            encoder.copy_texture_to_buffer(
                &wgt::TexelCopyTextureInfo {
                    texture: texture.clone(),
                    mip_level: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &wgt::TexelCopyBufferInfo {
                    buffer: copy,
                    layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(2),
                    },
                },
                &desc.size,
            );
        }
        color_pass(
            &encoder,
            view.clone(),
            wgt::LoadOp::Load,
            wgt::StoreOp::Store,
        );
        encoder.transition_resources(
            std::iter::empty(),
            std::iter::once(wgt::TextureTransition {
                texture: texture.clone(),
                selector: None,
                state: wgt::TextureUses::RESOURCE,
            }),
        );
        let submitted = fixture
            .producer_queue
            .submit(&[encoder.finish(&Default::default())])
            .unwrap();
        let keeper = texture.destroy_and_retain().unwrap();
        assert!(texture.destroy_and_retain().is_none());
        assert!(unsafe { texture.clone().as_hal::<hal::api::Vulkan>() }.is_none());
        wait(&fixture.producer, Some(submitted));
        let native = unsafe { keeper.as_hal::<hal::api::Vulkan>().unwrap() };
        assert_eq!(unsafe { native.raw_handle() }, image);
        let weak = Arc::downgrade(&keeper);
        let consumer_desc = descriptor(wgt::TextureUsages::TEXTURE_BINDING);
        let hal_desc = hal::TextureDescriptor {
            label: None,
            size: desc.size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: desc.dimension,
            format: desc.format,
            usage: wgt::TextureUses::RESOURCE,
            memory_flags: hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let dropped = Arc::new(AtomicUsize::new(0));
        let callback_dropped = dropped.clone();
        let callback_keeper = keeper.clone();
        let imported = unsafe {
            let device = fixture
                .consumer
                .clone()
                .as_hal::<hal::api::Vulkan>()
                .unwrap();
            let raw = device.texture_from_raw(
                image,
                &hal_desc,
                Some(Box::new(move || {
                    drop(callback_keeper);
                    callback_dropped.fetch_add(1, Ordering::Relaxed);
                })),
                hal::vulkan::TextureMemory::External,
            );
            assert_eq!(raw.raw_handle(), image);
            let (texture, error) = fixture.consumer.create_texture_from_hal(
                Box::new(raw),
                &consumer_desc,
                wgt::TextureUses::RESOURCE,
                true,
            );
            assert!(error.is_none());
            texture
        };
        drop(keeper);
        drop(view);
        drop(texture);
        let (commands, readback) = sample_command(&fixture.consumer, &imported);
        let submitted = fixture.consumer_queue.submit(&[commands]).unwrap();
        imported.destroy();
        drop(imported);
        assert!(weak.upgrade().is_some());
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        wait(&fixture.consumer, Some(submitted));
        let pixel = if case == 3 { [255, 0, 0, 255] } else { [0; 4] };
        assert_eq!(
            read(&fixture.consumer, &readback),
            pixel.repeat(4),
            "case {case}"
        );
        assert!(weak.upgrade().is_none());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(
            fixture.producer.get_hal_counters().textures.read(),
            producer_count
        );
        assert_eq!(
            fixture.consumer.get_hal_counters().textures.read(),
            consumer_count
        );
    }
    // Exercise ordinary core allocation and destroy(), without retained handoff.
    // The submission is not core-retired yet; no deliberately stalled GPU is needed.
    let consumer_count = fixture.consumer.get_hal_counters().textures.read();
    let texture = fixture.consumer.create_texture(&descriptor(
        wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::TEXTURE_BINDING,
    ));
    let encoder = fixture.consumer.create_command_encoder(&Default::default());
    color_pass(
        &encoder,
        texture.create_view(&Default::default()),
        wgt::LoadOp::Clear(wgt::Color::RED),
        wgt::StoreOp::Store,
    );
    let (sampling, readback) = sample_command(&fixture.consumer, &texture);
    let submitted = fixture
        .consumer_queue
        .submit(&[encoder.finish(&Default::default()), sampling])
        .unwrap();
    texture.destroy();
    assert!(unsafe { texture.clone().as_hal::<hal::api::Vulkan>() }.is_none());
    drop(texture);
    assert_eq!(
        fixture.consumer.get_hal_counters().textures.read(),
        consumer_count + 1
    );
    wait(&fixture.consumer, Some(submitted));
    assert_eq!(
        read(&fixture.consumer, &readback),
        [255, 0, 0, 255].repeat(4)
    );
    assert_eq!(
        fixture.consumer.get_hal_counters().textures.read(),
        consumer_count
    );
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before_errors);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos synchronization validation layer; run serially"]
fn retained_destroy_rejects_recorded_aliases_and_preserves_producer_retirement() {
    fn validation_context(device: &Arc<core::device::Device>) -> Box<core::error::ContextError> {
        match device
            .pop_error_scope()
            .unwrap()
            .expect("Expected validation error")
        {
            wgt::error::Error::Validation { source, .. } => {
                source.downcast::<core::error::ContextError>().unwrap()
            }
            error => panic!("Expected validation error, got {error:?}"),
        }
    }

    let fixture = Fixture::new();
    let texture = fixture
        .producer
        .create_texture_with_internal_sampling(&descriptor(
            wgt::TextureUsages::RENDER_ATTACHMENT
                | wgt::TextureUsages::TEXTURE_BINDING
                | wgt::TextureUsages::COPY_SRC,
        ))
        .unwrap();
    let view = texture.create_view(&Default::default());
    let encoder = fixture.producer.create_command_encoder(&Default::default());
    color_pass(
        &encoder,
        view.clone(),
        wgt::LoadOp::Clear(wgt::Color::RED),
        wgt::StoreOp::Store,
    );
    let submitted = fixture
        .producer_queue
        .submit(&[encoder.finish(&Default::default())])
        .unwrap();
    let (recorded_sampling, _) = sample_command(&fixture.producer, &texture);
    let encoder = fixture.producer.create_command_encoder(&Default::default());
    color_pass(
        &encoder,
        view,
        wgt::LoadOp::Clear(wgt::Color::GREEN),
        wgt::StoreOp::Store,
    );
    let recorded_render = encoder.finish(&Default::default());
    let encoder = fixture.producer.create_command_encoder(&Default::default());
    let copy = fixture
        .producer
        .create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 512,
            usage: wgt::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
    encoder.copy_texture_to_buffer(
        &wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &wgt::TexelCopyBufferInfo {
            buffer: copy,
            layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(2),
            },
        },
        &texture.descriptor().size,
    );
    let recorded_copy = encoder.finish(&Default::default());
    let keeper = texture.destroy_and_retain().unwrap();
    let weak = Arc::downgrade(&keeper);
    drop(keeper);
    assert!(weak.upgrade().is_some());
    for (alias, command) in [
        ("recorded sampling bind group", recorded_sampling),
        ("recorded render attachment view", recorded_render),
        ("recorded texture copy", recorded_copy),
    ] {
        fixture
            .producer
            .push_error_scope(wgt::error::ErrorFilter::Validation);
        assert!(
            fixture.producer_queue.submit(&[command]).is_none(),
            "{alias}"
        );
        let context = validation_context(&fixture.producer);
        assert_eq!(context.fn_ident, "Queue::submit");
        assert!(
            matches!(
                context
                    .source
                    .downcast_ref::<core::device::queue::QueueSubmitError>(),
                Some(core::device::queue::QueueSubmitError::DestroyedResource(_))
            ),
            "{alias}: {context:?}"
        );
        assert!(fixture.producer.is_valid());
    }
    fixture
        .producer
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    texture.create_view(&Default::default());
    let context = validation_context(&fixture.producer);
    assert_eq!(context.fn_ident, "Texture::create_view failed");
    assert!(
        matches!(
            context
                .source
                .downcast_ref::<core::resource::CreateTextureViewError>(),
            Some(core::resource::CreateTextureViewError::DestroyedResource(_))
        ),
        "{context:?}"
    );
    wait(&fixture.producer, Some(submitted));
    assert!(weak.upgrade().is_none());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos synchronization validation layer; run serially"]
fn retained_destroy_preserves_pending_write_retirement() {
    let fixture = Fixture::new();
    let before = fixture.producer.get_hal_counters().textures.read();
    let desc = descriptor(wgt::TextureUsages::COPY_DST);
    let texture = fixture
        .producer
        .create_texture_with_internal_sampling(&desc)
        .unwrap();
    fixture.producer_queue.write_texture(
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &[71; 16],
        &wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(2),
        },
        &desc.size,
    );
    let keeper = texture.destroy_and_retain().unwrap();
    let weak = Arc::downgrade(&keeper);
    drop(keeper);
    drop(texture);
    assert!(weak.upgrade().is_some());
    let submitted = fixture.producer_queue.submit(&[]).unwrap();
    assert!(weak.upgrade().is_some());
    wait(&fixture.producer, Some(submitted));
    assert!(weak.upgrade().is_none());
    assert_eq!(fixture.producer.get_hal_counters().textures.read(), before);
}
