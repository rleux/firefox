use super::*;
use crate::Instance as _;
use alloc::vec;
use core::cell::{Cell, RefCell};

std::thread_local! {
    static FAIL_AFTER_GRAPHICS: Cell<bool> = const { Cell::new(false) };
    static FAILED_GROUP: RefCell<Option<Arc<QueueGroup>>> = const { RefCell::new(None) };
}

pub(super) fn after_graphics(device: &crate::vulkan::Device) -> Result<(), SharedDeviceError> {
    if FAIL_AFTER_GRAPHICS.replace(false) {
        FAILED_GROUP.set(device.shared.shared_queue_group.clone());
        Err(crate::DeviceError::OutOfMemory.into())
    } else {
        Ok(())
    }
}

fn native_instance() -> crate::vulkan::Instance {
    assert!(
        cfg!(all(feature = "validation_canary", debug_assertions)),
        "Native shared-device tests require validation_canary and debug assertions"
    );
    let instance = unsafe {
        crate::vulkan::Instance::init(&crate::InstanceDescriptor {
            name: "HAL shared-device tests",
            flags: wgt::InstanceFlags::VALIDATION | wgt::InstanceFlags::DEBUG,
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            telemetry: None,
            display: None,
        })
    }
    .unwrap();
    assert!(
        instance
            .shared
            .debug_utils
            .as_ref()
            .is_some_and(|debug| debug.callback_data.validation_layer_properties.is_some()),
        "Khronos validation layer must be enabled"
    );
    instance
}

