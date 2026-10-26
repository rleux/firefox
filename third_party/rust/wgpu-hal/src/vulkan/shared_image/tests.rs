use super::*;
use crate::Instance as _;
use alloc::{vec, vec::Vec};
use ash::vk;

fn descriptor() -> crate::TextureDescriptor<'static> {
    crate::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 2,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::COPY_SRC
            | wgt::TextureUses::COPY_DST
            | wgt::TextureUses::RESOURCE
            | wgt::TextureUses::COLOR_TARGET,
        memory_flags: crate::MemoryFlags::empty(),
        view_formats: vec![wgt::TextureFormat::Rgba8UnormSrgb],
    }
}

#[test]
fn alias_descriptor_preserves_storage_shape_and_permitted_views() {
    let physical = descriptor();
    let mut alias = physical.clone();
    alias.label = Some("independent label");
    alias.usage = wgt::TextureUses::RESOURCE;
    alias.view_formats.clear();
    assert!(compatible(&physical, &alias));
    for field in 0..9 {
        let mut other = alias.clone();
        match field {
            0 => other.size.width += 1,
            1 => other.size.depth_or_array_layers += 1,
            2 => other.mip_level_count += 1,
            3 => other.sample_count *= 4,
            4 => other.dimension = wgt::TextureDimension::D3,
            5 => other.format = wgt::TextureFormat::Bgra8Unorm,
            6 => other.memory_flags = crate::MemoryFlags::TRANSIENT,
            7 => other.usage |= wgt::TextureUses::STORAGE_WRITE_ONLY,
            _ => other.view_formats.push(wgt::TextureFormat::Bgra8UnormSrgb),
        }
        assert!(!compatible(&physical, &other), "descriptor field {field}");
    }
}

