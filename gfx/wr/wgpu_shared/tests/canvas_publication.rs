/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{
    borrow::Cow,
    sync::Arc,
    time::{Duration, Instant},
};
use wgpu_core as core;
use wgpu_hal as hal;
use wgpu_shared::{canvas::CanvasPublication, NativeContext};
use wgpu_types as wgt;

fn wait(device: &Arc<core::device::Device>, index: Option<core::SubmissionIndex>) {
    device
        .poll(wgt::PollType::Wait {
            submission_index: index,
            timeout: Some(Duration::from_secs(10)),
        })
        .unwrap();
}

fn wait_for_allocation_retirement(device: &Arc<core::device::Device>, expected: isize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while device.get_hal_counters().textures.read() != expected {
        assert!(
            Instant::now() < deadline,
            "Original canvas allocation did not retire"
        );
        device.poll(wgt::PollType::Poll).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn fixture() -> (
    Arc<NativeContext>,
    Arc<core::device::Device>,
    Arc<core::device::queue::Queue>,
) {
    let instance = core::instance::Instance::new(
        "Owned canvas publication test",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    );
    let adapters = instance.enumerate_adapters(wgt::Backends::VULKAN, false);
    let adapter = adapters
        .iter()
        .find(|adapter| adapter.get_info().device_type == wgt::DeviceType::DiscreteGpu)
        .unwrap_or_else(|| adapters.first().unwrap())
        .clone();
    eprintln!("Owned canvas publication adapter: {:?}", adapter.get_info());
    let desc = core::device::DeviceDescriptor {
        required_limits: adapter.limits(),
        ..Default::default()
    };
    let (device, queue) = adapter.request_device(&desc).unwrap();
    let context = Arc::new(NativeContext::new(instance, adapter.clone(), device, queue));
    let (producer, producer_queue) = context.open(&adapter, &desc).unwrap();
    for device in [&producer, &context.device] {
        device.on_uncaptured_error(Arc::new(|error| panic!("Unexpected core error: {error:?}")));
    }
    assert!(!Arc::ptr_eq(&producer, &context.device));
    (context, producer, producer_queue)
}

fn descriptor() -> core::resource::TextureDescriptor<'static> {
    core::resource::TextureDescriptor {
        label: Some("Owned test canvas".into()),
        size: wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
        view_formats: vec![],
    }
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
#[ignore = "Requires remote Vulkan and Khronos synchronization validation; run serially"]
fn owned_publication_uses_identical_image_and_independent_consumers() {
    let (context, producer, queue) = fixture();
    let before = producer.get_hal_counters().textures.read();
    for stored in [false, true] {
        let texture = producer
            .create_texture_with_internal_sampling(&descriptor())
            .unwrap();
        let native_image = unsafe {
            texture
                .clone()
                .as_hal::<hal::api::Vulkan>()
                .unwrap()
                .raw_handle()
        };
        let native_device = unsafe {
            producer
                .clone()
                .as_hal::<hal::api::Vulkan>()
                .unwrap()
                .raw_device()
                .handle()
        };
        if stored {
            let encoder = producer.create_command_encoder(&Default::default());
            let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
                color_attachments: Cow::Owned(vec![Some(
                    core::command::RenderPassColorAttachment {
                        view: texture.create_view(&Default::default()),
                        depth_slice: None,
                        resolve_target: None,
                        load_op: wgt::LoadOp::Clear(wgt::Color::RED),
                        store_op: wgt::StoreOp::Store,
                    },
                )]),
                ..Default::default()
            });
            pass.end();
            queue
                .submit(&[encoder.finish(&Default::default())])
                .unwrap();
        }
        let lease = unsafe { CanvasPublication::seal(&context, &texture, &queue, 41, 73) }.unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !lease.is_ready(),
            "A completed native fence cannot replace owner-thread core cleanup"
        );
        assert!(lease.matches(&context, 41, 73));
        assert!(!lease.matches(&context, 42, 73));
        assert!(!lease.matches(&context, 41, 74));
        assert!(unsafe { texture.clone().as_hal::<hal::api::Vulkan>() }.is_none());
        assert!(lease.wrap(&context.device).unwrap().is_none());
        wait(&producer, None);
        assert!(!unsafe { lease.poll_producer_on_owner_thread() });
        assert!(lease.is_ready());
        let a = lease.wrap(&context.device).unwrap().unwrap();
        let b = lease.wrap(&context.device).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(a.descriptor().usage, wgt::TextureUsages::TEXTURE_BINDING);
        for consumer in [&a, &b] {
            let consumer_image = {
                let raw = unsafe { consumer.clone().as_hal::<hal::api::Vulkan>().unwrap() };
                unsafe { raw.raw_handle() }
            };
            assert_eq!(consumer_image, native_image);
            assert!(lease.matches_texture(consumer));
        }
        assert_eq!(native_device, unsafe {
            context
                .device
                .clone()
                .as_hal::<hal::api::Vulkan>()
                .unwrap()
                .raw_device()
                .handle()
        });
        let (commands_a, pixels_a) = sample_command(&context.device, &a);
        let (commands_b, pixels_b) = sample_command(&context.device, &b);
        context.queue.submit(&[commands_a]).unwrap();
        let last = context.queue.submit(&[commands_b]).unwrap();
        drop(lease);
        drop(texture);
        a.destroy();
        b.destroy();
        drop((a, b));
        assert_eq!(producer.get_hal_counters().textures.read(), before + 1);
        wait(&context.device, Some(last));
        let expected = if stored { [255, 0, 0, 255] } else { [0; 4] }.repeat(4);
        assert_eq!(read(&context.device, &pixels_a), expected);
        assert_eq!(read(&context.device, &pixels_b), expected);
        wait_for_allocation_retirement(&producer, before);
        eprintln!("Owned publication proof: producer_device={native_device:?} producer_image={native_image:?} consumers=2 same_device=true same_image=true exact_pixels=true");
    }
}

