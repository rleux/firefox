/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[path = "shared_canvas_tests.rs"]
mod shared_canvas_tests;

#[path = "shared_canvas_sampling_tests.rs"]
mod shared_canvas_sampling_tests;

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use std::{
    borrow::Cow,
    cell::Cell,
    convert::TryInto,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use wgpu_core as core;
use hal::GraphicsRecorderFactory as _;

struct GraphicsProvider {
    graphics: Arc<hal::DynOpenDevice>,
    context: Arc<dyn hal::shared::SharedDeviceContext>,
    creations: AtomicUsize,
    recoveries: AtomicUsize,
    receipts: Mutex<Vec<Arc<Receipt>>>,
    lanes: Mutex<Vec<hal::shared::SharedQueueLane>>,
    native_wait: Mutex<Option<hal::external_sync::SemaphoreValue>>,
}

struct Receipt {
    native: Box<dyn hal::shared::SharedSubmissionReceipt>,
    ready: AtomicBool,
}

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeReceiptWithPollGate")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct ReceiptHandle(Arc<Receipt>);

impl hal::shared::SharedSubmissionReceipt for ReceiptHandle {
    fn poll(&self) -> Result<bool, hal::DeviceError> {
        self.0
            .native
            .poll()
            .map(|complete| complete && self.0.ready.load(Ordering::SeqCst))
    }

    fn wait(&self, timeout: Option<std::time::Duration>) -> Result<bool, hal::DeviceError> {
        self.0.native.wait(timeout)
    }
}

impl GraphicsProvider {
    fn wait_native(&self) {
        for receipt in self.receipts.lock().unwrap().iter() {
            assert!(receipt.native.wait(None).unwrap());
        }
    }

    fn release_receipts(&self) {
        for receipt in self.receipts.lock().unwrap().iter() {
            receipt.ready.store(true, Ordering::SeqCst);
        }
    }
}

impl fmt::Debug for GraphicsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealWRGraphicsProvider")
            .finish_non_exhaustive()
    }
}

unsafe extern "C" fn terminate_test_process() -> ! {
    std::process::abort()
}

impl core::graphics::GraphicsSubmissionProvider for GraphicsProvider {
    fn seal_encoder(
        &self,
        encoder: &mut dyn hal::DynCommandEncoder,
    ) -> Result<(), hal::DeviceError> {
        encoder
            .as_any_mut()
            .downcast_mut::<wgpu_graphics::RoutedEncoder>()
            .ok_or(hal::DeviceError::Unexpected)?
            .seal_graphics()
    }

    fn submission_parts<'a>(
        &self,
        encoder: &'a dyn hal::DynCommandEncoder,
        buffers: &'a [Box<dyn hal::DynCommandBuffer>],
        queue: &'a dyn hal::DynQueue,
    ) -> Result<Vec<core::graphics::GraphicsSubmitPart<'a>>, hal::DeviceError> {
        encoder
            .as_any()
            .downcast_ref::<wgpu_graphics::RoutedEncoder>()
            .ok_or(hal::DeviceError::Unexpected)?
            .submit_parts(buffers, queue)
            .map(|parts| {
                parts
                    .into_iter()
                    .map(|part| core::graphics::GraphicsSubmitPart {
                        lane: part.lane,
                        queue: part.queue,
                        buffer: part.buffer,
                    })
                    .collect()
            })
    }

    fn graphics_queue(&self) -> Option<&dyn hal::DynQueue> {
        Some(self.graphics.queue.as_ref())
    }

    unsafe fn submit(
        &self,
        logical_queue: &dyn hal::DynQueue,
        segments: &[hal::shared::SharedSubmitSegment<'_>],
        terminal: (&dyn hal::DynFence, hal::FenceValue),
    ) -> Result<Box<dyn hal::shared::SharedSubmissionReceipt>, hal::shared::SharedSubmitError> {
        self.lanes
            .lock()
            .unwrap()
            .extend(segments.iter().map(|segment| segment.lane));
        let native = unsafe {
            shared_canvas_tests::submit_with_gate(self, logical_queue, segments, terminal)
        }?;
        let receipt = Arc::new(Receipt {
            native,
            ready: AtomicBool::new(false),
        });
        self.receipts.lock().unwrap().push(receipt.clone());
        Ok(Box::new(ReceiptHandle(receipt)))
    }

    fn drain(&self) -> Result<(), hal::DeviceError> {
        self.context.drain()
    }

    fn request_recovery(&self, _: core::graphics::NativeRetirement) {
        self.recoveries.fetch_add(1, Ordering::SeqCst);
    }

    fn fatal_process_termination(&self) -> unsafe extern "C" fn() -> ! {
        terminate_test_process
    }
}

