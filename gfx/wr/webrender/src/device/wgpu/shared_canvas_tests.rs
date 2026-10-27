/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

struct CanvasCase {
    texture: Arc<core::resource::Texture>,
    lease: Option<Arc<dyn hal::shared_image::ImageLease>>,
    readback: Arc<core::resource::Buffer>,
    partial: bool,
    pixel: [u8; 4],
}

#[derive(Default)]
pub(super) struct CanvasCases {
    cases: Vec<CanvasCase>,
}

fn graphics_counts(graphics: &hal::DynOpenDevice) -> (isize, isize) {
    let counters = graphics.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

fn logical_counts(device: &core::device::Device) -> (isize, isize) {
    let counters = device.get_hal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

impl CanvasCases {
    pub(super) fn run(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
    ) {
        let counter = wgt::InternalCounter::new();
        counter.add(1);
        assert_eq!(
            counter.read(),
            1,
            "Canvas tests require wgpu-types/counters"
        );
        let graphics_before = graphics_counts(graphics);
        let logical_before = logical_counts(device);
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        let size = wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        };
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for render_pass in [true, false] {
                for partial in [false, true] {
                    let mut usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
                    if render_pass {
                        usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
                    }
                    let desc = core::resource::TextureDescriptor {
                        label: Some(Cow::Borrowed("native canvas initialization")),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgt::TextureDimension::D2,
                        format,
                        usage,
                        view_formats: vec![format.add_srgb_suffix()],
                    };
                    let (texture, lease) =
                        unsafe { device.create_texture_with_shared_image(&desc, graphics.clone()) };
                    let lease = lease.expect("Native canvas allocation must succeed");
                    assert_eq!(texture.descriptor().usage, usage);
                    assert_eq!(texture.descriptor().view_formats, desc.view_formats);
                    let readback = device.create_buffer(&core::resource::BufferDescriptor {
                        label: None,
                        size: 512,
                        usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    });
                    self.cases.push(CanvasCase {
                        texture,
                        lease: Some(lease),
                        readback,
                        partial,
                        pixel: match format {
                            wgt::TextureFormat::Rgba8Unorm => [17, 51, 102, 255],
                            _ => [102, 51, 17, 255],
                        },
                    });
                }
            }
        }
        let allocated = graphics_counts(graphics);
        assert_eq!(allocated.0, graphics_before.0 + 8);
        assert!(allocated.1 > graphics_before.1);
        assert_eq!(
            logical_counts(device),
            (logical_before.0 + 8, logical_before.1)
        );

        let encoder = device.create_command_encoder(&Default::default());
        for case in &self.cases {
            if case.partial {
                queue.write_texture(
                    wgt::TexelCopyTextureInfo {
                        texture: case.texture.clone(),
                        mip_level: 0,
                        origin: wgt::Origin3d { x: 1, y: 1, z: 0 },
                        aspect: wgt::TextureAspect::All,
                    },
                    &case.pixel,
                    &wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: None,
                        rows_per_image: None,
                    },
                    &wgt::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            encoder.copy_texture_to_buffer(
                &wgt::TexelCopyTextureInfo {
                    texture: case.texture.clone(),
                    mip_level: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &wgt::TexelCopyBufferInfo {
                    buffer: case.readback.clone(),
                    layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: None,
                    },
                },
                &size,
            );
        }
        let commands = encoder.finish(&Default::default());
        let submission = queue.submit(&[commands])
            .expect("Valid test copy must be submitted");
        let mapped = Arc::new(Mutex::new(Vec::new()));
        for case in &self.cases {
            let mapped = mapped.clone();
            case.readback.map_async(
                wgt::MapMode::Read,
                0,
                Some(512),
                Some(Box::new(move |result| mapped.lock().unwrap().push(result))),
            );
        }
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
        assert!(device.pop_error_scope().unwrap().is_none());
        let mapped = mapped.lock().unwrap();
        assert_eq!(mapped.len(), 8);
        assert!(
            mapped.iter().all(Result::is_ok),
            "Canvas mapping failed: {mapped:?}"
        );
        drop(mapped);
        for case in &self.cases {
            let bytes = case
                .readback
                .get_mapped_range(0, Some(512))
                .unwrap()
                .read_slice()
                .to_vec();
            for (index, offset) in [0, 4, 256, 260].iter().copied().enumerate() {
                let expected = if case.partial && index == 3 {
                    case.pixel
                } else {
                    [0; 4]
                };
                assert_eq!(
                    &bytes[offset..offset + 4],
                    &expected,
                    "Canvas {:?}, usage {:?}, partial {}, pixel {index}",
                    case.texture.descriptor().format,
                    case.texture.descriptor().usage,
                    case.partial,
                );
            }
            case.readback.unmap();
            case.readback.destroy();
            case.texture.destroy();
        }
        device.poll(wgt::PollType::Poll).unwrap();
        assert_eq!(logical_counts(device), logical_before);
        assert_eq!(graphics_counts(graphics), allocated);
        for case in &mut self.cases {
            drop(case.lease.take());
        }
        assert_eq!(graphics_counts(graphics), graphics_before);
    }
}

pub(super) unsafe fn submit_with_gate(
    provider: &GraphicsProvider,
    queue: &dyn hal::DynQueue,
    segments: &[hal::shared::SharedSubmitSegment<'_>],
    terminal: (&dyn hal::DynFence, hal::FenceValue),
) -> Result<Box<dyn hal::shared::SharedSubmissionReceipt>, hal::shared::SharedSubmitError> {
    let wait = provider.native_wait.lock().unwrap().take();
    if let Some(wait) = wait {
        let mut synchronization: Vec<_> = segments
            .iter()
            .map(|segment| segment.synchronization.cloned().unwrap_or_default())
            .collect();
        synchronization[0].waits.push(wait);
        let gated: Vec<_> = segments
            .iter()
            .zip(&synchronization)
            .map(
                |(segment, synchronization)| hal::shared::SharedSubmitSegment {
                    lane: segment.lane,
                    queue: segment.queue,
                    command_buffers: segment.command_buffers,
                    synchronization: Some(synchronization),
                },
            )
            .collect();
        unsafe { provider.context.submit_ordered(queue, &gated, terminal) }
    } else {
        unsafe { provider.context.submit_ordered(queue, segments, terminal) }
    }
}

pub(super) struct NativeGate {
    _control: Rc<Device>,
    _producer: Arc<dyn hal::external_sync::Semaphore>,
    pub(super) imported: Arc<dyn hal::external_sync::Semaphore>,
    release: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<Result<bool, String>>>,
    result: Option<Result<bool, String>>,
}

impl NativeGate {
    pub(super) fn new(graphics: &hal::DynOpenDevice) -> Self {
        let control = Rc::new(
            Device::new(&Options {
                validation: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let producer = hal::external_sync::create_timeline(&*control.open.device, true).unwrap();
        let handle = hal::external_sync::export_timeline(&*producer).unwrap();
        let imported = hal::external_sync::import_timeline(&*graphics.device, &handle).unwrap();
        let owner = control.open.clone();
        let timeline = producer.clone();
        let fence = unsafe { owner.device.create_fence() }.unwrap();
        let (release, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            struct UnconfirmedController;
            impl Drop for UnconfirmedController {
                fn drop(&mut self) {
                    let _ = std::io::Write::write_all(
                        &mut std::io::stderr(),
                        b"Controller retirement unconfirmed; exiting isolated test with failure 88\n",
                    );
                    unsafe extern "C" {
                        fn _exit(code: i32) -> !;
                    }
                    unsafe { _exit(88) }
                }
            }
            let unconfirmed = UnconfirmedController;
            struct Resources {
                owner: Arc<hal::DynOpenDevice>,
                timeline: Arc<dyn hal::external_sync::Semaphore>,
                fence: Option<Box<dyn hal::DynFence>>,
            }
            let mut resources = std::mem::ManuallyDrop::new(Resources {
                owner,
                timeline,
                fence: Some(fence),
            });
            let explicit = receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok();
            let synchronization = hal::external_sync::SubmissionSync {
                waits: Vec::new(),
                signals: vec![hal::external_sync::SemaphoreValue {
                    semaphore: resources.timeline.clone(),
                    value: Some(1),
                }],
            };
            let submitted = unsafe {
                hal::external_sync::submit_legacy(
                    &*resources.owner.queue,
                    &[],
                    &[],
                    (&**resources.fence.as_ref().unwrap(), 1),
                    &synchronization,
                )
            };
            if let Err(error) = submitted {
                eprintln!("Controller submission ownership quarantined: {error:?}");
                return Err(format!("Controller submission failed: {error:?}"));
            }
            let retired = unsafe {
                resources.owner.device.wait(
                    &**resources.fence.as_ref().unwrap(),
                    1,
                    Some(std::time::Duration::from_secs(30)),
                )
            };
            match retired {
                Ok(true) => {
                    let fence = resources.fence.take().unwrap();
                    unsafe {
                        resources.owner.device.destroy_fence(fence);
                        std::mem::ManuallyDrop::drop(&mut resources);
                    }
                    std::mem::forget(unconfirmed);
                    Ok(explicit)
                }
                result => {
                    eprintln!("Controller retirement ownership quarantined: {result:?}");
                    Err(format!("Controller retirement failed: {result:?}"))
                }
            }
        });
        Self {
            _control: control,
            _producer: producer,
            imported,
            release: Some(release),
            worker: Some(worker),
            result: None,
        }
    }

    pub(super) fn release_signal(&self) -> std::sync::mpsc::Sender<()> {
        self.release.as_ref().unwrap().clone()
    }

    pub(super) fn release(&mut self) -> Result<bool, String> {
        if let Some(result) = &self.result {
            return result.clone();
        }
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        let result = self.worker.take().unwrap().join().unwrap_or_else(|_| {
            Err("Controller thread panicked; native ownership quarantined".into())
        });
        self.result = Some(result.clone());
        result
    }
}

struct FinalizedAlias {
    raw: resources::Owned<dyn hal::DynTexture>,
    _payload: hal::shared_image::ImagePayload,
}

struct FinalizedCanvas {
    texture: Arc<core::resource::Texture>,
    lease: Arc<dyn hal::shared_image::ImageLease>,
    epoch: Option<Arc<hal::shared_image::ImageEpoch>>,
    stale: Arc<core::command::CommandBuffer>,
    stale_readback: Arc<core::resource::Buffer>,
    alias: Option<Rc<FinalizedAlias>>,
    readback: Option<Rc<resources::Owned<dyn hal::DynBuffer>>>,
    consumer: Option<Submission>,
    pixel: [u8; 4],
    partial: bool,
}

#[derive(Default)]
pub(super) struct CanvasFinalizations {
    cases: Vec<FinalizedCanvas>,
    gate: Option<NativeGate>,
    marker: Option<Arc<core::resource::Buffer>>,
    sampled: Option<Arc<core::resource::Buffer>>,
}

impl CanvasFinalizations {
    pub(super) fn release_gate(&mut self) -> Result<bool, String> {
        self.gate.as_mut().map_or(Ok(true), NativeGate::release)
    }

    fn create(
        &mut self,
        device: &Arc<core::device::Device>,
        graphics: &Arc<hal::DynOpenDevice>,
        format: wgt::TextureFormat,
        render_pass: bool,
        partial: bool,
        sampled: bool,
    ) -> usize {
        let mut usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
        if render_pass {
            usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
        }
        if sampled {
            usage |= wgt::TextureUsages::TEXTURE_BINDING;
        }
        let desc = core::resource::TextureDescriptor {
            label: Some(Cow::Borrowed("native canvas finalization")),
            size: wgt::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format,
            usage,
            view_formats: Vec::new(),
        };
        let (texture, lease) =
            unsafe { device.create_texture_with_shared_image(&desc, graphics.clone()) };
        let lease = lease.expect("Native finalization allocation must succeed");
        let stale_readback = device.create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 512,
            usage: wgt::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            &wgt::TexelCopyTextureInfo {
                texture: texture.clone(),
                mip_level: 0,
                origin: wgt::Origin3d::ZERO,
                aspect: wgt::TextureAspect::All,
            },
            &wgt::TexelCopyBufferInfo {
                buffer: stale_readback.clone(),
                layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: None,
                },
            },
            &desc.size,
        );
        let stale = encoder.finish(&Default::default());
        self.cases.push(FinalizedCanvas {
            texture,
            lease,
            epoch: None,
            stale,
            stale_readback,
            alias: None,
            readback: None,
            consumer: None,
            pixel: if format == wgt::TextureFormat::Rgba8Unorm {
                [17, 51, 102, 255]
            } else {
                [102, 51, 17, 255]
            },
            partial,
        });
        self.cases.len() - 1
    }

    fn write_pixel(&self, index: usize, queue: &core::device::queue::Queue) {
        let case = &self.cases[index];
        queue.write_texture(
            wgt::TexelCopyTextureInfo {
                texture: case.texture.clone(),
                mip_level: 0,
                origin: wgt::Origin3d { x: 1, y: 1, z: 0 },
                aspect: wgt::TextureAspect::All,
            },
            &case.pixel,
            &wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: None,
                rows_per_image: None,
            },
            &wgt::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    }

    fn record_consumer(&mut self, index: usize, owner: &Rc<Device>, pitch: u64) {
        let case = &mut self.cases[index];
        let desc = hal::TextureDescriptor {
            label: Some("WR finalized canvas alias"),
            size: case.texture.descriptor().size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: case.texture.descriptor().format,
            usage: wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC,
            memory_flags: hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let epoch = case.epoch.as_ref().unwrap();
        let payload = epoch.acquire_payload().unwrap();
        let alias = unsafe {
            hal::shared_image::create_alias(epoch.lease().clone(), &*owner.open.device, &desc)
        }
        .unwrap();
        case.alias = Some(Rc::new(FinalizedAlias {
            raw: resources::Owned::new(owner, alias, <dyn hal::DynDevice>::destroy_texture),
            _payload: payload,
        }));
        let (readback, _) = unsafe {
            owner.open.device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR finalized canvas readback"),
                size: pitch * 2,
                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .unwrap();
        case.readback = Some(Rc::new(resources::Owned::new(
            owner,
            readback,
            <dyn hal::DynDevice>::destroy_buffer,
        )));
        case.consumer = Some(Submission::new(owner).unwrap());
        let alias = case.alias.as_ref().unwrap();
        let readback = case.readback.as_ref().unwrap();
        let mut commands = case.consumer.as_mut().unwrap().recording().unwrap();
        commands.keep(alias);
        commands.keep(readback);
        unsafe {
            let encoder = commands.encoder();
            encoder.transition_textures(&[hal::TextureBarrier {
                texture: &*alias.raw,
                range: wgt::ImageSubresourceRange::default(),
                usage: hal::StateTransition {
                    from: wgt::TextureUses::RESOURCE,
                    to: wgt::TextureUses::COPY_SRC,
                },
                queue_family_ownership_transfer: None,
            }]);
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &***readback,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::empty(),
                    to: wgt::BufferUses::COPY_DST,
                },
            }]);
            encoder.copy_texture_to_buffer(
                &*alias.raw,
                wgt::TextureUses::COPY_SRC,
                &***readback,
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
                    size: desc.size.into(),
                }],
            );
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &***readback,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_DST,
                    to: wgt::BufferUses::MAP_READ,
                },
            }]);
        }
    }

    fn verify(&mut self, index: usize, owner: &Rc<Device>, pitch: u64) {
        let case = &mut self.cases[index];
        assert!(case
            .consumer
            .as_mut()
            .unwrap()
            .wait(Some(std::time::Duration::from_secs(30)))
            .unwrap());
        let readback = case.readback.as_ref().unwrap();
        let bytes = unsafe {
            let mapping = owner
                .open
                .device
                .map_buffer(&***readback, 0..pitch * 2)
                .unwrap();
            if !mapping.is_coherent {
                owner
                    .open
                    .device
                    .invalidate_mapped_ranges(&***readback, &[0..pitch * 2]);
            }
            let bytes =
                std::slice::from_raw_parts(mapping.ptr.as_ptr(), (pitch * 2) as usize).to_vec();
            owner.open.device.unmap_buffer(&***readback);
            bytes
        };
        for (pixel, offset) in [0, 4, pitch as usize, pitch as usize + 4]
            .iter()
            .copied()
            .enumerate()
        {
            let expected = if case.partial && pixel == 3 {
                case.pixel
            } else {
                [0; 4]
            };
            assert_eq!(
                &bytes[offset..offset + 4],
                &expected,
                "Finalized {:?}, partial {}, pixel {pixel}",
                case.texture.descriptor().format,
                case.partial
            );
        }
    }
}