fn native_factory() -> Result<SharedDeviceEndpoints, SharedDeviceError> {
    let instance = native_instance();
    let adapter = unsafe { instance.enumerate_adapters(None) }
        .into_iter()
        .next()
        .unwrap();
    unsafe {
        crate::shared::open_shared_device(
            &adapter.adapter,
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    }
}

fn assert_validation_clean() {
    #[cfg(feature = "validation_canary")]
    {
        let errors = crate::VALIDATION_CANARY.get_and_reset();
        assert!(errors.is_empty(), "Vulkan validation errors: {errors:?}");
    }
}

fn native_group(pair: &SharedDeviceEndpoints) -> Arc<QueueGroup> {
    pair.graphics
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap()
        .shared
        .shared_queue_group
        .as_ref()
        .unwrap()
        .clone()
}

#[test]
fn shared_queue_selection_requires_two_universal_queues_in_family_zero() {
    let family = |count, flags| vk::QueueFamilyProperties {
        queue_count: count,
        queue_flags: flags,
        ..Default::default()
    };
    let universal = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE;
    assert!(!supports_independent_queues(&[]));
    assert!(!supports_independent_queues(&[family(1, universal)]));
    assert!(!supports_independent_queues(&[family(
        2,
        vk::QueueFlags::COMPUTE
    )]));
    assert!(!supports_independent_queues(&[
        family(1, universal),
        family(2, universal)
    ]));
    assert!(supports_independent_queues(&[family(2, universal)]));
}

#[test]
fn shared_identity_is_unique_per_native_owner() {
    let a = SharedDeviceIdentity::new();
    assert_eq!(a, a.clone());
    assert_ne!(a, SharedDeviceIdentity::new());
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_native_device_uses_independent_queues_and_cross_endpoint_resources() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let graphics = pair
        .graphics
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let compute = pair
        .compute
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    assert_eq!(graphics.shared.raw.handle(), compute.shared.raw.handle());
    assert_eq!(
        graphics.shared.physical_device,
        compute.shared.physical_device
    );
    assert!(Arc::ptr_eq(
        &graphics.shared.instance,
        &compute.shared.instance
    ));
    assert_ne!(graphics.shared.raw_queue, compute.shared.raw_queue);
    assert_eq!(
        (graphics.shared.family_index, graphics.shared.queue_index),
        (0, 0)
    );
    assert_eq!(
        (compute.shared.family_index, compute.shared.queue_index),
        (0, 1)
    );

    let texture_desc = crate::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::COLOR_TARGET,
        memory_flags: crate::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let texture = unsafe { pair.compute.device.create_texture(&texture_desc) }.unwrap();
    let other_texture = unsafe { pair.graphics.device.create_texture(&texture_desc) }.unwrap();
    let view_desc = crate::TextureViewDescriptor {
        label: None,
        format: texture_desc.format,
        dimension: wgt::TextureViewDimension::D2,
        usage: wgt::TextureUses::COLOR_TARGET,
        range: wgt::ImageSubresourceRange {
            aspect: wgt::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: Some(1),
            base_array_layer: 0,
            array_layer_count: Some(1),
        },
        swizzle: Default::default(),
    };
    let view = unsafe {
        pair.compute
            .device
            .create_texture_view(&*texture, &view_desc)
    }
    .unwrap();
    let other_view = unsafe {
        pair.graphics
            .device
            .create_texture_view(&*other_texture, &view_desc)
    }
    .unwrap();
    let native_view = view
        .as_any()
        .downcast_ref::<crate::vulkan::TextureView>()
        .unwrap();
    let other_native_view = other_view
        .as_any()
        .downcast_ref::<crate::vulkan::TextureView>()
        .unwrap();
    assert_ne!(native_view.view_identity, other_native_view.view_identity);
    assert_ne!(
        native_view.texture_identity,
        other_native_view.texture_identity
    );

    let mut graphics_encoder = unsafe {
        pair.graphics
            .device
            .create_command_encoder(&crate::CommandEncoderDescriptor {
                label: None,
                queue: &*pair.graphics.queue,
            })
    }
    .unwrap();
    let mut compute_encoder = unsafe {
        pair.compute
            .device
            .create_command_encoder(&crate::CommandEncoderDescriptor {
                label: None,
                queue: &*pair.compute.queue,
            })
    }
    .unwrap();
    unsafe {
        graphics_encoder.begin_encoding(None).unwrap();
        graphics_encoder.transition_textures(&[crate::TextureBarrier {
            texture: &*texture,
            range: view_desc.range,
            usage: crate::StateTransition {
                from: wgt::TextureUses::UNINITIALIZED,
                to: wgt::TextureUses::COLOR_TARGET,
            },
            queue_family_ownership_transfer: None,
        }]);
        graphics_encoder
            .begin_render_pass(&crate::RenderPassDescriptor {
                label: None,
                extent: texture_desc.size,
                sample_count: 1,
                color_attachments: &[Some(crate::ColorAttachment {
                    target: crate::Attachment {
                        view: &*view,
                        usage: wgt::TextureUses::COLOR_TARGET,
                    },
                    depth_slice: None,
                    resolve_target: None,
                    ops: crate::AttachmentOps::LOAD_CLEAR | crate::AttachmentOps::STORE,
                    clear_value: wgt::Color::RED,
                })],
                depth_stencil_attachment: None,
                multiview_mask: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .unwrap();
        graphics_encoder.end_render_pass();
        compute_encoder.begin_encoding(None).unwrap();
    }
    let graphics_buffer = unsafe { graphics_encoder.end_encoding() }.unwrap();
    let compute_buffer = unsafe { compute_encoder.end_encoding() }.unwrap();
    let graphics_fence = unsafe { pair.graphics.device.create_fence() }.unwrap();
    let compute_fence = unsafe { pair.compute.device.create_fence() }.unwrap();
    let submitted_graphics = unsafe {
        pair.graphics
            .queue
            .submit(&[&*graphics_buffer], &[], (&*graphics_fence, 1))
    };
    let submitted_compute = unsafe {
        pair.compute
            .queue
            .submit(&[&*compute_buffer], &[], (&*compute_fence, 1))
    };
    let retired = pair.context.drain();
    if let Err(error) = retired {
        core::mem::forget((
            pair,
            graphics_encoder,
            compute_encoder,
            graphics_buffer,
            compute_buffer,
            graphics_fence,
            compute_fence,
            texture,
            other_texture,
            view,
            other_view,
        ));
        panic!("Shared work retirement unconfirmed: {error:?}");
    }
    unsafe {
        graphics_encoder.reset_all(vec![graphics_buffer]);
        compute_encoder.reset_all(vec![compute_buffer]);
        pair.graphics.device.destroy_fence(graphics_fence);
        pair.compute.device.destroy_fence(compute_fence);
        pair.compute.device.destroy_texture_view(view);
        pair.graphics.device.destroy_texture_view(other_view);
        pair.compute.device.destroy_texture(texture);
        pair.graphics.device.destroy_texture(other_texture);
    }
    drop(graphics_encoder);
    drop(compute_encoder);
    drop(pair);
    assert!(submitted_graphics.is_ok(), "{submitted_graphics:?}");
    assert!(submitted_compute.is_ok(), "{submitted_compute:?}");
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_submit_error_closes_both_lanes_until_native_drain() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let mut encoder = unsafe {
        pair.graphics
            .device
            .create_command_encoder(&crate::CommandEncoderDescriptor {
                label: None,
                queue: &*pair.graphics.queue,
            })
    }
    .unwrap();
    unsafe { encoder.begin_encoding(None) }.unwrap();
    let buffer = unsafe { encoder.end_encoding() }.unwrap();
    let fence = unsafe { pair.graphics.device.create_fence() }.unwrap();
    let accepted = unsafe { pair.graphics.queue.submit(&[&*buffer], &[], (&*fence, 1)) };
    let simulated_error = group.submit::<()>(1, || Err(crate::DeviceError::OutOfMemory));
    let entered = Cell::new(false);
    let rejected_graphics = group.submit(0, || {
        entered.set(true);
        Ok(())
    });
    let rejected_compute = group.submit(1, || {
        entered.set(true);
        Ok(())
    });
    let retired = pair.context.drain();
    if let Err(error) = retired {
        core::mem::forget((pair, encoder, buffer, fence));
        panic!("Shared work retirement unconfirmed: {error:?}");
    }
    let waited_after_close = unsafe { pair.graphics.queue.wait_for_idle() };
    unsafe {
        encoder.reset_all(vec![buffer]);
        pair.graphics.device.destroy_fence(fence);
    }
    drop(encoder);
    drop(pair);
    assert!(accepted.is_ok(), "{accepted:?}");
    assert_eq!(simulated_error, Err(crate::DeviceError::OutOfMemory));
    assert_eq!(rejected_graphics, Err(crate::DeviceError::Unexpected));
    assert_eq!(rejected_compute, Err(crate::DeviceError::Unexpected));
    assert!(!entered.get());
    assert!(waited_after_close.is_ok(), "{waited_after_close:?}");
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_setup_failure_destroys_native_device_once() {
    FAIL_AFTER_GRAPHICS.set(true);
    let result = native_factory();
    assert!(matches!(
        result,
        Err(SharedDeviceError::Device(crate::DeviceError::OutOfMemory))
    ));
    let group = FAILED_GROUP.take().unwrap();
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_encoder_lease_outlives_both_endpoint_wrappers() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let encoder = unsafe {
        pair.graphics
            .device
            .create_command_encoder(&crate::CommandEncoderDescriptor {
                label: None,
                queue: &*pair.graphics.queue,
            })
    }
    .unwrap();
    pair.context.drain().unwrap();
    let SharedDeviceEndpoints {
        context,
        graphics,
        compute,
    } = pair;
    drop(graphics);
    drop(context);
    drop(compute);
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 0);
    drop(encoder);
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_instance_clone_outlives_native_device_context() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let graphics = pair
        .graphics
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let original_instance = Arc::downgrade(&graphics.shared.instance);
    let physical_device = graphics.shared.physical_device;
    let (backend, instance) = pair.context.clone_instance();
    assert_eq!(backend, wgt::Backend::Vulkan);
    let native = instance
        .as_any()
        .downcast_ref::<crate::vulkan::Instance>()
        .unwrap();
    assert!(Arc::ptr_eq(&native.shared, &graphics.shared.instance));
    pair.context.drain().unwrap();
    drop(pair);
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert!(original_instance.upgrade().is_some());

    let adapters = unsafe { instance.enumerate_adapters(None) };
    assert_eq!(adapters.len(), 1);
    assert!(adapters.iter().any(|adapter| {
        let adapter = adapter
            .adapter
            .as_any()
            .downcast_ref::<crate::vulkan::Adapter>()
            .unwrap();
        adapter.raw == physical_device && Arc::ptr_eq(&adapter.instance, &native.shared)
    }));
    drop(adapters);
    drop(instance);
    assert!(original_instance.upgrade().is_none());
    assert_validation_clean();
}

fn device_uuid(adapter: &crate::vulkan::Adapter) -> [u8; vk::UUID_SIZE] {
    assert!(adapter.instance.instance_api_version >= vk::API_VERSION_1_1);
    let mut ids = vk::PhysicalDeviceIDProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
    unsafe {
        adapter
            .instance
            .raw
            .get_physical_device_properties2(adapter.raw, &mut properties)
    };
    ids.device_uuid
}

#[test]
#[ignore = "Requires Vulkan 1.1, two universal queues and the Khronos validation layer"]
fn shared_compute_endpoint_rejects_foreign_instance_features_limits_and_closed_context() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let (_, selected_instance) = pair.context.clone_instance();
    let mut selected_adapters = unsafe { selected_instance.enumerate_adapters(None) };
    assert_eq!(selected_adapters.len(), 1);
    let selected = selected_adapters.pop().unwrap();
    let selected_native = selected
        .adapter
        .as_any()
        .downcast_ref::<crate::vulkan::Adapter>()
        .unwrap();
    let missing = crate::vulkan::Instance {
        shared: selected_native.instance.clone(),
        selected_physical_device: Some(vk::PhysicalDevice::null()),
        selected_features: None,
    };
    assert!(unsafe { missing.enumerate_adapters(None) }.is_empty());

    let foreign_instance = native_instance();
    assert!(foreign_instance.selected_physical_device.is_none());
    let selected_uuid = device_uuid(selected_native);
    assert_ne!(selected_uuid, [0; vk::UUID_SIZE]);
    let foreign = unsafe { foreign_instance.enumerate_adapters(None) }
        .into_iter()
        .find(|candidate| device_uuid(&candidate.adapter) == selected_uuid)
        .expect("The same physical GPU must be visible through the other instance");
    assert!(!Arc::ptr_eq(
        &foreign.adapter.instance,
        &selected_native.instance
    ));
    let hints = wgt::MemoryHints::default();
    let foreign_result = unsafe {
        pair.context.clone().open_compute_device(
            &foreign.adapter,
            wgt::Features::empty(),
            &selected.capabilities.limits,
            &hints,
        )
    };
    assert!(matches!(
        foreign_result,
        Err(SharedDeviceError::IncompatibleAdapter)
    ));
    let unsupported = unsafe {
        pair.context.clone().open_compute_device(
            &*selected.adapter,
            wgt::Features::TIMESTAMP_QUERY,
            &selected.capabilities.limits,
            &hints,
        )
    };
    assert!(matches!(
        unsupported,
        Err(SharedDeviceError::UnsupportedFeatures(_))
    ));
    let mut excessive = selected.capabilities.limits.clone();
    excessive.max_bind_groups = excessive.max_bind_groups.checked_add(1).unwrap();
    let limits_error = unsafe {
        pair.context.clone().open_compute_device(
            &*selected.adapter,
            wgt::Features::empty(),
            &excessive,
            &hints,
        )
    };
    assert!(matches!(
        limits_error,
        Err(SharedDeviceError::LimitsExceeded)
    ));
    for uniform in [true, false] {
        let mut insufficient = selected.capabilities.limits.clone();
        let alignment = if uniform {
            &mut insufficient.min_uniform_buffer_offset_alignment
        } else {
            &mut insufficient.min_storage_buffer_offset_alignment
        };
        *alignment = alignment
            .checked_sub(1)
            .expect("Vulkan minimum alignment is nonzero");
        let alignment_error = unsafe {
            pair.context.clone().open_compute_device(
                &*selected.adapter,
                wgt::Features::empty(),
                &insufficient,
                &hints,
            )
        };
        assert!(matches!(
            alignment_error,
            Err(SharedDeviceError::LimitsExceeded)
        ));
    }
    assert_eq!(group.compute_wrapper_attempts.load(Ordering::SeqCst), 0);
    pair.context.drain().unwrap();
    let closed = unsafe {
        pair.context.clone().open_compute_device(
            &*selected.adapter,
            wgt::Features::empty(),
            &selected.capabilities.limits,
            &hints,
        )
    };
    assert!(matches!(closed, Err(SharedDeviceError::Closed)));
    assert_eq!(group.compute_wrapper_attempts.load(Ordering::SeqCst), 0);
    drop((
        foreign,
        foreign_instance,
        selected,
        selected_adapters,
        selected_instance,
        missing,
    ));
    drop(pair);
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

fn endpoint_texture(open: &crate::DynOpenDevice) -> Box<dyn crate::DynTexture> {
    unsafe {
        open.device.create_texture(&crate::TextureDescriptor {
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
            usage: wgt::TextureUses::COPY_DST,
            memory_flags: crate::MemoryFlags::empty(),
            view_formats: Vec::new(),
        })
    }
    .unwrap()
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_compute_endpoints_keep_private_owners_on_one_native_device_and_queue() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let (_, instance) = pair.context.clone_instance();
    let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
    let open = || {
        unsafe {
            pair.context.clone().open_compute_device(
                &*adapter.adapter,
                wgt::Features::empty(),
                &adapter.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .unwrap()
    };
    let first = open();
    let second = open();
    assert_eq!(group.compute_wrapper_attempts.load(Ordering::SeqCst), 2);
    let original = pair
        .compute
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let first_native = first
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let second_native = second
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    assert_eq!(
        first_native.shared.raw.handle(),
        original.shared.raw.handle()
    );
    assert_eq!(
        second_native.shared.raw.handle(),
        original.shared.raw.handle()
    );
    assert_eq!(first_native.shared.raw_queue, original.shared.raw_queue);
    assert_eq!(second_native.shared.raw_queue, original.shared.raw_queue);
    assert!(!Arc::ptr_eq(&first_native.shared, &second_native.shared));

    let first_texture = endpoint_texture(&first);
    let second_texture = endpoint_texture(&second);
    let first_id = first_texture
        .as_any()
        .downcast_ref::<crate::vulkan::Texture>()
        .unwrap()
        .identity;
    let second_id = second_texture
        .as_any()
        .downcast_ref::<crate::vulkan::Texture>()
        .unwrap()
        .identity;
    assert_ne!(first_id, second_id);
    let mut first_encoder = unsafe {
        first
            .device
            .create_command_encoder(&crate::CommandEncoderDescriptor {
                label: None,
                queue: &*first.queue,
            })
    }
    .unwrap();
    let mut second_encoder = unsafe {
        second
            .device
            .create_command_encoder(&crate::CommandEncoderDescriptor {
                label: None,
                queue: &*second.queue,
            })
    }
    .unwrap();
    unsafe { first_encoder.begin_encoding(None) }.unwrap();
    let (second_data, _) = unsafe {
        second.device.create_buffer(&crate::BufferDescriptor {
            label: None,
            size: 4,
            usage: wgt::BufferUses::COPY_DST,
            memory_flags: crate::MemoryFlags::empty(),
        })
    }
    .unwrap();
    unsafe {
        second_encoder.begin_encoding(None).unwrap();
        second_encoder.transition_buffers(&[crate::BufferBarrier {
            buffer: &*second_data,
            usage: crate::StateTransition {
                from: wgt::BufferUses::empty(),
                to: wgt::BufferUses::COPY_DST,
            },
        }]);
        second_encoder.clear_buffer(&*second_data, 0..4);
    }
    let first_buffer = unsafe { first_encoder.end_encoding() }.unwrap();
    let second_buffer = unsafe { second_encoder.end_encoding() }.unwrap();
    let first_fence = unsafe { first.device.create_fence() }.unwrap();
    let second_fence = unsafe { second.device.create_fence() }.unwrap();
    let first_submitted = unsafe {
        first
            .queue
            .submit(&[&*first_buffer], &[], (&*first_fence, 1))
    };
    let first_idle = unsafe { first.queue.wait_for_idle() };
    if let Err(error) = first_idle {
        core::mem::forget((
            pair,
            first,
            second,
            first_encoder,
            second_encoder,
            first_buffer,
            second_buffer,
            first_fence,
            second_fence,
            first_texture,
            second_texture,
            second_data,
        ));
        panic!("First logical endpoint retirement unconfirmed: {error:?}");
    }
    unsafe {
        first_encoder.reset_all(vec![first_buffer]);
        first.device.destroy_fence(first_fence);
        first.device.destroy_texture(first_texture);
    }
    drop(first_encoder);
    drop(first);
    assert!(first_submitted.is_ok(), "{first_submitted:?}");
    let second_submitted = unsafe {
        second
            .queue
            .submit(&[&*second_buffer], &[], (&*second_fence, 1))
    };
    let retired = pair.context.drain();
    if let Err(error) = retired {
        core::mem::forget((
            pair,
            second,
            second_encoder,
            second_buffer,
            second_fence,
            second_texture,
            second_data,
        ));
        panic!("Second logical endpoint retirement unconfirmed: {error:?}");
    }
    unsafe {
        second_encoder.reset_all(vec![second_buffer]);
        second.device.destroy_fence(second_fence);
        second.device.destroy_texture(second_texture);
        second.device.destroy_buffer(second_data);
    }
    drop(second_encoder);
    drop(second);
    drop((adapter, instance, pair));
    assert!(second_submitted.is_ok(), "{second_submitted:?}");
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_compute_endpoint_waiting_for_host_lock_observes_drain_closure() {
    let pair = native_factory().unwrap();
    let group = native_group(&pair);
    let (_, instance) = pair.context.clone_instance();
    let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
    let host = group.host[1].lock();
    let opening_context = pair.context.clone();
    let opening = std::thread::spawn(move || unsafe {
        opening_context.open_compute_device(
            &*adapter.adapter,
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while group.compute_open_waiters.load(Ordering::SeqCst) != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "Open did not reach the held host lock"
        );
        std::thread::yield_now();
    }
    let draining_context = pair.context.clone();
    let draining = std::thread::spawn(move || draining_context.drain());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !group.closed.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "Drain did not close admission"
        );
        std::thread::yield_now();
    }
    assert_eq!(group.compute_wrapper_attempts.load(Ordering::SeqCst), 0);
    drop(host);
    let opened = opening.join().unwrap();
    let retired = draining.join().unwrap();
    if let Err(error) = retired {
        core::mem::forget((pair, opened));
        panic!("Concurrent drain retirement unconfirmed: {error:?}");
    }
    assert!(matches!(opened, Err(SharedDeviceError::Closed)));
    assert_eq!(group.compute_wrapper_attempts.load(Ordering::SeqCst), 0);
    drop(instance);
    drop(pair);
    assert_eq!(group.native_device_drops.load(Ordering::SeqCst), 1);
    assert_validation_clean();
}

#[path = "submit_tests.rs"]
mod ordered;

#[test]
fn shared_presentation_requires_version_extensions_and_feature() {
    assert!(presentation_support(vk::API_VERSION_1_1, true, true).is_ok());
    for (version, extensions, feature) in [
        (vk::API_VERSION_1_0, true, true),
        (vk::API_VERSION_1_1, false, true),
        (vk::API_VERSION_1_1, true, false),
    ] {
        assert!(matches!(
            presentation_support(version, extensions, feature),
            Err(SharedDeviceError::PresentationUnavailable)
        ));
    }
}

#[test]
#[ignore = "Requires Vulkan swapchain maintenance1 and Khronos validation"]
fn shared_presentation_capability_is_enabled_only_by_explicit_instance() {
    let offscreen = native_factory().unwrap();
    assert!(!native_group(&offscreen).presentation_fences_enabled);
    let offscreen_device = offscreen
        .graphics
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    assert!(!offscreen_device
        .shared
        .enabled_extensions
        .contains(&ash::ext::swapchain_maintenance1::NAME));
    drop(offscreen);

    let (backend, instance) = unsafe {
        crate::shared::create_presentation_instance(&crate::InstanceDescriptor {
            name: "HAL shared-presentation capability test",
            flags: wgt::InstanceFlags::VALIDATION | wgt::InstanceFlags::DEBUG,
            memory_budget_thresholds: wgt::MemoryBudgetThresholds {
                for_resource_creation: Some(95),
                for_device_loss: Some(99),
            },
            backend_options: Default::default(),
            telemetry: None,
            display: None,
        })
    }
    .unwrap();
    assert_eq!(backend, wgt::Backend::Vulkan);
    let native = instance
        .as_any()
        .downcast_ref::<crate::vulkan::Instance>()
        .unwrap();
    assert!(native
        .shared
        .debug_utils
        .as_ref()
        .is_some_and(|debug| { debug.callback_data.validation_layer_properties.is_some() }));
    for required in [
        ash::khr::surface::NAME,
        ash::khr::get_surface_capabilities2::NAME,
        ash::ext::surface_maintenance1::NAME,
    ] {
        assert!(native.shared.extensions.contains(&required));
    }
    let adapter = unsafe { instance.enumerate_adapters(None) }
        .into_iter()
        .next()
        .unwrap();
    let endpoints = unsafe {
        crate::shared::open_shared_device(
            &*adapter.adapter,
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    }
    .unwrap();
    let group = native_group(&endpoints);
    assert!(group.presentation_fences_enabled);
    for endpoint in [&endpoints.graphics, &endpoints.compute] {
        let device = endpoint
            .device
            .as_any()
            .downcast_ref::<crate::vulkan::Device>()
            .unwrap();
        assert!(device
            .shared
            .enabled_extensions
            .contains(&ash::ext::swapchain_maintenance1::NAME));
        assert!(device
            .shared
            .enabled_extensions
            .contains(&ash::khr::swapchain::NAME));
    }
    let (_, clone) = endpoints.context.clone_instance();
    drop((instance, adapter));
    let native = clone
        .as_any()
        .downcast_ref::<crate::vulkan::Instance>()
        .unwrap();
    assert!(native
        .shared
        .extensions
        .contains(&ash::ext::surface_maintenance1::NAME));
    endpoints.context.drain().unwrap();
    drop((clone, endpoints, group));
    assert_validation_clean();
}
