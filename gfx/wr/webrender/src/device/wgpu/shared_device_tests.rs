/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_native_device_keeps_core_errors_and_destruction_isolated() {
    let owner = tests::device();
    let descriptor = core::device::DeviceDescriptor {
        label: Some("isolated WebGPU".into()),
        required_limits: owner.adapter.limits(),
        ..Default::default()
    };
    let (first, first_queue) =
        wgpu_shared::open_device(&owner.adapter, &owner.device, &owner.queue, &descriptor).unwrap();
    let (second, second_queue) =
        wgpu_shared::open_device(&owner.adapter, &owner.device, &owner.queue, &descriptor).unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(*first.features(), wgt::Features::empty());
    assert!(!first
        .features()
        .contains(wgt::Features::PASSTHROUGH_SHADERS));
    let first_errors = Arc::new(AtomicUsize::new(0));
    let errors = first_errors.clone();
    first.on_uncaptured_error(Arc::new(move |_| {
        errors.fetch_add(1, Ordering::Relaxed);
    }));
    let _invalid = first.create_buffer(&core::resource::BufferDescriptor {
        label: None,
        size: 4,
        usage: wgt::BufferUsages::empty(),
        mapped_at_creation: false,
    });
    assert_eq!(first_errors.load(Ordering::Relaxed), 1);
    owner.check().unwrap();
    second.check_is_valid().unwrap();
    unsafe {
        let a = first.clone().as_hal::<hal::api::Vulkan>().unwrap();
        let b = second.clone().as_hal::<hal::api::Vulkan>().unwrap();
        assert_eq!(a.raw_device().handle(), b.raw_device().handle());
        assert_eq!(a.raw_queue(), b.raw_queue());
    }
    first.destroy();
    assert!(!first.is_valid());
    owner.check().unwrap();
    second.check_is_valid().unwrap();
    let encoder = second.create_command_encoder(&Default::default());
    let command = encoder.finish(&Default::default());
    assert!(second_queue.submit(&[command]).is_some());
    second
        .poll(wgt::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_secs(10)),
        })
        .unwrap();
    drop(first_queue);
    second.destroy();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_native_device_rejects_unrelated_queue() {
    let owner = tests::device();
    let other = tests::device();
    assert!(wgpu_shared::open_device(
        &owner.adapter,
        &owner.device,
        &other.queue,
        &Default::default(),
    )
    .is_err());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_hal_instance_keeps_webgpu_core_flags() {
    let owner = tests::device();
    let context = wgpu_shared::NativeContext::new(
        owner.instance.clone(),
        owner.adapter.clone(),
        owner.device.clone(),
        owner.queue.clone(),
    );
    let descriptor = || wgt::InstanceDescriptor {
        backends: wgt::Backends::VULKAN,
        flags: wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
            | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION,
        ..wgt::InstanceDescriptor::new_without_display_handle()
    };
    let independent =
        core::instance::Instance::new("Independent WebGPU policy", descriptor(), None);
    let shared = context.instance_with_policy(descriptor(), None);
    let mut selected = None;
    for bucketed in [false, true] {
        let baseline = independent
            .enumerate_adapters(wgt::Backends::VULKAN, bucketed)
            .into_iter()
            .find(|adapter| adapter.get_info().device == owner.info().device);
        let admitted = shared
            .enumerate_adapters(wgt::Backends::VULKAN, bucketed)
            .into_iter()
            .find(|adapter| context.matches_adapter(adapter));
        assert_eq!(
            admitted.is_some(),
            baseline.is_some(),
            "bucketed={}",
            bucketed
        );
        if let (Some(admitted), Some(baseline)) = (admitted, baseline) {
            assert_eq!(admitted.features(), baseline.features());
            assert_eq!(admitted.limits(), baseline.limits());
            assert!(!admitted
                .features()
                .contains(wgt::Features::PASSTHROUGH_SHADERS));
            if !bucketed {
                selected = Some(admitted);
            }
        }
    }
    let adapter =
        selected.expect("The independent and shared unbucketed adapter must be available");
    let (device, _) = context
        .open(
            &adapter,
            &core::device::DeviceDescriptor {
                required_features: wgt::Features::PASSTHROUGH_SHADERS,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!device
        .features()
        .contains(wgt::Features::PASSTHROUGH_SHADERS));
    device.destroy();
    owner.check().unwrap();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_context_stops_admitting_devices_after_loss() {
    let owner = tests::device();
    let context = wgpu_shared::NativeContext::new(
        owner.instance.clone(),
        owner.adapter.clone(),
        owner.device.clone(),
        owner.queue.clone(),
    );
    assert!(context.is_valid());
    context.mark_failed();
    assert!(!context.is_valid());
    assert!(context.open(&owner.adapter, &Default::default()).is_err());
    owner.check().unwrap();
}

fn round_trip(
    device: &Arc<core::device::Device>,
    queue: &Arc<core::device::queue::Queue>,
    seed: u8,
) {
    let source = device.create_buffer(&core::resource::BufferDescriptor {
        label: Some("shared queue source".into()),
        size: 64,
        usage: wgt::BufferUsages::COPY_SRC | wgt::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let target = device.create_buffer(&core::resource::BufferDescriptor {
        label: Some("shared queue readback".into()),
        size: 64,
        usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    for offset in 0..8 {
        queue.write_buffer(source.clone(), 0, &[seed + offset; 64]);
        let encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(source.clone(), 0, target.clone(), 0, Some(64));
        assert!(queue
            .submit(&[encoder.finish(&Default::default())])
            .is_some());
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    target.map_async(
        wgt::MapMode::Read,
        0,
        Some(64),
        Some(Box::new(move |result| {
            sender.send(result).unwrap();
        })),
    );
    device
        .poll(wgt::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_secs(10)),
        })
        .unwrap();
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let bytes = target
        .get_mapped_range(0, Some(64))
        .unwrap()
        .read_slice()
        .to_vec();
    target.unmap();
    assert_eq!(bytes, vec![seed + 7; 64]);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_native_owner_outlives_source_core_device() {
    let owner = tests::device();
    let errors = tests::VALIDATION_ERRORS.load(Ordering::Relaxed);
    let (device, queue) = wgpu_shared::open_device(
        &owner.adapter,
        &owner.device,
        &owner.queue,
        &core::device::DeviceDescriptor {
            required_limits: owner.adapter.limits(),
            ..Default::default()
        },
    )
    .unwrap();
    let source = Arc::downgrade(&owner.device);
    drop(owner);
    assert!(source.upgrade().is_none());
    round_trip(&device, &queue, 11);
    assert_eq!(tests::VALIDATION_ERRORS.load(Ordering::Relaxed), errors);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_native_queues_submit_concurrently_without_cross_client_corruption() {
    let owner = tests::device();
    let errors = tests::VALIDATION_ERRORS.load(Ordering::Relaxed);
    let descriptor = core::device::DeviceDescriptor {
        required_limits: owner.adapter.limits(),
        ..Default::default()
    };
    let (first, first_queue) =
        wgpu_shared::open_device(&owner.adapter, &owner.device, &owner.queue, &descriptor).unwrap();
    let (second, second_queue) =
        wgpu_shared::open_device(&owner.adapter, &owner.device, &owner.queue, &descriptor).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| round_trip(&first, &first_queue, 21));
        scope.spawn(|| round_trip(&second, &second_queue, 41));
        round_trip(&owner.device, &owner.queue, 61);
    });
    assert_eq!(tests::VALIDATION_ERRORS.load(Ordering::Relaxed), errors);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn closing_a_renderer_preserves_the_remaining_shared_context() {
    tests::validation_logging();
    let errors = tests::VALIDATION_ERRORS.load(Ordering::Relaxed);
    let options = Options {
        validation: true,
        shared_webgpu: true,
        ..Default::default()
    };
    let first = Device::new(&options).unwrap();
    let context = first
        .shared
        .as_ref()
        .expect("Sharing must activate on this adapter")
        .clone();
    let second = Device::new(&options).unwrap();
    assert!(Arc::ptr_eq(&context, second.shared.as_ref().unwrap()));
    drop(first);
    assert!(context.is_valid());
    second.check().unwrap();
    round_trip(&second.device, &second.queue, 81);
    drop(second);
    assert!(context.is_valid());
    drop(context);
    assert!(wgpu_shared::NativeContext::active().is_none());
    assert_eq!(tests::VALIDATION_ERRORS.load(Ordering::Relaxed), errors);
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and Khronos validation"]
fn shared_canvas_poll_waits_for_native_progress_then_drains_owner_callbacks() {
    use ash::vk;
    let owner = Arc::new(tests::device());
    let errors = tests::VALIDATION_ERRORS.load(Ordering::Relaxed);
    let context = Arc::new(wgpu_shared::NativeContext::new(
        owner.instance.clone(),
        owner.adapter.clone(),
        owner.device.clone(),
        owner.queue.clone(),
    ));
    let (producer, queue) = context
        .open(
            &owner.adapter,
            &core::device::DeviceDescriptor {
                required_limits: owner.adapter.limits(),
                ..Default::default()
            },
        )
        .unwrap();
    let texture = producer
        .create_texture_with_internal_sampling(&core::resource::TextureDescriptor {
            label: None,
            size: wgt::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: wgt::TextureFormat::Rgba8Unorm,
            usage: wgt::TextureUsages::RENDER_ATTACHMENT,
            view_formats: vec![],
        })
        .unwrap();
    // Unblock even when an assertion unwinds, before destroying the semaphore.
    struct Release(Rc<SharedTimeline>);
    impl Drop for Release {
        fn drop(&mut self) {
            let hal = self.0.owner.raw_device();
            let info = vk::SemaphoreSignalInfo::default()
                .semaphore(self.0.semaphore)
                .value(1);
            unsafe {
                if hal
                    .enabled_device_extensions()
                    .contains(&ash::khr::timeline_semaphore::NAME)
                {
                    ash::khr::timeline_semaphore::Device::new(
                        hal.shared_instance().raw_instance(),
                        hal.raw_device(),
                    )
                    .signal_semaphore(&info)
                    .unwrap();
                } else {
                    hal.raw_device().signal_semaphore(&info).unwrap();
                }
            }
            self.0.owner.native_idle().unwrap();
        }
    }
    let release = Release(SharedTimeline::local(&owner).unwrap());
    unsafe {
        queue
            .clone()
            .as_hal::<hal::api::Vulkan>()
            .unwrap()
            .add_wait_semaphore(
                release.0.semaphore,
                Some(1),
                vk::PipelineStageFlags::TOP_OF_PIPE,
            );
    }
    let lease = unsafe { CanvasPublication::seal(&context, &texture, &queue, 91, 101) }.unwrap();
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        assert!(unsafe { lease.poll_producer_on_owner_thread() });
        assert!(!lease.is_ready());
    }
    eprintln!("1000 native-pending canvas polls: {:?}", start.elapsed());
    assert!(lease.wrap(&context.device).unwrap().is_none());
    drop(release);
    assert!(
        !lease.is_ready(),
        "Native completion alone must not deliver core callbacks"
    );
    assert!(!unsafe { lease.poll_producer_on_owner_thread() });
    assert!(lease.is_ready());
    let consumer = lease.wrap(&context.device).unwrap().unwrap();
    assert!(lease.matches_texture(&consumer));
    assert!(!unsafe { lease.poll_producer_on_owner_thread() });
    consumer.destroy();
    assert_eq!(tests::VALIDATION_ERRORS.load(Ordering::Relaxed), errors);
}