#[test]
#[ignore = "Requires remote Vulkan and Khronos synchronization validation; run serially"]
fn sealed_canvas_survives_logical_producer_destroy_and_rejects_retired_context() {
    let (context, producer, queue) = fixture();
    let texture = producer
        .create_texture_with_internal_sampling(&descriptor())
        .unwrap();
    let lease = unsafe { CanvasPublication::seal(&context, &texture, &queue, 51, 83) }.unwrap();
    producer.destroy();
    wait(&producer, None);
    assert!(!unsafe { lease.poll_producer_on_owner_thread() });
    assert!(lease.is_ready());
    let sampled = lease.wrap(&context.device).unwrap().unwrap();
    let (command, pixels) = sample_command(&context.device, &sampled);
    let last = context.queue.submit(&[command]).unwrap();
    wait(&context.device, Some(last));
    assert_eq!(read(&context.device, &pixels), [0; 16]);
    context.mark_failed();
    assert!(!lease.matches(&context, 51, 83));
    assert!(lease.wrap(&context.device).is_err());
}

#[test]
#[ignore = "Requires remote Vulkan and Khronos synchronization validation; run serially"]
fn immediate_actor_queue_drop_drains_callback_and_pending_publication() {
    let (context, producer, queue) = fixture();
    let texture = producer
        .create_texture_with_internal_sampling(&descriptor())
        .unwrap();
    let lease = unsafe { CanvasPublication::seal(&context, &texture, &queue, 61, 93) }.unwrap();
    // ActorDestroy drops Global and the last producer Queue on the actor thread.
    drop(queue);
    assert!(!unsafe { lease.poll_producer_on_owner_thread() });
    let consumer = lease.wrap(&context.device).unwrap().unwrap();
    let (command, pixels) = sample_command(&context.device, &consumer);
    let index = context.queue.submit(&[command]).unwrap();
    wait(&context.device, Some(index));
    assert_eq!(read(&context.device, &pixels), [0; 16]);
}

#[test]
#[ignore = "Requires remote Vulkan and Khronos synchronization validation; run serially"]
fn shared_quota_returns_after_delayed_publications_retire() {
    let (context, producer, queue) = fixture();
    let before = producer.get_hal_counters().textures.read();
    let admit = |publication| {
        let texture = producer
            .create_texture_with_internal_sampling(&descriptor())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(lease) =
                unsafe { CanvasPublication::seal(&context, &texture, &queue, 71, publication) }
            {
                break lease;
            }
            assert!(
                Instant::now() < deadline,
                "Shared quota was not returned after retirement"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    for wave in 0..2 {
        let leases = (0..6)
            .map(|index| admit(100 + wave * 6 + index))
            .collect::<Vec<_>>();
        let fallback = producer
            .create_texture_with_internal_sampling(&descriptor())
            .unwrap();
        assert!(unsafe { CanvasPublication::seal(&context, &fallback, &queue, 71, 200) }.is_none());
        assert!(unsafe { fallback.clone().as_hal::<hal::api::Vulkan>() }.is_some());
        drop(fallback);
        std::thread::sleep(Duration::from_millis(300));
        wait(&producer, None);
        for lease in &leases {
            assert!(!unsafe { lease.poll_producer_on_owner_thread() });
            assert!(!unsafe { lease.poll_producer_on_owner_thread() });
        }
        drop(leases);
        wait_for_allocation_retirement(&producer, before);
    }
}
