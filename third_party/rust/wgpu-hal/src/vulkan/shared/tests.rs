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

fn native_factory() -> Result<SharedDeviceEndpoints, SharedDeviceError> {
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
