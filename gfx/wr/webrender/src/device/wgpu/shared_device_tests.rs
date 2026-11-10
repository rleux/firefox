/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
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
fn shared_hal_instance_keeps_webgpu_core_flags() {
    let owner = tests::device();
    let raw = unsafe { owner.instance.as_hal::<hal::api::Vulkan>() }
        .unwrap()
        .clone();
    let mut builder = core::instance::InstanceBuilder::new(
        "WebGPU policies",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
                | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION,
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    );
    builder.try_add_custom_hal(hal::api::Vulkan, |_| Ok(raw));
    let instance = builder.build();
    let adapter = instance
        .enumerate_adapters(wgt::Backends::VULKAN, true)
        .into_iter()
        .find(|adapter| adapter.get_info().device == owner.info().device)
        .unwrap();
    let descriptor = core::device::DeviceDescriptor {
        required_features: wgt::Features::PASSTHROUGH_SHADERS,
        ..Default::default()
    };
    let (device, _) =
        wgpu_shared::open_device(&adapter, &owner.device, &owner.queue, &descriptor).unwrap();
    assert!(!device
        .features()
        .contains(wgt::Features::PASSTHROUGH_SHADERS));
    device.destroy();
    owner.check().unwrap();
}

#[test]
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
