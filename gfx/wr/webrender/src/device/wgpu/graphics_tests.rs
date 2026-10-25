/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, GraphicsRecorderFactory};
use crate::device::wgpu::wgt;
use hal::{Adapter as _, GraphicsRecorderFactory as _, Instance as _};

fn noop_device() -> (hal::noop::Context, hal::DynOpenDevice) {
    let instance = unsafe {
        hal::noop::Context::init(&hal::InstanceDescriptor {
            name: "WR graphics recorder tests",
            flags: wgt::InstanceFlags::empty(),
            memory_budget_thresholds: Default::default(),
            backend_options: wgt::BackendOptions {
                noop: wgt::NoopBackendOptions {
                    enable: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            telemetry: None,
            display: None,
        })
    }
    .unwrap();
    let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
    let open = unsafe {
        adapter.adapter.open(
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    }
    .unwrap();
    (instance, open.into())
}

fn empty_pass() -> hal::RenderPassDescriptor<'static, dyn hal::DynQuerySet, dyn hal::DynTextureView>
{
    hal::RenderPassDescriptor {
        label: Some("WR recorder lifecycle"),
        extent: wgt::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        sample_count: 1,
        color_attachments: &[],
        depth_stencil_attachment: None,
        multiview_mask: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    }
}

unsafe fn record_segment(recorder: &mut dyn hal::GraphicsRecorder) -> usize {
    unsafe {
        recorder
            .begin_encoding(Some("WR graphics segment"))
            .unwrap();
        recorder.begin_render_pass(&empty_pass()).unwrap();
        recorder.record(hal::GraphicsCommand::SetViewport {
            rect: hal::Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            depth_range: 0.0..1.0,
        });
        recorder.record(hal::GraphicsCommand::SetScissorRect {
            rect: hal::Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
        });
        recorder.end_render_pass();
        recorder.end_encoding().unwrap()
    }
}

#[test]
fn graphics_recorder_keeps_segments_in_one_execution() {
    let (_instance, open) = noop_device();
    let mut recorder =
        unsafe { GraphicsRecorderFactory.create(open.device.as_ref(), open.queue.as_ref()) }
            .unwrap();
    assert_eq!(unsafe { record_segment(recorder.as_mut()) }, 0);
    assert_eq!(unsafe { record_segment(recorder.as_mut()) }, 1);
    let execution = recorder.finish().unwrap();
    assert_eq!(execution.command_buffers().len(), 2);
}

#[test]
fn graphics_recorder_rejects_unclosed_encoding() {
    let (_instance, open) = noop_device();
    let mut recorder =
        unsafe { GraphicsRecorderFactory.create(open.device.as_ref(), open.queue.as_ref()) }
            .unwrap();
    unsafe { recorder.begin_encoding(None) }.unwrap();
    assert!(matches!(
        recorder.finish(),
        Err(hal::DeviceError::Unexpected)
    ));
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn graphics_recorder_retains_native_pool_until_gpu_completion() {
    use crate::device::wgpu::{
        Device, Options,
        resources::Owned,
        tests::{validation_logging, ERRORS},
    };
    use std::{rc::Rc, sync::atomic::Ordering};

    validation_logging();
    {
        let owner = Rc::new(
            Device::new(&Options {
                validation: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let device = owner.open.device.as_ref();
        let queue = owner.open.queue.as_ref();
        let mut recorder = unsafe { GraphicsRecorderFactory.create(device, queue) }.unwrap();
        assert_eq!(unsafe { record_segment(recorder.as_mut()) }, 0);
        assert_eq!(unsafe { record_segment(recorder.as_mut()) }, 1);
        let execution = recorder.finish().unwrap();
        let fence = Owned::new(
            &owner,
            unsafe { device.create_fence() }.unwrap(),
            <dyn hal::DynDevice>::destroy_fence,
        );
        let buffers: Vec<_> = execution
            .command_buffers()
            .iter()
            .map(|buffer| buffer.as_ref())
            .collect();
        let completed = unsafe { queue.submit(&buffers, &[], (&*fence, 1)) }
            .and_then(|()| unsafe { device.wait(&*fence, 1, None) });
        drop(buffers);
        if !matches!(completed, Ok(true)) {
            // Pending native work must retain its pool, fence and device until process exit.
            std::mem::forget((execution, fence, owner));
            panic!("Graphics work retirement unconfirmed: {completed:?}");
        }
        drop(execution);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn graphics_recorder_discards_unfinished_native_pass() {
    use crate::device::wgpu::{
        Device, Options,
        tests::{validation_logging, ERRORS},
    };
    use std::sync::atomic::Ordering;

    validation_logging();
    {
        let owner = Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let mut recorder = unsafe {
            GraphicsRecorderFactory.create(owner.open.device.as_ref(), owner.open.queue.as_ref())
        }
        .unwrap();
        unsafe {
            recorder.begin_encoding(None).unwrap();
            recorder.begin_render_pass(&empty_pass()).unwrap();
        }
        drop(recorder);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
