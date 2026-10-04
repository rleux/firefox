/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::khr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;
use wgpu_bindings::vulkan_timeline::{
    submit_with_timelines, wgpu_vulkan_timeline_delete, wgpu_vulkan_timeline_export, VulkanTimeline,
    VulkanTimelineDescriptor, VulkanTimelinePoint,
};

fn device(
    adapter: &Arc<wgc::instance::Adapter>,
    external: bool,
) -> (Arc<wgc::device::Device>, Arc<wgc::device::queue::Queue>) {
    let extensions = if external {
        vec![khr::external_semaphore_fd::NAME]
    } else {
        Vec::new()
    };
    device_with_extensions(adapter, &extensions)
}

pub(super) fn device_with_extensions(
    adapter: &Arc<wgc::instance::Adapter>,
    extensions: &[&'static std::ffi::CStr],
) -> (Arc<wgc::device::Device>, Arc<wgc::device::queue::Queue>) {
    let mut desc = wgc::device::DeviceDescriptor::default();
    adapter.validate_device_descriptor(&mut desc).unwrap();
    let hal = unsafe { adapter.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
    let open = unsafe {
        hal.open_with_callback(
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
            Some(Box::new(|args| args.extensions.extend_from_slice(extensions))),
        )
        .unwrap()
    };
    unsafe { adapter.create_device_and_queue_from_hal(open.into(), &desc) }.unwrap()
}

pub(super) fn adapter() -> Arc<wgc::instance::Adapter> {
    let instance = wgc::instance::Instance::new(
        "WebGPU timeline test",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::VALIDATION,
            ..wgt::InstanceDescriptor::new_without_display_handle()
        },
        None,
    );
    instance
        .request_adapter(&Default::default(), wgt::Backends::VULKAN)
        .unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_webgpu_timeline_lifecycle() {
    let adapter = adapter();
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

fn point(timeline: &VulkanTimeline, value: u64) -> VulkanTimelinePoint<'_> {
    VulkanTimelinePoint {
        timeline: Some(timeline),
        value,
    }
}

fn import(device: &Arc<wgc::device::Device>, timeline: &VulkanTimeline) -> VulkanTimeline {
    let descriptor = timeline.export().unwrap();
    let _fd = unsafe { OwnedFd::from_raw_fd(descriptor.fd) };
    unsafe { VulkanTimeline::import(device.clone(), &descriptor) }.unwrap()
}

fn wait(device: &Arc<wgc::device::Device>, index: u64) {
    device
        .poll(wgt::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(Duration::from_secs(5)),
        })
        .unwrap();
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_timeline_submission() {
    let adapter = adapter();
    let (producer, producer_queue) = device(&adapter, true);
    let (consumer, consumer_queue) = device(&adapter, true);
    let ready = VulkanTimeline::new(producer.clone()).unwrap();
    let gate = VulkanTimeline::new(producer.clone()).unwrap();
    let returned = VulkanTimeline::new(consumer.clone()).unwrap();
    let ready_import = import(&consumer, &ready);
    let gate_import = import(&consumer, &gate);
    let return_import = import(&producer, &returned);
    let monitor = import(&consumer, &ready);

    assert!(submit_with_timelines(&producer_queue, &[], &[], &[point(&ready, 0)]).is_none());
    assert!(submit_with_timelines(
        &producer_queue,
        &[],
        &[],
        &[VulkanTimelinePoint {
            timeline: None,
            value: 1
        }]
    )
    .is_none());
    assert!(submit_with_timelines(&consumer_queue, &[], &[], &[point(&ready, 1)]).is_none());
    assert!(submit_with_timelines(&consumer_queue, &[], &[], &[point(&ready_import, 1)]).is_none());
    assert!(submit_with_timelines(&producer_queue, &[], &[point(&ready, 1)], &[]).is_none());
    let first = submit_with_timelines(&producer_queue, &[], &[], &[point(&ready, 1), point(&ready, 2)]).unwrap();
    wait(&producer, first);
    assert_eq!(ready.current_value(), Some(2));
    assert!(submit_with_timelines(&producer_queue, &[], &[], &[point(&ready, 2)]).is_none());
    assert!(submit_with_timelines(&producer_queue, &[], &[point(&ready, 3)], &[]).is_none());

    let consumer_index = submit_with_timelines(
        &consumer_queue,
        &[],
        &[point(&ready_import, 1), point(&ready_import, 2), point(&gate_import, 1)],
        &[point(&returned, 1)],
    )
    .unwrap();
    drop(ready_import);
    drop(gate_import);
    wgpu_vulkan_timeline_delete(Box::into_raw(Box::new(returned)));
    submit_with_timelines(&producer_queue, &[], &[], &[point(&gate, 1)]).unwrap();
    let last = submit_with_timelines(
        &producer_queue,
        &[],
        &[point(&return_import, 1), point(&ready, 2)],
        &[point(&ready, 3)],
    )
    .unwrap();
    drop(return_import);
    wgpu_vulkan_timeline_delete(Box::into_raw(Box::new(ready)));
    drop(gate);
    wait(&producer, last);
    wait(&consumer, consumer_index);
    assert_eq!(monitor.current_value(), Some(3));
    drop(monitor);
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_timeline_failed_submission() {
    let adapter = adapter();
    let (producer, producer_queue) = device(&adapter, true);
    let (consumer, consumer_queue) = device(&adapter, true);
    let gate = VulkanTimeline::new(producer.clone()).unwrap();
    let gate_import = import(&consumer, &gate);
    let signal = VulkanTimeline::new(consumer.clone()).unwrap();
    let commands = consumer
        .create_command_encoder(&Default::default())
        .finish(&Default::default());
    consumer_queue.submit(&[commands.clone()]);
    assert!(submit_with_timelines(
        &consumer_queue,
        &[commands],
        &[point(&gate_import, 1)],
        &[point(&signal, 1)]
    )
    .is_none());
    assert!(consumer.is_valid());
    assert!(submit_with_timelines(&consumer_queue, &[], &[point(&signal, 1)], &[]).is_none());
    assert!(submit_with_timelines(&consumer_queue, &[], &[], &[point(&signal, 1)]).is_none());

    let probe = consumer_queue.submit(&[]);
    let completed = consumer
        .poll(wgt::PollType::Wait {
            submission_index: Some(probe),
            timeout: Some(Duration::from_secs(1)),
        })
        .is_ok();
    let released = submit_with_timelines(&producer_queue, &[], &[], &[point(&gate, 1)]).unwrap();
    wait(&producer, released);
    wait(&consumer, probe);
    assert!(completed, "a failed submission left a wait on the queue");
    assert_eq!(
        signal.current_value(),
        Some(0),
        "a failed submission left a signal on the queue"
    );
    let next = submit_with_timelines(&consumer_queue, &[], &[], &[point(&signal, 2)]).unwrap();
    wait(&consumer, next);
    assert_eq!(signal.current_value(), Some(2));
    consumer.destroy();
    assert!(submit_with_timelines(&consumer_queue, &[], &[], &[point(&signal, 3)]).is_none());
}
