/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::super::draw::DrawBatch;
use super::super::super::pipeline::DrawPipeline;
use super::super::super::shader::select_draw_shader;
use super::super::super::{wgt, Buffer};
use crate::device::RenderState;
use euclid::default::Transform3D;

fn default_descriptor(width: i32, height: i32) -> RenderPassDescriptor {
    RenderPassDescriptor {
        target: DrawTarget::new_default(DeviceIntSize::new(width, height), true),
        render_area: None,
        color_load: LoadOp::Load,
        depth_load: LoadOp::Load,
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn resized_default_targets_clip_stale_viewports_without_scaling() {
    let (mut textures, mut handle, queue) = setup(false);
    let owner = textures.image(&handle).unwrap().raw.owner.clone();
    let quad = Buffer::new(
        &owner,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let pipeline = DrawPipeline::new(
        &owner,
        select_draw_shader("ps_clear", &[], false).unwrap(),
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .unwrap();
    let colors = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
    ];
    let instances: Vec<Vec<u8>> = colors
        .iter()
        .enumerate()
        .map(|(index, color)| {
            let x = (index % 2 * 2) as f32;
            let y = (index / 2 * 2) as f32;
            [x, y, x + 2.0, y + 2.0]
                .iter()
                .copied()
                .chain(color.iter().map(|&channel| channel as f32 / 255.0))
                .flat_map(f32::to_ne_bytes)
                .collect()
        })
        .collect();
    let mut state = RenderPassState::default();
    for (width, height, x, y, vw, vh) in [
        (4, 4, 0, 0, 4, 4),
        (2, 3, 0, 0, 4, 4),
        (2, 3, 1, 1, 4, 4),
        (2, 3, 3, 3, 4, 4),
        (6, 5, 0, 0, 4, 4),
        (2, 3, 0, 0, 2, 3),
    ] {
        let mut desc = default_descriptor(width, height);
        let viewport = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(x, y),
            DeviceIntSize::new(vw, vh),
        );
        if let DrawTarget::Default { ref mut rect, .. } = desc.target {
            *rect = viewport.cast_unit();
        }
        desc.color_load = LoadOp::Clear([0.0, 0.0, 0.0, 1.0]);
        {
            let mut commands = queue.recording().unwrap();
            state
                .begin(&mut commands, &mut textures, &desc)
                .unwrap();
            state.set_scissor_rect(viewport.cast_unit());
            state.enable_scissor();
            let pass = state.draw_pass().unwrap();
            let projection = pass.projection(&Transform3D::ortho(
                0.0, vw as f32, 0.0, vh as f32, -1.0, 1.0,
            ));
            let batches: Vec<_> = instances
                .iter()
                .map(|instance| DrawBatch {
                    pipeline: &pipeline,
                    projection: Some(&projection),
                    textures: &[],
                    buffers: &[],
                    instances: instance,
                    instance_count: 1,
                    scissor: state.scissor_rect().unwrap(),
                })
                .collect();
            pass.record_batches(&mut commands, &queue, &quad, None, &batches)
                .unwrap();
            state.end(&mut commands, StoreOp::Store).unwrap();
        }
        queue.wait().unwrap();
        let actual = textures
            .output()
            .unwrap()
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(width, height)))
            .unwrap()
            .wait()
            .unwrap();
        for py in 0..height {
            for px in 0..width {
                let local_x = px - x;
                let local_y = py - y;
                let expected =
                    if (0..vw.min(4)).contains(&local_x) && (0..vh.min(4)).contains(&local_y) {
                        colors[(local_y / 2 * 2 + local_x / 2) as usize]
                    } else {
                        [0, 0, 0, 255]
                    };
                let offset = ((py * width + px) * 4) as usize;
                assert_eq!(
                    &actual[offset..offset + 4],
                    &expected,
                    "target {width}x{height}, viewport {viewport:?}, pixel ({px}, {py})"
                );
            }
        }
    }
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn default_output_reuses_storage_and_retains_previous_size() {
    let (mut textures, mut handle, queue) = setup(false);
    let mut state = RenderPassState::default();
    assert!(textures.output().is_none());
    let mut desc = default_descriptor(2, 2);
    desc.color_load = LoadOp::Clear([0.0, 0.0, 1.0, 1.0]);
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc).unwrap();
        assert!(state.draw_pass().unwrap().depth.is_none());
        state.end(&mut commands, StoreOp::Discard).unwrap();
    }
    let old = textures.output().unwrap();
    desc.color_load = LoadOp::Load;
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc).unwrap();
        assert!(Rc::ptr_eq(&old, state.draw_pass().unwrap().target));
        state
            .clear(
                &mut commands,
                Some([1.0, 0.0, 0.0, 1.0]),
                None,
                Some(left().cast_unit()),
            )
            .unwrap();
        state.end(&mut commands, StoreOp::Store).unwrap();
        let mut owned = descriptor(&handle, false);
        owned.color_load = LoadOp::Clear([0.0, 1.0, 0.0, 1.0]);
        state.begin(&mut commands, &mut textures, &owned).unwrap();
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    assert!(Rc::ptr_eq(&old, &textures.output().unwrap()));
    assert_eq!(
        (
            textures.created(),
            textures.deleted(),
            textures.depth_bytes()
        ),
        (1, 0, 0)
    );
    let mut resized = default_descriptor(3, 1);
    resized.color_load = LoadOp::Clear([1.0, 1.0, 0.0, 1.0]);
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &resized).unwrap();
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    let output = textures.output().unwrap();
    assert!(!Rc::ptr_eq(&old, &output));
    assert_eq!(output.format(), wgt::TextureFormat::Rgba8Unorm);
    assert_eq!(output.filter(), TextureFilter::Linear);
    textures.delete(&mut handle).unwrap();
    drop(textures);
    queue.wait().unwrap();
    assert_eq!(pixels(&old), [255, 0, 0, 255, 0, 0, 255, 255].repeat(2));
    assert_eq!(
        output
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(3, 1)))
            .unwrap()
            .wait()
            .unwrap(),
        [255, 255, 0, 255].repeat(3)
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn default_viewport_places_draws_without_discarding_other_pixels() {
    let (mut textures, mut handle, queue) = setup(false);
    let owner = textures.image(&handle).unwrap().raw.owner.clone();
    let quad = Buffer::new(
        &owner,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let pipeline = DrawPipeline::new(
        &owner,
        select_draw_shader("ps_clear", &[], false).unwrap(),
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .unwrap();
    let mut state = RenderPassState::default();
    let mut desc = default_descriptor(4, 3);
    let viewport =
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(1, 1), DeviceIntSize::new(2, 2));
    if let DrawTarget::Default { ref mut rect, .. } = desc.target {
        *rect = viewport.cast_unit();
    }
    desc.color_load = LoadOp::Clear([1.0, 0.0, 0.0, 1.0]);
    let draw = |state: &RenderPassState, commands: &mut Recording<'_>, color: [f32; 4]| {
        let pass = state.draw_pass().unwrap();
        assert_eq!(pass.viewport, Some(viewport));
        let projection = pass.projection(&Transform3D::identity());
        let data: Vec<_> = [-1.0f32, -1.0, 1.0, 1.0]
            .iter()
            .chain(&color)
            .flat_map(|f| f.to_ne_bytes())
            .collect();
        pass.record_batches(
            commands,
            &queue,
            &quad,
            None,
            &[DrawBatch {
                pipeline: &pipeline,
                projection: Some(&projection),
                textures: &[],
                buffers: &[],
                instances: &data,
                instance_count: 1,
                scissor: state.scissor_rect().unwrap(),
            }],
        )
        .unwrap();
    };
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc).unwrap();
        draw(&state, &mut commands, [0.0, 1.0, 0.0, 1.0]);
        state.end(&mut commands, StoreOp::Store).unwrap();
        desc.color_load = LoadOp::DontCare;
        desc.render_area = Some(DeviceIntRect::from_size(DeviceIntSize::new(4, 3)));
        state.begin(&mut commands, &mut textures, &desc).unwrap();
        assert!(state.draw_pass().unwrap().target.initialized());
        state.set_scissor_rect(
            DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(1, 1),
                DeviceIntSize::new(1, 2),
            )
            .cast_unit(),
        );
        state.enable_scissor();
        draw(&state, &mut commands, [0.0, 0.0, 1.0, 1.0]);
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    queue.wait().unwrap();
    let output = textures.output().unwrap();
    let actual = output
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(4, 3)))
        .unwrap()
        .wait()
        .unwrap();
    let r = [255, 0, 0, 255];
    let g = [0, 255, 0, 255];
    let b = [0, 0, 255, 255];
    assert_eq!(actual, [r, r, r, r, r, b, g, r, r, b, g, r].concat());
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn invalid_default_targets_preserve_output_and_bindings() {
    let (mut textures, mut handle, queue) = setup(false);
    let owner = textures.image(&handle).unwrap().raw.owner.clone();
    let output = textures.default_target(DeviceIntSize::new(2, 2)).unwrap();
    assert!(textures.default_target(DeviceIntSize::new(0, 2)).is_err());
    assert!(textures.default_target(DeviceIntSize::new(-1, 2)).is_err());
    assert!(textures
        .default_target(DeviceIntSize::new(i32::MAX, i32::MAX))
        .is_err());
    let mut state = RenderPassState::default();
    textures.bind(TextureSlot(0), &handle).unwrap();
    let mut commands = queue.recording().unwrap();
    let mut invalid = default_descriptor(2, 2);
    invalid.depth_load = LoadOp::Clear(1.0);
    assert!(state.begin(&mut commands, &mut textures, &invalid).is_err());
    for viewport in [
        DeviceIntRect::zero(),
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(-1, 0), DeviceIntSize::new(2, 2)),
        DeviceIntRect::from_size(DeviceIntSize::new(
            owner.max_viewport_dimensions[0] as i32 + 1, 2,
        )),
        DeviceIntRect::from_size(DeviceIntSize::new(
            2, owner.max_viewport_dimensions[1] as i32 + 1,
        )),
        DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(owner.viewport_bounds_range[1] as i32, 0),
            DeviceIntSize::new(1, 1),
        ),
        DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(0, owner.viewport_bounds_range[1] as i32),
            DeviceIntSize::new(1, 1),
        ),
    ] {
        invalid = default_descriptor(2, 2);
        if let DrawTarget::Default { ref mut rect, .. } = invalid.target {
            *rect = viewport.cast_unit();
        }
        assert!(state.begin(&mut commands, &mut textures, &invalid).is_err());
    }
    for size in [
        DeviceIntSize::new(0, 2),
        DeviceIntSize::new(2, 0),
        DeviceIntSize::new(-1, 2),
    ] {
        invalid = default_descriptor(2, 2);
        if let DrawTarget::Default { ref mut total_size, .. } = invalid.target {
            *total_size = size.cast_unit();
        }
        assert!(state.begin(&mut commands, &mut textures, &invalid).is_err());
    }
    assert!(state.draw_pass().is_err());
    assert!(Rc::ptr_eq(&output, &textures.output().unwrap()));
    assert!(textures.bindings()[0].is_some());
    let valid = default_descriptor(2, 2);
    state.begin(&mut commands, &mut textures, &valid).unwrap();
    let mut pass = state.draw_pass().unwrap();
    pass.viewport = Some(DeviceIntRect::zero());
    assert!(pass
        .clear_rect(&mut commands, left(), Some([1.0; 4]), None)
        .is_err());
    state.end(&mut commands, StoreOp::Store).unwrap();
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
