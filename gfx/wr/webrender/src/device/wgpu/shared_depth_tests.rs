/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::draw::DrawBatch;
use crate::device::wgpu::pipeline::DrawPipeline;
use crate::device::wgpu::render_pass::RenderPassState;
use crate::device::wgpu::shader::select_draw_shader;
use crate::device::wgpu::Buffer;
use crate::device::{DepthFunction, LoadOp, RenderPassDescriptor, RenderState, StoreOp};
use euclid::default::Transform3D;

fn target(store: &mut TextureStore, size: DeviceIntSize, depth: bool) -> TextureHandle {
    store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            size,
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: depth }),
        )
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn depth_targets_share_storage_by_dimensions_and_release_last_user() {
    let device = device();
    let mut store = TextureStore::new(&device);
    let size = DeviceIntSize::new(2, 3);
    let mut first = target(&mut store, size, true);
    let mut second = target(&mut store, size, true);
    let mut third = target(&mut store, size, false);
    let mut other = target(&mut store, DeviceIntSize::new(3, 2), true);
    store
        .reuse_render_target(&mut third, RenderTargetInfo { has_depth: true })
        .unwrap();
    let (first_color, first_depth) = store.render_target(first.target_id, true).unwrap();
    let (second_color, second_depth) = store.render_target(second.target_id, true).unwrap();
    let first_depth = first_depth.unwrap();
    let second_depth = second_depth.unwrap();
    assert!(!Rc::ptr_eq(&first_color, &second_color));
    assert!(Rc::ptr_eq(&first_depth, &second_depth));
    assert!(Rc::ptr_eq(
        &first_depth,
        &store
            .render_target(third.target_id, true)
            .unwrap()
            .1
            .unwrap()
    ));
    assert!(!Rc::ptr_eq(
        &first_depth,
        &store
            .render_target(other.target_id, true)
            .unwrap()
            .1
            .unwrap()
    ));
    assert_eq!(store.depth_bytes(), 48);
    let weak = Rc::downgrade(&first_depth);
    drop(first_depth);
    drop(second_depth);
    store.delete(&mut first).unwrap();
    store.delete(&mut second).unwrap();
    assert_eq!(store.depth_bytes(), 48);
    assert!(weak.upgrade().is_some());
    store
        .reuse_render_target(&mut third, RenderTargetInfo { has_depth: false })
        .unwrap();
    assert_eq!(store.depth_bytes(), 24);
    assert!(weak.upgrade().is_none());
    store
        .reuse_render_target(&mut third, RenderTargetInfo { has_depth: false })
        .unwrap();
    store.delete(&mut third).unwrap();
    store.delete(&mut other).unwrap();
    assert_eq!(store.depth_bytes(), 0);
    assert!(store.depth_targets.is_empty());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_depth_preserves_clears_and_submission_lifetimes() {
    let device = device();
    let mut store = TextureStore::new(&device);
    let size = DeviceIntSize::new(2, 2);
    let rect = DeviceIntRect::from_size(size);
    let left = DeviceIntRect::from_size(DeviceIntSize::new(1, 2));
    let right =
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(1, 0), DeviceIntSize::new(1, 2));
    let mut first = target(&mut store, size, true);
    let mut second = target(&mut store, size, true);
    let first_image = store.image(&first).unwrap();
    let second_image = store.image(&second).unwrap();
    let depth = store
        .render_target(first.target_id, true)
        .unwrap()
        .1
        .unwrap();
    let weak = Rc::downgrade(&depth);
    assert!(Rc::ptr_eq(
        &depth,
        &store
            .render_target(second.target_id, true)
            .unwrap()
            .1
            .unwrap()
    ));
    drop(depth);
    let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&device)), 4).unwrap();
    let quad = Buffer::new(
        &device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let pipeline = DrawPipeline::new(
        &device,
        select_draw_shader("ps_clear", &[], false).unwrap(),
        first_image.format(),
        true,
        RenderState {
            depth_test: Some(DepthFunction::Less),
            depth_write: true,
            ..RenderState::default()
        },
    )
    .unwrap();
    let mut state = RenderPassState::new(&quad);
    for (handle, area, clear_depth, clear_color, draw_color, depth_store) in [
        (
            &first,
            rect,
            0.0,
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            StoreOp::Store,
        ),
        (
            &second,
            rect,
            1.0,
            [0.0, 0.0, 1.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            StoreOp::Store,
        ),
        (
            &first,
            left,
            1.0,
            [0.0, 0.0, 1.0, 1.0],
            [1.0, 1.0, 0.0, 1.0],
            StoreOp::Discard,
        ),
        (
            &second,
            right,
            0.0,
            [0.0, 0.0, 1.0, 1.0],
            [1.0, 0.0, 1.0, 1.0],
            StoreOp::Store,
        ),
    ] {
        let full = area == rect;
        let mut commands = queue.recording().unwrap();
        state
            .begin(
                &mut commands,
                &mut store,
                &RenderPassDescriptor {
                    target: DrawTarget::from_texture(handle, true),
                    render_area: Some(area),
                    color_load: if full {
                        LoadOp::Clear(clear_color)
                    } else {
                        LoadOp::Load
                    },
                    depth_load: if full {
                        LoadOp::Clear(clear_depth)
                    } else {
                        LoadOp::DontCare
                    },
                },
                None,
            )
            .unwrap();
        if !full {
            state
                .clear(
                    &mut commands,
                    Some(clear_color),
                    Some(clear_depth),
                    Some(area.cast_unit()),
                )
                .unwrap();
        }
        state.flush(&mut commands, StoreOp::Store).unwrap();
        let mut pass = state.draw_pass().unwrap().unwrap();
        pass.depth_range = 0.0..0.5;
        let projection = pass.projection(&Transform3D::identity());
        let instances: Vec<_> = IntoIterator::into_iter([-1.0f32, -1.0, 1.0, 1.0])
            .chain(draw_color)
            .flat_map(f32::to_ne_bytes)
            .collect();
        pass.record_batches(
            &mut commands,
            &queue,
            &quad,
            None,
            &[DrawBatch {
                pipeline: &pipeline,
                projection: Some(&projection),
                textures: &[],
                buffers: &[],
                instances: &instances,
                instance_count: 1,
                scissor: area,
            }],
        )
        .unwrap();
        state.end(&mut commands, depth_store).unwrap();
        drop(commands);
        queue.submit().unwrap();
    }
    store.delete(&mut first).unwrap();
    store.delete(&mut second).unwrap();
    assert_eq!(store.depth_bytes(), 0);
    assert!(weak.upgrade().is_some());
    queue.wait().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(
        first_image.readback(rect).unwrap().wait().unwrap(),
        [[255, 255, 0, 255], [255, 0, 0, 255]].concat().repeat(2)
    );
    assert_eq!(
        second_image.readback(rect).unwrap().wait().unwrap(),
        [[0, 255, 0, 255], [0, 0, 255, 255]].concat().repeat(2)
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