impl CanvasFinalizations {
    fn sample_before_finalization(
        &mut self,
        index: usize,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        provider: &GraphicsProvider,
    ) {
        use super::core::{binding_model as binding, command, pipeline};
        let shader = device.create_shader_module(
            &pipeline::ShaderModuleDescriptor {
                label: None,
                runtime_checks: Default::default(),
            },
            pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
                "@group(0) @binding(0) var canvas: texture_2d<f32>;
                 @group(0) @binding(1) var<storage, read_write> sampled: vec4f;
                 @compute @workgroup_size(1) fn main() {
                     sampled = textureLoad(canvas, vec2i(0, 0), 0);
                 }",
            )),
        );
        let pipeline = device.create_compute_pipeline(pipeline::ComputePipelineDescriptor {
            label: None,
            layout: None,
            stage: pipeline::ProgrammableStageDescriptor {
                module: shader,
                entry_point: Some(Cow::Borrowed("main")),
                constants: Default::default(),
                zero_initialize_workgroup_memory: true,
            },
            cache: None,
        });
        let sampled = device.create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 16,
            usage: wgt::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        self.sampled = Some(sampled.clone());
        let group = device.create_bind_group(&binding::BindGroupDescriptor {
            label: None,
            layout: pipeline.get_bind_group_layout(0),
            entries: Cow::Owned(vec![
                binding::BindGroupEntry {
                    binding: 0,
                    resource: binding::BindingResource::TextureView(
                        self.cases[index].texture.create_view(&Default::default()),
                    ),
                },
                binding::BindGroupEntry {
                    binding: 1,
                    resource: binding::BindingResource::Buffer(binding::BufferBinding {
                        buffer: sampled,
                        offset: 0,
                        size: Some(16),
                    }),
                },
            ]),
        });
        let encoder = device.create_command_encoder(&Default::default());
        let mut pass = encoder.begin_compute_pass(&command::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, Some(group), &[]);
        pass.dispatch_workgroups(1, 1, 1);
        pass.end();
        let submission = queue.submit(&[encoder.finish(&Default::default())])
            .expect("Valid test copy must be submitted");
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
    }

    pub(super) fn run(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
        owner: &Rc<Device>,
    ) {
        let graphics_before = graphics_counts(graphics);
        let logical_before = logical_counts(device);
        let alignment = owner
            .capabilities()
            .alignments
            .buffer_copy_pitch
            .get()
            .max(4);
        let pitch = 8u64.div_ceil(alignment) * alignment;
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for render_pass in [true, false] {
                for partial in [false, true] {
                    let index = self.create(device, graphics, format, render_pass, partial, false);
                    if partial {
                        self.write_pixel(index, queue);
                    }
                    let gated = format == wgt::TextureFormat::Bgra8Unorm && !render_pass && partial;
                    let lanes_before = provider.lanes.lock().unwrap().len();
                    if gated {
                        self.gate = Some(NativeGate::new(graphics));
                        *provider.native_wait.lock().unwrap() =
                            Some(hal::external_sync::SemaphoreValue {
                                semaphore: self.gate.as_ref().unwrap().imported.clone(),
                                value: Some(1),
                            });
                    }
                    let case = &self.cases[index];
                    let finalized =
                        unsafe { queue.finalize_shared_canvas(&case.texture, case.lease.clone()) }
                            .unwrap();
                    let submission = finalized.submission_index();
                    self.cases[index].epoch = Some(finalized.publish_epoch());
                    self.record_consumer(index, owner, pitch);
                    self.cases[index]
                        .consumer
                        .as_mut()
                        .unwrap()
                        .submit()
                        .unwrap();
                    if gated {
                        let producer_ready =
                            provider.receipts.lock().unwrap().last().map(|receipt| {
                                receipt
                                    .native
                                    .wait(Some(std::time::Duration::from_millis(50)))
                            });
                        let consumer_ready = self.cases[index]
                            .consumer
                            .as_mut()
                            .unwrap()
                            .wait(Some(std::time::Duration::from_millis(250)));
                        let explicit = self.release_gate().unwrap();
                        assert!(explicit, "Controller fallback released the native gate");
                        assert!(
                            matches!(producer_ready, Some(Ok(false))),
                            "Producer was not gated: {producer_ready:?}"
                        );
                        assert!(
                            matches!(consumer_ready, Ok(false)),
                            "Ordinary WR consumer overtook producer: {consumer_ready:?}"
                        );
                        let lanes = provider.lanes.lock().unwrap();
                        assert_eq!(lanes[lanes_before], hal::shared::SharedQueueLane::Compute);
                        assert_eq!(lanes.last(), Some(&hal::shared::SharedQueueLane::Graphics));
                        assert!(provider.native_wait.lock().unwrap().is_none());
                    }
                    self.verify(index, owner, pitch);
                    provider.release_receipts();
                    device
                        .poll(wgt::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: Some(std::time::Duration::from_secs(30)),
                        })
                        .unwrap();
                    device.push_error_scope(wgt::error::ErrorFilter::Validation);
                    queue.submit(&[self.cases[index].stale.clone()]);
                    let stale = device
                        .pop_error_scope()
                        .unwrap()
                        .expect("Expired canvas command submission must fail validation");
                    let message = stale.to_string();
                    assert!(
                        message.contains("native canvas finalization")
                            && message.contains("has been destroyed"),
                        "Unexpected stale submission error: {message}"
                    );
                    self.cases[index].stale_readback.destroy();
                }
            }
        }

        let index = self.create(
            device,
            graphics,
            wgt::TextureFormat::Rgba8Unorm,
            false,
            false,
            true,
        );
        self.sample_before_finalization(index, device, queue, provider);
        let marker = device.create_buffer(&core::resource::BufferDescriptor {
            label: Some(Cow::Borrowed("pending write beside RESOURCE canvas")),
            size: 4,
            usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.marker = Some(marker.clone());
        const MARKER: [u8; 4] = [201, 17, 33, 255];
        queue.write_buffer(marker.clone(), 0, &MARKER);
        let case = &self.cases[index];
        let finalized =
            unsafe { queue.finalize_shared_canvas(&case.texture, case.lease.clone()) }.unwrap();
        let submission = finalized.submission_index();
        self.cases[index].epoch = Some(finalized.publish_epoch());
        let mapped = Arc::new(Mutex::new(None));
        let callback = mapped.clone();
        let before_map = provider.receipts.lock().unwrap().len();
        marker.map_async(
            wgt::MapMode::Read,
            0,
            Some(4),
            Some(Box::new(move |result| {
                *callback.lock().unwrap() = Some(result)
            })),
        );
        let after_map = provider.receipts.lock().unwrap().len();
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
        assert!(matches!(*mapped.lock().unwrap(), Some(Ok(wgt::MapMode::Read))));
        let bytes = marker
            .get_mapped_range(0, Some(4))
            .unwrap()
            .read_slice()
            .to_vec();
        marker.unmap();
        assert_eq!(bytes, MARKER);
        self.record_consumer(index, owner, pitch);
        self.cases[index]
            .consumer
            .as_mut()
            .unwrap()
            .submit()
            .unwrap();
        self.verify(index, owner, pitch);
        assert_eq!(
            after_map, before_map,
            "Mapping masked an unflushed pending write"
        );
        self.cases[index].stale_readback.destroy();
        marker.destroy();
        self.sampled.as_ref().unwrap().destroy();
        device.poll(wgt::PollType::Poll).unwrap();
        assert!(device.pop_error_scope().unwrap().is_none());
        assert_eq!(logical_counts(device), logical_before);
        let retained = graphics_counts(graphics);
        assert_eq!(retained.0, graphics_before.0 + 18);
        assert!(retained.1 > graphics_before.1);
        self.cases.clear();
        assert_eq!(graphics_counts(graphics), graphics_before);
    }
}
