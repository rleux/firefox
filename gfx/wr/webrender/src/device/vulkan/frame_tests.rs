/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{Fence, FenceStatus, GpuFrameId};

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn frames_reset_bindings_and_counters_without_reallocating_output() {
    let mut device = device();
    assert_eq!(device.begin_frame().unwrap(), GpuFrameId::new(1));
    let mut texture = device
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(1, 1),
            TextureFilter::Nearest,
            None,
        )
        .unwrap();
    assert_eq!(texture.last_frame_used(), GpuFrameId::new(1));
    let mut program = device.programs.create("ps_clear", &[], false).unwrap();
    device.programs.link(&mut program, &desc::CLEAR).unwrap();
    let mut vertices = device
        .vertex_arrays
        .create_buffer(crate::device::BufferKind::Vertex)
        .unwrap();
    let mut instances = device
        .vertex_arrays
        .create_buffer(crate::device::BufferKind::Vertex)
        .unwrap();
    let mut vao = device
        .vertex_arrays
        .create(&desc::CLEAR, &vertices, Some(&instances), None, 1)
        .unwrap();
    device
        .vertex_arrays
        .write_buffer(
            &mut instances,
            &floats(&[-1.0, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0]),
        )
        .unwrap();
    device.vertex_arrays.bind(&vao).unwrap();
    device.begin_render_pass(&descriptor()).unwrap();
    device
        .bind_pipeline(&program, RenderState::default())
        .unwrap();
    device.textures.bind(TextureSlot(5), &texture).unwrap();
    device.draw_instanced(0, 1).unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    let first = device.end_frame().unwrap();
    assert_eq!(device.textures.created(), 1);
    let output = device.textures.output().unwrap();
    assert_eq!(device.begin_frame().unwrap(), GpuFrameId::new(2));
    assert_eq!(
        (device.textures.created(), device.textures.deleted()),
        (0, 0)
    );
    assert!(device.textures.bindings().iter().all(Option::is_none));
    assert!(device.programs.current().is_err());
    assert!(device.vertex_arrays.instances(0, 1).is_err());
    assert!(Rc::ptr_eq(&output, &device.textures.output().unwrap()));
    assert_eq!(device.end_frame().unwrap(), first);
    device.submissions.wait_for(first).unwrap();
    assert_eq!(pixels(&output), [0, 255, 0, 255].repeat(2));
    device.textures.delete(&mut texture).unwrap();
    device.programs.delete(&mut program).unwrap();
    device.vertex_arrays.delete(&mut vao).unwrap();
    device.vertex_arrays.delete_buffer(&mut vertices).unwrap();
    device.vertex_arrays.delete_buffer(&mut instances).unwrap();
    device.frame = GpuFrameId::new(usize::MAX);
    assert!(device.begin_frame().is_err());
    assert!(!device.inside_frame);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn frame_boundaries_reject_nesting_and_unfinished_passes() {
    let mut invalid = device();
    invalid.begin_frame().unwrap();
    assert!(crate::device::GpuBackend::set_surface_paused(&mut invalid, true).is_err());
    assert!(crate::device::GpuBackend::set_surface_paused(&mut invalid, false).is_err());
    drop(invalid);
    let mut device = device();
    assert!(device.end_frame().is_err());
    assert!(device.begin_render_pass(&descriptor()).is_err());
    assert!(device.draw_instanced(0, 0).is_err());
    assert_eq!(device.begin_frame().unwrap(), GpuFrameId::new(1));
    assert!(device.begin_frame().is_err());
    device.begin_render_pass(&descriptor()).unwrap();
    assert!(device.end_frame().is_err());
    device
        .clear_target(Some([1.0, 0.0, 0.0, 1.0]), None, None)
        .unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    let serial = device.end_frame().unwrap();
    assert!(device.end_frame().is_err());
    device.submissions.wait_for(serial).unwrap();
    let fence = device.submissions.create_fence().unwrap();
    assert_eq!(device.submissions.poll_fence(&fence), FenceStatus::Signaled);
    assert_eq!(
        device.submissions.poll_fence(&Fence(usize::MAX)),
        FenceStatus::Error
    );
    assert_eq!(
        pixels(&device.textures.output().unwrap()),
        [255, 0, 0, 255].repeat(2)
    );
    device.fallback.raw.owner.lost.set(true);
    assert_eq!(device.submissions.poll_fence(&fence), FenceStatus::Error);
    assert!(device.submissions.create_fence().is_err());
    assert!(crate::device::GpuBackend::gpu_submission_status(&mut device).is_err());
    assert!(device.failure().is_some());
    assert!(crate::device::GpuBackend::set_surface_paused(&mut device, true).is_err());
    device.fallback.raw.owner.lost.set(false);
    assert!(crate::device::GpuBackend::set_surface_paused(&mut device, false).is_err());
    assert!(crate::device::GpuBackend::gpu_submission_status(&mut device).is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
