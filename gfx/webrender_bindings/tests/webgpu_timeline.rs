/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::khr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use wgpu_bindings::vulkan_timeline::{
    wgpu_vulkan_timeline_delete, wgpu_vulkan_timeline_export, VulkanTimeline, VulkanTimelineDescriptor,
};

fn device(
    adapter: &Arc<wgc::instance::Adapter>,
    external: bool,
) -> (Arc<wgc::device::Device>, Arc<wgc::device::queue::Queue>) {
    let mut desc = wgc::device::DeviceDescriptor::default();
    adapter.validate_device_descriptor(&mut desc).unwrap();
    let hal = unsafe { adapter.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
    let open = unsafe {
        hal.open_with_callback(
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
            Some(Box::new(move |args| {
                if external {
                    args.extensions.push(khr::external_semaphore_fd::NAME);
                }
            })),
        )
        .unwrap()
    };
    unsafe { adapter.create_device_and_queue_from_hal(open.into(), &desc) }.unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_webgpu_timeline_lifecycle() {
    let instance = wgc::instance::Instance::new(
        "WebGPU timeline test",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::VALIDATION,
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    );
    let adapter = instance
        .request_adapter(&Default::default(), wgt::Backends::VULKAN)
        .unwrap();
    let (unsupported, unsupported_queue) = device(&adapter, false);
    assert!(VulkanTimeline::new(unsupported.clone()).is_none());
    drop(unsupported_queue);
    drop(unsupported);

    let (producer, producer_queue) = device(&adapter, true);
    let (consumer, consumer_queue) = device(&adapter, true);
    let producer_weak = Arc::downgrade(&producer);
    let consumer_weak = Arc::downgrade(&consumer);
    let timeline = VulkanTimeline::new(producer.clone()).unwrap();
    let descriptor = timeline.export().unwrap();
    let fd = unsafe { OwnedFd::from_raw_fd(descriptor.fd) };
    let imported = unsafe { VulkanTimeline::import(consumer.clone(), &descriptor) }.unwrap();
    assert!(fd.try_clone().is_ok());
    assert!(imported.export().is_none());
    for field in 0..3 {
        let mut invalid = VulkanTimelineDescriptor {
            fd: fd.as_raw_fd(),
            device_uuid: descriptor.device_uuid,
            driver_uuid: descriptor.driver_uuid,
        };
        match field {
            0 => invalid.fd = -1,
            1 => invalid.device_uuid[0] ^= 1,
            _ => invalid.driver_uuid[15] ^= 1,
        }
        assert!(unsafe { VulkanTimeline::import(consumer.clone(), &invalid) }.is_none());
    }
    let mut unchanged = VulkanTimelineDescriptor {
        fd: -7,
        device_uuid: [3; 16],
        driver_uuid: [4; 16],
    };
    assert!(!wgpu_vulkan_timeline_export(None, &mut unchanged));
    assert!(!wgpu_vulkan_timeline_export(Some(&imported), &mut unchanged));
    assert_eq!(unchanged.fd, -7);
    assert_eq!(unchanged.device_uuid, [3; 16]);
    assert_eq!(unchanged.driver_uuid, [4; 16]);
    drop(fd);
    drop(producer_queue);
    drop(consumer_queue);
    drop(producer);
    drop(consumer);
    assert!(producer_weak.upgrade().is_some());
    assert!(consumer_weak.upgrade().is_some());
    assert!(wgpu_vulkan_timeline_export(Some(&timeline), &mut unchanged));
    let _fd = unsafe { OwnedFd::from_raw_fd(unchanged.fd) };
    producer_weak.upgrade().unwrap().destroy();
    consumer_weak.upgrade().unwrap().destroy();
    assert!(VulkanTimeline::new(producer_weak.upgrade().unwrap()).is_none());
    assert!(unsafe { VulkanTimeline::import(consumer_weak.upgrade().unwrap(), &unchanged) }.is_none());
    let previous_fd = unchanged.fd;
    assert!(!wgpu_vulkan_timeline_export(Some(&timeline), &mut unchanged));
    assert_eq!(unchanged.fd, previous_fd);
    unsafe {
        wgpu_vulkan_timeline_delete(Box::into_raw(Box::new(timeline)));
        wgpu_vulkan_timeline_delete(Box::into_raw(Box::new(imported)));
        wgpu_vulkan_timeline_delete(std::ptr::null_mut());
    }
    assert!(producer_weak.upgrade().is_none());
    assert!(consumer_weak.upgrade().is_none());
}