fn factory() -> (
    crate::vulkan::Instance,
    crate::ExposedAdapter<super::super::Api>,
    crate::shared::SharedDeviceEndpoints,
) {
    let counter = wgt::InternalCounter::new();
    counter.add(1);
    assert_eq!(
        counter.read(),
        1,
        "Shared image tests require --features wgpu-types/counters"
    );
    assert!(cfg!(all(feature = "validation_canary", debug_assertions)));
    let instance = unsafe {
        crate::vulkan::Instance::init(&crate::InstanceDescriptor {
            name: "HAL shared image tests",
            flags: wgt::InstanceFlags::VALIDATION | wgt::InstanceFlags::DEBUG,
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            telemetry: None,
            display: None,
        })
    }
    .unwrap();
    assert!(instance
        .shared
        .debug_utils
        .as_ref()
        .is_some_and(|debug| { debug.callback_data.validation_layer_properties.is_some() }));
    let adapter = unsafe { instance.enumerate_adapters(None) }
        .into_iter()
        .find(|adapter| adapter.info.device_type != wgt::DeviceType::Cpu)
        .unwrap();
    let pair = unsafe {
        crate::shared::open_shared_device(
            &adapter.adapter,
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .unwrap();
    (instance, adapter, pair)
}

fn counts(device: &dyn crate::DynDevice) -> (isize, isize) {
    let counters = device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

struct Alias {
    owner: Arc<crate::DynOpenDevice>,
    texture: Option<Box<dyn crate::DynTexture>>,
}
impl Alias {
    fn raw(&self) -> &dyn crate::DynTexture {
        &**self.texture.as_ref().unwrap()
    }
}
impl Drop for Alias {
    fn drop(&mut self) {
        if let Some(texture) = self.texture.take() {
            unsafe { self.owner.device.destroy_texture(texture) };
        }
    }
}

fn alias(
    owner: &Arc<crate::DynOpenDevice>,
    lease: &Arc<dyn ImageLease>,
    desc: &crate::TextureDescriptor<'_>,
) -> Alias {
    let texture =
        unsafe { crate::shared_image::create_alias(lease.clone(), &*owner.device, desc) }.unwrap();
    let native = texture
        .as_any()
        .downcast_ref::<super::super::Texture>()
        .unwrap();
    assert!(matches!(
        &native.memory,
        super::super::TextureMemory::External
    ));
    assert!(native.drop_guard.is_some());
    Alias {
        owner: owner.clone(),
        texture: Some(texture),
    }
}

fn validation_clean() {
    #[cfg(feature = "validation_canary")]
    {
        let errors = crate::VALIDATION_CANARY.get_and_reset();
        assert!(errors.is_empty(), "Vulkan validation errors: {errors:?}");
    }
}

#[test]
#[ignore = "Requires Vulkan, two queues, validation_canary and wgpu-types/counters"]
fn aliases_retain_graphics_allocator_and_balance_texture_counters() {
    for logical_last in [true, false] {
        let (_, _, pair) = factory();
        let graphics = Arc::new(pair.graphics);
        let compute = Arc::new(pair.compute);
        let mut desc = descriptor();
        if !logical_last {
            desc.format = wgt::TextureFormat::Bgra8Unorm;
            desc.view_formats = vec![wgt::TextureFormat::Bgra8UnormSrgb];
        }
        assert_eq!(counts(&*graphics.device), (0, 0));
        let lease = unsafe { crate::shared_image::allocate(graphics.clone(), &desc) }.unwrap();
        let allocated = counts(&*graphics.device);
        assert_eq!(allocated.0, 1);
        assert!(allocated.1 > 0);
        let graphics_alias = alias(&graphics, &lease, &desc);
        let logical_alias = alias(&compute, &lease, &desc);
        assert_eq!(counts(&*graphics.device), (2, allocated.1));
        assert_eq!(counts(&*compute.device), (1, 0));
        let weak = Arc::downgrade(&graphics);
        drop(graphics);
        drop(lease);
        if logical_last {
            drop(graphics_alias);
            let monitor = weak
                .upgrade()
                .expect("Logical alias must retain graphics allocator");
            assert_eq!(counts(&*monitor.device), allocated);
            drop(logical_alias);
            assert_eq!(counts(&*monitor.device), (0, 0));
            drop(monitor);
        } else {
            drop(logical_alias);
            let monitor = weak.upgrade().unwrap();
            assert_eq!(counts(&*monitor.device), (2, allocated.1));
            drop(graphics_alias);
            assert_eq!(counts(&*monitor.device), (0, 0));
            drop(monitor);
        }
        assert!(weak.upgrade().is_none());
        assert_eq!(counts(&*compute.device), (0, 0));
        drop(compute);
        drop(pair.context);
        validation_clean();
    }
}

#[test]
#[ignore = "Requires Vulkan, two queues, validation_canary and wgpu-types/counters"]
fn invalid_aliases_and_allocations_do_not_change_counters() {
    let (_, adapter, pair) = factory();
    let graphics = Arc::new(pair.graphics);
    let compute = Arc::new(pair.compute);
    let desc = descriptor();
    assert!(matches!(
        unsafe { crate::shared_image::allocate(compute.clone(), &desc) },
        Err(ImageLeaseError::InvalidEndpoint)
    ));
    let foreign = unsafe {
        crate::shared::open_shared_device(
            &adapter.adapter,
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .unwrap();
    let mismatched = Arc::new(crate::DynOpenDevice {
        device: foreign.graphics.device,
        queue: foreign.compute.queue,
    });
    assert!(matches!(
        unsafe { crate::shared_image::allocate(mismatched, &desc) },
        Err(ImageLeaseError::InvalidEndpoint)
    ));
    let mut feature = desc.clone();
    feature.format = wgt::TextureFormat::Bgra8Unorm;
    feature.view_formats.clear();
    feature.usage = wgt::TextureUses::STORAGE_WRITE_ONLY;
    assert!(matches!(
        unsafe { crate::shared_image::allocate(graphics.clone(), &feature) },
        Err(ImageLeaseError::InvalidDescriptor)
    ));
    assert_eq!(counts(&*graphics.device), (0, 0));
    let lease = unsafe { crate::shared_image::allocate(graphics.clone(), &desc) }.unwrap();
    let before = counts(&*graphics.device);
    for field in 0..4 {
        let mut bad = desc.clone();
        match field {
            0 => bad.size.width += 1,
            1 => bad.format = wgt::TextureFormat::Bgra8Unorm,
            2 => bad.view_formats.push(wgt::TextureFormat::Bgra8UnormSrgb),
            _ => bad.usage |= wgt::TextureUses::STORAGE_WRITE_ONLY,
        }
        assert!(matches!(
            unsafe { crate::shared_image::create_alias(lease.clone(), &*compute.device, &bad) },
            Err(ImageLeaseError::InvalidDescriptor)
        ));
        assert_eq!(counts(&*compute.device), (0, 0));
        assert_eq!(counts(&*graphics.device), before);
    }
    assert!(matches!(
        unsafe {
            crate::shared_image::create_alias(lease.clone(), &*foreign.compute.device, &desc)
        },
        Err(ImageLeaseError::IncompatibleDevice)
    ));
    assert_eq!(counts(&*foreign.compute.device), (0, 0));
    pair.context.drain().unwrap();
    assert!(matches!(
        unsafe { crate::shared_image::create_alias(lease.clone(), &*compute.device, &desc) },
        Err(ImageLeaseError::Closed)
    ));
    assert!(matches!(
        unsafe { crate::shared_image::allocate(graphics.clone(), &desc) },
        Err(ImageLeaseError::Closed)
    ));
    assert_eq!(counts(&*graphics.device), before);
    drop(lease);
    assert_eq!(counts(&*graphics.device), (0, 0));
    validation_clean();
}

struct Commands {
    owner: Arc<crate::DynOpenDevice>,
    encoder: Box<dyn crate::DynCommandEncoder>,
    buffers: Vec<Box<dyn crate::DynCommandBuffer>>,
    open: bool,
}
struct Resources {
    context: Arc<dyn crate::shared::SharedDeviceContext>,
    graphics: Arc<crate::DynOpenDevice>,
    compute: Arc<crate::DynOpenDevice>,
    aliases: Vec<Alias>,
    views: Vec<Box<dyn crate::DynTextureView>>,
    buffers: Vec<Box<dyn crate::DynBuffer>>,
    commands: Vec<Commands>,
    fence: Option<Box<dyn crate::DynFence>>,
    receipt: Option<Box<dyn crate::shared::SharedSubmissionReceipt>>,
}
struct Work(core::mem::ManuallyDrop<Resources>);
impl core::ops::Deref for Work {
    type Target = Resources;
    fn deref(&self) -> &Resources {
        &self.0
    }
}
impl core::ops::DerefMut for Work {
    fn deref_mut(&mut self) -> &mut Resources {
        &mut self.0
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        if !matches!(self.context.drain(), Ok(()) | Err(crate::DeviceError::Lost)) {
            if !std::thread::panicking() {
                panic!("Shared image GPU resources quarantined");
            }
            return;
        }
        let data = &mut *self.0;
        unsafe {
            for mut commands in data.commands.drain(..) {
                if commands.open {
                    commands.encoder.discard_encoding();
                }
                commands
                    .encoder
                    .reset_all(core::mem::take(&mut commands.buffers));
                drop(commands.encoder);
            }
            data.receipt.take();
            for view in data.views.drain(..) {
                data.graphics.device.destroy_texture_view(view);
            }
            data.aliases.clear();
            for buffer in data.buffers.drain(..) {
                data.compute.device.destroy_buffer(buffer);
            }
            if let Some(fence) = data.fence.take() {
                data.compute.device.destroy_fence(fence);
            }
            core::mem::ManuallyDrop::drop(&mut self.0);
        }
    }
}

#[test]
#[ignore = "Requires Vulkan, two queues, validation_canary and wgpu-types/counters"]
fn shared_image_aliases_transfer_between_endpoints_and_preserve_srgb_views() {
    let (_, _, pair) = factory();
    let graphics = Arc::new(pair.graphics);
    let compute = Arc::new(pair.compute);
    let fence = unsafe { compute.device.create_fence() }.unwrap();
    let mut work = Work(core::mem::ManuallyDrop::new(Resources {
        context: pair.context,
        graphics,
        compute,
        aliases: Vec::new(),
        views: Vec::new(),
        buffers: Vec::new(),
        commands: Vec::new(),
        fence: Some(fence),
        receipt: None,
    }));
    let desc = descriptor();
    let lease = unsafe { crate::shared_image::allocate(work.graphics.clone(), &desc) }.unwrap();
    let graphics_alias = alias(&work.graphics, &lease, &desc);
    let logical_alias = alias(&work.compute, &lease, &desc);
    work.aliases.extend([graphics_alias, logical_alias]);
    let range = wgt::ImageSubresourceRange {
        aspect: wgt::TextureAspect::All,
        base_mip_level: 0,
        mip_level_count: Some(1),
        base_array_layer: 0,
        array_layer_count: Some(1),
    };
    let view = unsafe {
        work.graphics.device.create_texture_view(
            work.aliases[0].raw(),
            &crate::TextureViewDescriptor {
                label: None,
                format: wgt::TextureFormat::Rgba8UnormSrgb,
                dimension: wgt::TextureViewDimension::D2,
                usage: wgt::TextureUses::COLOR_TARGET,
                range,
                swizzle: Default::default(),
            },
        )
    }
    .unwrap();
    work.views.push(view);
    drop(lease);
    let native = work
        .compute
        .device
        .as_any()
        .downcast_ref::<super::super::Device>()
        .unwrap();
    let properties = unsafe {
        native
            .shared
            .instance
            .raw
            .get_physical_device_properties(native.shared.physical_device)
    };
    let alignment = properties
        .limits
        .optimal_buffer_copy_row_pitch_alignment
        .max(4);
    let pitch = 8u64.div_ceil(alignment) * alignment;
    for usage in [
        wgt::BufferUses::MAP_WRITE | wgt::BufferUses::COPY_SRC,
        wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
    ] {
        let (buffer, _) = unsafe {
            work.compute.device.create_buffer(&crate::BufferDescriptor {
                label: None,
                size: pitch,
                usage,
                memory_flags: crate::MemoryFlags::PREFER_COHERENT,
            })
        }
        .unwrap();
        work.buffers.push(buffer);
    }
    const PIXELS: [u8; 8] = [17, 51, 102, 255, 204, 153, 68, 255];
    unsafe {
        let mapping = work
            .compute
            .device
            .map_buffer(&*work.buffers[0], 0..pitch)
            .unwrap();
        core::ptr::copy_nonoverlapping(PIXELS.as_ptr(), mapping.ptr.as_ptr(), PIXELS.len());
        if !mapping.is_coherent {
            work.compute
                .device
                .flush_mapped_ranges(&*work.buffers[0], &[0..pitch]);
        }
        work.compute.device.unmap_buffer(&*work.buffers[0]);
    }
    let copy = crate::BufferTextureCopy {
        buffer_layout: wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(pitch as u32),
            rows_per_image: None,
        },
        texture_base: crate::TextureCopyBase {
            mip_level: 0,
            array_layer: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: crate::FormatAspects::COLOR,
        },
        size: desc.size.into(),
    };
    for (index, endpoint) in [
        work.compute.clone(),
        work.graphics.clone(),
        work.compute.clone(),
    ]
    .into_iter()
    .enumerate()
    {
        let encoder = unsafe {
            endpoint
                .device
                .create_command_encoder(&crate::CommandEncoderDescriptor {
                    label: None,
                    queue: &*endpoint.queue,
                })
        }
        .unwrap();
        work.commands.push(Commands {
            owner: endpoint,
            encoder,
            buffers: Vec::new(),
            open: false,
        });
        let data = &mut *work.0;
        let commands = &mut data.commands[index];
        unsafe { commands.encoder.begin_encoding(None) }.unwrap();
        commands.open = true;
        let encoder = &mut commands.encoder;
        unsafe {
            match index {
                0 => {
                    encoder.transition_buffers(&[crate::BufferBarrier {
                        buffer: &*data.buffers[0],
                        usage: crate::StateTransition {
                            from: wgt::BufferUses::MAP_WRITE,
                            to: wgt::BufferUses::COPY_SRC,
                        },
                    }]);
                    encoder.transition_textures(&[crate::TextureBarrier {
                        texture: data.aliases[1].raw(),
                        range,
                        usage: crate::StateTransition {
                            from: wgt::TextureUses::UNINITIALIZED,
                            to: wgt::TextureUses::COPY_DST,
                        },
                        queue_family_ownership_transfer: None,
                    }]);
                    encoder.copy_buffer_to_texture(
                        &*data.buffers[0],
                        data.aliases[1].raw(),
                        &[copy.clone()],
                    );
                }
                1 => {
                    encoder.transition_textures(&[crate::TextureBarrier {
                        texture: data.aliases[0].raw(),
                        range,
                        usage: crate::StateTransition {
                            from: wgt::TextureUses::COPY_DST,
                            to: wgt::TextureUses::COLOR_TARGET,
                        },
                        queue_family_ownership_transfer: None,
                    }]);
                    encoder
                        .begin_render_pass(&crate::RenderPassDescriptor {
                            label: None,
                            extent: desc.size,
                            sample_count: 1,
                            color_attachments: &[Some(crate::ColorAttachment {
                                target: crate::Attachment {
                                    view: &*data.views[0],
                                    usage: wgt::TextureUses::COLOR_TARGET,
                                },
                                depth_slice: None,
                                resolve_target: None,
                                ops: crate::AttachmentOps::LOAD | crate::AttachmentOps::STORE,
                                clear_value: wgt::Color::BLACK,
                            })],
                            depth_stencil_attachment: None,
                            multiview_mask: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        })
                        .unwrap();
                    let native = encoder
                        .as_any_mut()
                        .downcast_mut::<super::super::CommandEncoder>()
                        .unwrap();
                    native.device.raw.cmd_clear_attachments(
                        native.active,
                        &[vk::ClearAttachment {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            color_attachment: 0,
                            clear_value: vk::ClearValue {
                                color: vk::ClearColorValue {
                                    float32: [1.0, 0.0, 0.0, 1.0],
                                },
                            },
                        }],
                        &[vk::ClearRect {
                            rect: vk::Rect2D {
                                offset: vk::Offset2D { x: 1, y: 0 },
                                extent: vk::Extent2D {
                                    width: 1,
                                    height: 1,
                                },
                            },
                            base_array_layer: 0,
                            layer_count: 1,
                        }],
                    );
                    encoder.end_render_pass();
                }
                _ => {
                    encoder.transition_textures(&[crate::TextureBarrier {
                        texture: data.aliases[1].raw(),
                        range,
                        usage: crate::StateTransition {
                            from: wgt::TextureUses::COLOR_TARGET,
                            to: wgt::TextureUses::COPY_SRC,
                        },
                        queue_family_ownership_transfer: None,
                    }]);
                    encoder.transition_buffers(&[crate::BufferBarrier {
                        buffer: &*data.buffers[1],
                        usage: crate::StateTransition {
                            from: wgt::BufferUses::empty(),
                            to: wgt::BufferUses::COPY_DST,
                        },
                    }]);
                    encoder.copy_texture_to_buffer(
                        data.aliases[1].raw(),
                        wgt::TextureUses::COPY_SRC,
                        &*data.buffers[1],
                        &[copy.clone()],
                    );
                    encoder.transition_buffers(&[crate::BufferBarrier {
                        buffer: &*data.buffers[1],
                        usage: crate::StateTransition {
                            from: wgt::BufferUses::COPY_DST,
                            to: wgt::BufferUses::MAP_READ,
                        },
                    }]);
                }
            }
        }
        let buffer = unsafe { commands.encoder.end_encoding() }.unwrap();
        commands.open = false;
        commands.buffers.push(buffer);
    }
    let result = unsafe {
        work.context.submit_ordered(
            &*work.compute.queue,
            &[
                crate::shared::SharedSubmitSegment {
                    lane: crate::shared::SharedQueueLane::Compute,
                    queue: &*work.commands[0].owner.queue,
                    command_buffers: &[&*work.commands[0].buffers[0]],
                    synchronization: None,
                },
                crate::shared::SharedSubmitSegment {
                    lane: crate::shared::SharedQueueLane::Graphics,
                    queue: &*work.commands[1].owner.queue,
                    command_buffers: &[&*work.commands[1].buffers[0]],
                    synchronization: None,
                },
                crate::shared::SharedSubmitSegment {
                    lane: crate::shared::SharedQueueLane::Compute,
                    queue: &*work.commands[2].owner.queue,
                    command_buffers: &[&*work.commands[2].buffers[0]],
                    synchronization: None,
                },
            ],
            (work.fence.as_deref().unwrap(), 1),
        )
    };
    match result {
        Ok(receipt) => work.receipt = Some(receipt),
        Err(error) => {
            work.receipt = error.receipt;
            panic!("Alias transfer submission failed: {:?}", error.error);
        }
    }
    assert!(work
        .receipt
        .as_ref()
        .unwrap()
        .wait(Some(core::time::Duration::from_secs(10)))
        .unwrap());
    unsafe {
        let mapping = work
            .compute
            .device
            .map_buffer(&*work.buffers[1], 0..pitch)
            .unwrap();
        if !mapping.is_coherent {
            work.compute
                .device
                .invalidate_mapped_ranges(&*work.buffers[1], &[0..pitch]);
        }
        let pixels = core::slice::from_raw_parts(mapping.ptr.as_ptr(), PIXELS.len()).to_vec();
        work.compute.device.unmap_buffer(&*work.buffers[1]);
        assert_eq!(pixels, [17, 51, 102, 255, 255, 0, 0, 255]);
    }
    drop(work);
    validation_clean();
}

#[test]
#[ignore = "Requires Vulkan, two queues, validation_canary and wgpu-types/counters"]
fn alias_validation_requires_exact_sampling_allocation_and_graphics_endpoint() {
    let (_, adapter, pair) = factory();
    let graphics = Arc::new(pair.graphics);
    let compute = Arc::new(pair.compute);
    let desc = descriptor();
    let lease = unsafe { crate::shared_image::allocate(graphics.clone(), &desc) }.unwrap();
    let texture = alias(&compute, &lease, &desc);
    crate::shared_image::validate_alias(
        lease.as_ref(),
        &*compute.device,
        texture.raw(),
        &*graphics.queue,
        &desc,
    )
    .unwrap();
    let other = unsafe { crate::shared_image::allocate(graphics.clone(), &desc) }.unwrap();
    assert!(crate::shared_image::validate_alias(
        other.as_ref(),
        &*compute.device,
        texture.raw(),
        &*graphics.queue,
        &desc
    )
    .is_err());
    let mut wrong = desc.clone();
    wrong.mip_level_count = 2;
    assert!(crate::shared_image::validate_alias(
        lease.as_ref(),
        &*compute.device,
        texture.raw(),
        &*graphics.queue,
        &wrong
    )
    .is_err());
    assert!(crate::shared_image::validate_alias(
        lease.as_ref(),
        &*compute.device,
        texture.raw(),
        &*compute.queue,
        &desc
    )
    .is_err());
    let native = graphics
        .device
        .as_any()
        .downcast_ref::<super::super::Device>()
        .unwrap();
    let context = pair.context.clone();
    let duplicate = unsafe {
        adapter.adapter.device_from_raw_shared(
            native.shared.raw.clone(),
            Some(Box::new(move || drop(context))),
            &native.shared.enabled_extensions,
            native.shared.features,
            &adapter.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
            0,
            0,
            native.shared.shared_queue_group.clone(),
        )
    }
    .unwrap();
    assert!(crate::shared_image::validate_alias(
        lease.as_ref(),
        &*compute.device,
        texture.raw(),
        &duplicate.queue,
        &desc
    )
    .is_err());
    let mut unsampled = desc.clone();
    unsampled.usage.remove(wgt::TextureUses::RESOURCE);
    let unsampled_lease =
        unsafe { crate::shared_image::allocate(graphics.clone(), &unsampled) }.unwrap();
    let unsampled_alias = alias(&compute, &unsampled_lease, &unsampled);
    assert!(crate::shared_image::validate_alias(
        unsampled_lease.as_ref(),
        &*compute.device,
        unsampled_alias.raw(),
        &*graphics.queue,
        &unsampled
    )
    .is_err());
    pair.context.drain().unwrap();
    assert!(matches!(
        crate::shared_image::validate_alias(
            lease.as_ref(),
            &*compute.device,
            texture.raw(),
            &*graphics.queue,
            &desc
        ),
        Err(ImageLeaseError::Closed)
    ));
    drop((texture, unsampled_alias, lease, other, unsampled_lease));
    assert_eq!(counts(&*graphics.device), (0, 0));
    assert_eq!(counts(&*compute.device), (0, 0));
    validation_clean();
}