impl core::graphics::GraphicsRecordingProvider for GraphicsProvider {
    fn submission(self: Arc<Self>) -> Option<Arc<dyn core::graphics::GraphicsSubmissionProvider>> {
        Some(self)
    }

    unsafe fn create_encoder(
        &self,
        _device: &dyn hal::DynDevice,
        native: Box<dyn hal::DynCommandEncoder>,
    ) -> Result<Box<dyn hal::DynCommandEncoder>, hal::DeviceError> {
        self.creations.fetch_add(1, Ordering::SeqCst);
        unsafe { GraphicsRecorderFactory.create(self.graphics.clone()) }.map(|graphics| {
            Box::new(wgpu_graphics::RoutedEncoder::new(native, graphics))
                as Box<dyn hal::DynCommandEncoder>
        })
    }
}

struct MixedWork {
    commands: Arc<core::command::CommandBuffer>,
    state: Arc<core::resource::Buffer>,
    texture: Arc<core::resource::Texture>,
    readback: Arc<core::resource::Buffer>,
}

fn webgpu_mixed_work(device: &Arc<core::device::Device>) -> MixedWork {
    use core::{binding_model as binding, command, pipeline, resource};

    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    let shader = device.create_shader_module(
        &pipeline::ShaderModuleDescriptor {
            label: None,
            runtime_checks: Default::default(),
        },
        pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
            "struct State { red: u32, green: u32, blue: u32, alpha: u32, marker: u32, }
            @group(0) @binding(0) var<storage, read_write> state: State;
            @group(0) @binding(1) var painted: texture_2d<f32>;
            @compute @workgroup_size(1) fn before() {
                state.red = 0u;
                state.green = 255u;
                state.blue = 0u;
                state.alpha = 255u;
                state.marker = 17u;
            }
            @compute @workgroup_size(1) fn after() {
                let pixel = textureLoad(painted, vec2i(0, 0), 0);
                state.red = u32(pixel.g * 255.0) + 7u;
                state.green = state.marker + 1u;
            }",
        )),
    );
    let render_shader = device.create_shader_module(
        &pipeline::ShaderModuleDescriptor {
            label: None,
            runtime_checks: Default::default(),
        },
        pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
            "struct State { red: u32, green: u32, blue: u32, alpha: u32, marker: u32, }
            @group(0) @binding(0) var<storage, read> state: State;
            @vertex fn vs(@builtin(vertex_index) id: u32) -> @builtin(position) vec4f {
                let x = f32((id << 1u) & 2u) * 2.0 - 1.0;
                let y = f32(id & 2u) * 2.0 - 1.0;
                return vec4f(x, y, 0.0, 1.0);
            }
            @fragment fn fs() -> @location(0) vec4f {
                return vec4f(f32(state.red), f32(state.green), f32(state.blue), f32(state.alpha)) / 255.0;
            }",
        )),
    );
    let stage = |module: Arc<pipeline::ShaderModule>, entry_point: &'static str| {
        pipeline::ProgrammableStageDescriptor {
            module,
            entry_point: Some(Cow::Borrowed(entry_point)),
            constants: Default::default(),
            zero_initialize_workgroup_memory: true,
        }
    };
    let before = device.create_compute_pipeline(pipeline::ComputePipelineDescriptor {
        label: None,
        layout: None,
        stage: stage(shader.clone(), "before"),
        cache: None,
    });
    let after = device.create_compute_pipeline(pipeline::ComputePipelineDescriptor {
        label: None,
        layout: None,
        stage: stage(shader, "after"),
        cache: None,
    });
    let render = device.create_render_pipeline(
        pipeline::RenderPipelineDescriptor {
            label: None,
            layout: None,
            vertex: pipeline::VertexState {
                stage: stage(render_shader.clone(), "vs"),
                buffers: Cow::Borrowed(&[]),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(pipeline::FragmentState {
                stage: stage(render_shader, "fs"),
                targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                    format: wgt::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })]),
            }),
            multiview_mask: None,
            cache: None,
        }
        .into(),
    );
    let state = device.create_buffer(&resource::BufferDescriptor {
        label: None,
        size: 32,
        usage: wgt::BufferUsages::STORAGE | wgt::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&resource::BufferDescriptor {
        label: None,
        size: 512,
        usage: wgt::BufferUsages::MAP_READ | wgt::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let size = wgt::Extent3d {
        width: 2,
        height: 1,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&resource::TextureDescriptor {
        label: None,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::RENDER_ATTACHMENT
            | wgt::TextureUsages::TEXTURE_BINDING
            | wgt::TextureUsages::COPY_SRC,
        view_formats: Vec::new(),
    });
    let view = texture.create_view(&Default::default());
    let bind = |layout, sampled| {
        let mut entries = vec![binding::BindGroupEntry {
            binding: 0,
            resource: binding::BindingResource::Buffer(binding::BufferBinding {
                buffer: state.clone(),
                offset: 0,
                size: Some(32),
            }),
        }];
        if sampled {
            entries.push(binding::BindGroupEntry {
                binding: 1,
                resource: binding::BindingResource::TextureView(view.clone()),
            });
        }
        device.create_bind_group(&binding::BindGroupDescriptor {
            label: None,
            layout,
            entries: Cow::Owned(entries),
        })
    };
    let before_bind = bind(before.get_bind_group_layout(0), false);
    let after_bind = bind(after.get_bind_group_layout(0), true);
    let render_bind = bind(render.get_bind_group_layout(0), false);
    let encoder = device.create_command_encoder(&Default::default());
    let compute_desc = command::ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    };
    {
        let mut pass = encoder.begin_compute_pass(&compute_desc);
        pass.set_pipeline(before);
        pass.set_bind_group(0, Some(before_bind), &[]);
        pass.dispatch_workgroups(1, 1, 1);
        pass.end();
    }
    {
        let mut pass = encoder.begin_render_pass(command::ResolvedRenderPassDescriptor {
            label: None,
            color_attachments: Cow::Owned(vec![Some(command::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                load_op: command::LoadOp::Clear(wgt::Color::BLACK),
                store_op: command::StoreOp::Store,
            })]),
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(render);
        pass.set_bind_group(0, Some(render_bind), &[]);
        pass.draw(3, 1, 0, 0);
        pass.end();
    }
    {
        let mut pass = encoder.begin_compute_pass(&compute_desc);
        pass.set_pipeline(after);
        pass.set_bind_group(0, Some(after_bind), &[]);
        pass.dispatch_workgroups(1, 1, 1);
        pass.end();
    }
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
                bytes_per_row: Some(256),
                rows_per_image: None,
            },
        },
        &size,
    );
    encoder.copy_buffer_to_buffer(state.clone(), 0, readback.clone(), 256, Some(32));
    let commands = encoder.finish(&Default::default());
    assert!(device.pop_error_scope().unwrap().is_none());
    MixedWork {
        commands,
        state,
        texture,
        readback,
    }
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_endpoint_records_webgpu_and_renders_webrender() {
    tests::validation_logging();
    let flags = wgt::InstanceFlags::DEBUG
        | wgt::InstanceFlags::VALIDATION
        | wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
        | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION;
    let original = core::instance::Instance::new(
        "Shared WR/core proof",
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
        None,
    );
    let mut adapters = unsafe {
        original
            .raw(wgt::Backend::Vulkan)
            .unwrap()
            .enumerate_adapters(None)
    };
    let index = select_adapter(
        adapters
            .iter()
            .map(|adapter| (adapter.info.name.as_str(), adapter.info.device_type)),
        None,
    )
    .unwrap();
    let exposed = adapters.swap_remove(index);
    drop(adapters);
    let features = Device::shared_features(exposed.features);
    let pair = unsafe {
        hal::shared::open_shared_device(
            &*exposed.adapter,
            features,
            &exposed.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .unwrap();
    let hal::shared::SharedDeviceEndpoints {
        context,
        graphics,
        compute: initial_compute,
    } = pair;
    drop(initial_compute);
    let graphics = Arc::new(graphics);
    let (_, wr_instance) = context.clone_instance();
    let wr_recoveries = Rc::new(Cell::new(0));
    let recoveries = wr_recoveries.clone();
    let owner = Rc::new(
        unsafe {
            Device::from_shared_endpoint(
                graphics.clone(),
                wr_instance,
                exposed,
                SharedDeviceOptions {
                    context: context.clone(),
                    recover: Rc::new(move |_| recoveries.set(recoveries.get() + 1)),
                    fatal_process: terminate_test_process,
                },
            )
        }
        .unwrap(),
    );
    assert!(Arc::ptr_eq(&owner.open, &graphics));
    assert!(
        hal::renderer::graphics_capabilities(
            &*owner.instance,
            &*owner.adapter,
            &*owner.open.device,
        )
        .unwrap()
        .validation_enabled,
        "Khronos validation must be active for the native proof"
    );

    let (backend, core_instance) = context.clone_instance();
    let core_instance = unsafe {
        core::instance::Instance::from_hal_instance_dynamic(
            "Private WebGPU core view".into(),
            backend,
            core_instance,
            flags,
            None,
        )
    };
    drop(original);
    let adapter = core_instance
        .enumerate_adapters(wgt::Backends::VULKAN, false)
        .pop()
        .unwrap();
    let mut desc = core::device::DeviceDescriptor::default();
    adapter.validate_device_descriptor(&mut desc).unwrap();
    let compute = unsafe {
        context.clone().open_compute_device(
            adapter.as_hal_dynamic(),
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
        )
    }
    .unwrap();
    let provider = Arc::new(GraphicsProvider {
        graphics: graphics.clone(),
        context: context.clone(),
        creations: AtomicUsize::new(0),
        recoveries: AtomicUsize::new(0),
        receipts: Mutex::new(Vec::new()),
        lanes: Mutex::new(Vec::new()),
        native_wait: Mutex::new(None),
    });
    let (core_device, core_queue) = unsafe {
        adapter.create_device_and_queue_from_hal_with_graphics(
            compute,
            &desc,
            Some(provider.clone()),
        )
    }
    .unwrap();
    let work = webgpu_mixed_work(&core_device);
    assert!(provider.creations.load(Ordering::SeqCst) >= 2);

    let target = Texture::new(
        &owner,
        2,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let alignment = owner
        .capabilities()
        .alignments
        .buffer_copy_pitch
        .get()
        .max(4);
    let pitch = 8u64.div_ceil(alignment) * alignment;
    let (buffer, _) = unsafe {
        owner.open.device.create_buffer(&hal::BufferDescriptor {
            label: Some("shared renderer proof readback"),
            size: pitch,
            usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
            memory_flags: hal::MemoryFlags::PREFER_COHERENT,
        })
    }
    .unwrap();
    let readback = Rc::new(resources::Owned::new(
        &owner,
        buffer,
        <dyn hal::DynDevice>::destroy_buffer,
    ));
    let mut submission = Submission::new(&owner).unwrap();
    let pass = draw::DrawPass {
        target: &target,
        origin: DeviceIntPoint::zero(),
        viewport: None,
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let full = DeviceIntRect::from_size(DeviceIntSize::new(2, 1));
    {
        let mut commands = submission.recording().unwrap();
        pass.clear_rect(&mut commands, full, Some([0.0, 0.0, 0.0, 1.0]), None)
            .unwrap();
        pass.clear_rect(
            &mut commands,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            Some([1.0, 0.0, 0.0, 1.0]),
            None,
        )
        .unwrap();
        commands.keep(&readback);
        target
            .transition(&mut commands, wgt::TextureUses::COPY_SRC)
            .unwrap();
        unsafe {
            let encoder = commands.encoder();
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &**readback,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::empty(),
                    to: wgt::BufferUses::COPY_DST,
                },
            }]);
            encoder.copy_texture_to_buffer(
                target.raw_texture(),
                wgt::TextureUses::COPY_SRC,
                &**readback,
                &[hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch as u32),
                        rows_per_image: None,
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: 0,
                        array_layer: 0,
                        origin: wgt::Origin3d::ZERO,
                        aspect: hal::FormatAspects::COLOR,
                    },
                    size: wgt::Extent3d {
                        width: 2,
                        height: 1,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }],
            );
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &**readback,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_DST,
                    to: wgt::BufferUses::MAP_READ,
                },
            }]);
        }
    }
    let pending = core_device
        .create_command_encoder(&Default::default())
        .finish(&Default::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut canvas = shared_canvas_tests::CanvasCases::default();
    let mut finalizations = shared_canvas_tests::CanvasFinalizations::default();
    let mut sampling = shared_canvas_sampling_tests::CanvasSampling::default();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        core_device.push_error_scope(wgt::error::ErrorFilter::Validation);
        core_queue.submit(&[work.commands.clone()]);
        assert!(core_device.pop_error_scope().unwrap().is_none());
        let mapped = events.clone();
        work.readback.map_async(
            wgt::MapMode::Read,
            0,
            Some(512),
            Some(Box::new(move |status| {
                assert!(status.is_ok(), "{status:?}");
                mapped.lock().unwrap().push("map");
            })),
        );
        let completed = events.clone();
        core_queue.on_submitted_work_done(Box::new(move || {
            completed.lock().unwrap().push("work-done");
        }));
        provider.wait_native();
        core_device.poll(wgt::PollType::Poll).unwrap();
        assert!(events.lock().unwrap().is_empty());

        let native_counts = || {
            let counters = core_device.get_hal_counters();
            (counters.buffers.read(), counters.textures.read())
        };
        let before_destroy = native_counts();
        assert!(
            before_destroy.0 >= 2 && before_destroy.1 >= 1,
            "Live native counters required: {before_destroy:?}",
        );
        work.state.destroy();
        work.texture.destroy();
        assert_eq!(native_counts(), before_destroy);
        core_device.poll(wgt::PollType::Poll).unwrap();
        assert_eq!(native_counts(), before_destroy);
        assert!(events.lock().unwrap().is_empty());

        provider.release_receipts();
        core_device.poll(wgt::PollType::Poll).unwrap();
        let after_retirement = native_counts();
        assert!(
            after_retirement.0 < before_destroy.0 && after_retirement.1 < before_destroy.1,
            "Native resources did not retire: {before_destroy:?} -> {after_retirement:?}",
        );
        assert_eq!(*events.lock().unwrap(), ["map", "work-done"]);
        let bytes = work
            .readback
            .get_mapped_range(0, Some(512))
            .unwrap()
            .read_slice()
            .to_vec();
        assert_eq!(&bytes[..8], &[0, 255, 0, 255, 0, 255, 0, 255]);
        let words: Vec<_> = bytes[256..276]
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(words, [262, 18, 0, 255, 17]);
        work.readback.unmap();
        work.readback.destroy();

        let lanes = provider.lanes.lock().unwrap().clone();
        let graphics_index = lanes
            .iter()
            .position(|lane| *lane == hal::shared::SharedQueueLane::Graphics)
            .unwrap();
        assert!(lanes[..graphics_index].contains(&hal::shared::SharedQueueLane::Compute));
        assert!(lanes[graphics_index + 1..].contains(&hal::shared::SharedQueueLane::Compute));

        canvas.run(&core_device, &core_queue, &graphics, &provider);
        finalizations.run(&core_device, &core_queue, &graphics, &provider, &owner);
        sampling.run(&core_device, &core_queue, &graphics, &provider, &owner);

        let receipt_count = provider.receipts.lock().unwrap().len();
        core_queue.submit(&[pending.clone()]);
        assert!(provider.receipts.lock().unwrap().len() > receipt_count);
        let completed = events.clone();
        core_queue.on_submitted_work_done(Box::new(move || {
            completed.lock().unwrap().push("destroy-work-done");
        }));
        let lost = events.clone();
        core_device.set_device_lost_closure(Box::new(move |reason, _| {
            assert_eq!(reason, wgt::DeviceLostReason::Destroyed);
            lost.lock().unwrap().push("device-lost");
        }));
        core_device.destroy();
        provider.wait_native();
        core_device.poll(wgt::PollType::Poll).unwrap();
        assert_eq!(*events.lock().unwrap(), ["map", "work-done"]);
        provider.release_receipts();
        core_device.poll(wgt::PollType::Poll).unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["map", "work-done", "destroy-work-done", "device-lost"],
        );
        assert_eq!(provider.recoveries.load(Ordering::SeqCst), 0);
        submission.submit().unwrap();
    }));
    let gate_release = finalizations.release_gate();
    let sampling_release = sampling.release_gate();
    provider.release_receipts();
    let retired = match (&gate_release, &sampling_release) {
        (Ok(_), Ok(_)) => context.drain(),
        _ => Err(hal::DeviceError::Unexpected),
    };
    if retired.is_err() || outcome.is_err() {
        std::mem::forget((
            context,
            graphics,
            owner,
            target,
            submission,
            readback,
            work,
            pending,
            canvas,
            finalizations,
            sampling,
            core_queue,
            core_device,
            adapter,
            core_instance,
            provider,
        ));
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
        panic!("Shared proof retirement unconfirmed: {retired:?}, controllers: {gate_release:?}, {sampling_release:?}");
    }
    let pixels = unsafe {
        let mapping = owner.open.device.map_buffer(&**readback, 0..pitch).unwrap();
        if !mapping.is_coherent {
            owner
                .open
                .device
                .invalidate_mapped_ranges(&**readback, &[0..pitch]);
        }
        let pixels = std::slice::from_raw_parts(mapping.ptr.as_ptr(), 8).to_vec();
        owner.open.device.unmap_buffer(&**readback);
        pixels
    };
    drop((work, pending, canvas, finalizations, sampling));
    drop((core_queue, core_device, adapter, core_instance, provider));
    drop((submission, readback, target, owner, graphics, context));
    assert_eq!(pixels, [255, 0, 0, 255, 0, 0, 0, 255]);
    assert_eq!(wr_recoveries.get(), 0);
    assert_eq!(tests::ERRORS.load(Ordering::Relaxed), 0);
}
