/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{BufferPool, Device, Options, SubmissionQueue, TextureFilter};
use super::super::tests::{validation_logging, ERRORS};
use api::{ImageBufferKind, ImageFormat, units::DeviceIntSize};
use crate::device::{DrawTarget, Texture as TextureHandle, TextureSlot};
use crate::internal_types::RenderTargetInfo;
use std::sync::atomic::Ordering;

#[path = "default_target_tests.rs"]
mod default_target;

fn setup(depth: bool) -> (TextureStore, TextureHandle, SubmissionQueue) {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&device)), 2).unwrap();
    let mut textures = TextureStore::new(&device);
    let handle = textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(2, 2),
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: depth }),
        )
        .unwrap();
    (textures, handle, queue)
}

fn pass_state(queue: &SubmissionQueue) -> RenderPassState {
    let quad = Buffer::new(queue.owner(),
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX).unwrap();
    RenderPassState::new(&quad)
}

fn descriptor(handle: &TextureHandle, depth: bool) -> RenderPassDescriptor {
    RenderPassDescriptor {
        target: DrawTarget::from_texture(handle, depth),
        render_area: None,
        color_load: LoadOp::Load,
        depth_load: LoadOp::Load,
    }
}

fn pixels(texture: &Rc<Texture>) -> Vec<u8> {
    texture
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
        .unwrap()
        .wait()
        .unwrap()
}

fn left() -> DeviceIntRect {
    DeviceIntRect::from_size(DeviceIntSize::new(1, 2))
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pass_load_clears_apply_once_even_without_draws() {
    let (mut textures, mut handle, queue) = setup(false);
    let image = textures.image(&handle).unwrap();
    let mut state = pass_state(&queue);
    let mut desc = descriptor(&handle, false);
    desc.color_load = LoadOp::Clear([1.0, 0.0, 0.0, 1.0]);
    desc.render_area = Some(left());
    state.set_scissor_rect(left().cast_unit());
    state.enable_scissor();
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc, None).unwrap();
        assert_eq!(
            state.scissor_rect().unwrap(),
            DeviceIntRect::from_size(DeviceIntSize::new(2, 2))
        );
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    queue.wait().unwrap();
    assert_eq!(pixels(&image), [255, 0, 0, 255].repeat(4));
    desc.color_load = LoadOp::Load;
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc, None).unwrap();
        textures.delete(&mut handle).unwrap();
        state.set_scissor_rect(left().cast_unit());
        state.enable_scissor();
        state
            .clear(&mut commands, Some([0.0, 1.0, 0.0, 1.0]), None, None)
            .unwrap();
        state
            .clear(
                &mut commands,
                Some([0.0, 0.0, 1.0, 1.0]),
                None,
                Some(
                    DeviceIntRect::from_origin_and_size(
                        DeviceIntPoint::new(1, 1),
                        DeviceIntSize::new(1, 1),
                    )
                    .cast_unit(),
                ))
            .unwrap();
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    queue.wait().unwrap();
    assert_eq!(
        pixels(&image),
        [0, 255, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn partial_or_unspecified_pass_areas_preserve_cached_pixels() {
    let (mut textures, mut handle, queue) = setup(false);
    let image = textures.image(&handle).unwrap();
    let mut state = pass_state(&queue);
    let mut desc = descriptor(&handle, false);
    for area in [
        None,
        Some(left()),
        Some(DeviceIntRect::from_size(DeviceIntSize::new(2, 2))),
    ] {
        image
            .upload(
                &queue,
                DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
                &[255, 0, 0, 255].repeat(4),
                None,
                0,
                None,
            )
            .unwrap();
        desc.color_load = LoadOp::DontCare;
        desc.render_area = area;
        let full = area == Some(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)));
        {
            let mut commands = queue.recording().unwrap();
            state.begin(&mut commands, &mut textures, &desc, None).unwrap();
            assert_eq!(image.initialized(), !full);
            state
                .clear(
                    &mut commands,
                    Some([0.0, 1.0, 0.0, 1.0]),
                    None,
                    Some(left().cast_unit()))
                .unwrap();
            state.end(&mut commands, StoreOp::Store).unwrap();
        }
        queue.wait().unwrap();
        let actual = pixels(&image);
        if full {
            assert_eq!(&actual[..4], &[0, 255, 0, 255]);
            assert_eq!(&actual[8..12], &[0, 255, 0, 255]);
            assert!(image.raw.owner.trace.borrow().iter().any(|event| {
                matches!(event, crate::device::wgpu::tests::Command::BeginPass(color, _) if color.contains(hal::AttachmentOps::LOAD_DONT_CARE))
            }));
        } else {
            assert_eq!(actual, [[0, 255, 0, 255], [255, 0, 0, 255]].concat().repeat(2));
        }
    }
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn depth_store_and_abandoned_discards_track_attachment_contents() {
    use super::super::draw::DrawBatch;
    use super::super::pipeline::DrawPipeline;
    use super::super::shader::select_draw_shader;
    use super::super::{wgt, Buffer};
    use crate::device::{DepthFunction, RenderState};
    use euclid::default::Transform3D;

    let (mut textures, mut handle, queue) = setup(true);
    let (image, depth) = textures.render_target(handle.target_id, true).unwrap();
    let depth = depth.unwrap();
    let owner = &image.raw.owner;
    let quad = Buffer::new(
        owner,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let pipeline = DrawPipeline::new(
        owner,
        select_draw_shader("ps_clear", &[], false).unwrap(),
        image.format(),
        true,
        RenderState {
            depth_test: Some(DepthFunction::Less),
            ..RenderState::default()
        },
    )
    .unwrap();
    let instances: Vec<_> = [-1.0f32, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0]
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let probe = |state: &RenderPassState, commands: &mut Recording<'_>| {
        state.flush(commands, StoreOp::Store).unwrap();
        let mut pass = state.draw_pass().unwrap().unwrap();
        pass.depth_range = 0.0..0.5;
        let projection = pass.projection(&Transform3D::identity());
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
                instances: &instances,
                instance_count: 1,
                scissor: state.scissor_rect().unwrap(),
            }],
        )
        .unwrap();
    };
    let mut state = pass_state(&queue);
    let mut desc = descriptor(&handle, true);
    desc.color_load = LoadOp::Clear([1.0, 0.0, 0.0, 1.0]);
    desc.depth_load = LoadOp::Clear(0.25);
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc, None).unwrap();
        probe(&state, &mut commands);
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    queue.wait().unwrap();
    assert!(image.initialized() && depth.initialized());
    assert_eq!(pixels(&image), [255, 0, 0, 255].repeat(4));
    desc.color_load = LoadOp::DontCare;
    desc.depth_load = LoadOp::DontCare;
    desc.render_area = Some(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)));
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc, None).unwrap();
        assert!(!image.initialized() && !depth.initialized());
        state.end(&mut commands, StoreOp::Discard).unwrap();
    }
    queue.discard_recording();
    assert!(image.initialized() && depth.initialized());
    assert_eq!(pixels(&image), [255, 0, 0, 255].repeat(4));
    desc.color_load = LoadOp::Load;
    desc.depth_load = LoadOp::Load;
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc, None).unwrap();
        state.end(&mut commands, StoreOp::Discard).unwrap();
    }
    queue.wait().unwrap();
    assert!(image.initialized() && !depth.initialized());
    {
        let mut commands = queue.recording().unwrap();
        state.begin(&mut commands, &mut textures, &desc, None).unwrap();
        probe(&state, &mut commands);
        state.end(&mut commands, StoreOp::Store).unwrap();
    }
    queue.wait().unwrap();
    assert_eq!(pixels(&image), [0, 255, 0, 255].repeat(4));
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pass_errors_preserve_bindings_and_active_state() {
    let (mut textures, mut handle, queue) = setup(false);
    let mut state = pass_state(&queue);
    let desc = descriptor(&handle, false);
    textures.bind(TextureSlot(0), &handle).unwrap();
    textures.bind(TextureSlot(5), &handle).unwrap();
    let mut commands = queue.recording().unwrap();
    assert!(state.end(&mut commands, StoreOp::Store).is_err());
    assert!(state.draw_pass().is_err());
    assert!(state.scissor_rect().is_err());
    for value in [0.5, -1.0, f32::NAN] {
        let mut invalid = desc;
        invalid.depth_load = LoadOp::Clear(value);
        assert!(state.begin(&mut commands, &mut textures, &invalid, None).is_err());
        assert!(state.draw_pass().is_err());
        assert!(textures.bindings()[0].is_some());
    }
    let mut invalid = desc;
    if let DrawTarget::Texture {
        ref mut dimensions, ..
    } = invalid.target
    {
        dimensions.width += 1;
    }
    assert!(state.begin(&mut commands, &mut textures, &invalid, None).is_err());
    invalid.target = DrawTarget::new_default(DeviceIntSize::new(2, 2), false);
    assert!(state.begin(&mut commands, &mut textures, &invalid, None).is_err());
    state.begin(&mut commands, &mut textures, &desc, None).unwrap();
    assert!(textures.bindings()[0].is_none());
    assert!(textures.bindings()[5].is_some());
    assert!(state.begin(&mut commands, &mut textures, &desc, None).is_err());
    state.set_scissor_rect(
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(-1, 0), DeviceIntSize::new(2, 3))
            .cast_unit(),
    );
    state.enable_scissor();
    assert_eq!(state.scissor_rect().unwrap(), left());
    state.set_scissor_rect(
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(3, 3), DeviceIntSize::new(1, 1))
            .cast_unit(),
    );
    assert!(state.scissor_rect().unwrap().is_empty());
    state.disable_scissor();
    assert_eq!(
        state.scissor_rect().unwrap(),
        DeviceIntRect::from_size(DeviceIntSize::new(2, 2))
    );
    state.end(&mut commands, StoreOp::Store).unwrap();
    assert!(state.draw_pass().is_err());
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn scoped_clears_share_uniform_storage_and_preserve_order() {
    use super::super::tests::Command;
    let (mut textures, mut handle, queue) = setup(false);
    let image = textures.image(&handle).unwrap();
    let owner = &image.raw.owner;
    let mut state = pass_state(&queue);
    let mut desc = descriptor(&handle, false);
    desc.color_load = LoadOp::Clear([1.0, 0.0, 0.0, 1.0]);
    let slots = 65536 / owner.capabilities.limits.min_uniform_buffer_offset_alignment.max(64);
    let mut previous_capacity = 0;
    for count in [0, 100, slots + 10, 100] {
        owner.trace.borrow_mut().clear();
        {
            let mut commands = queue.recording().unwrap();
            state.begin(&mut commands, &mut textures, &desc, None).unwrap();
            for i in 0..count {
                state.clear(&mut commands, Some([0.0, i as f32 / (count - 1) as f32, 1.0, 1.0]),
                    None, (i != 0).then(|| left().cast_unit())).unwrap();
            }
            state.end(&mut commands, StoreOp::Store).unwrap();
        }
        assert!(state.spare_commands.capacity() >= previous_capacity);
        previous_capacity = state.spare_commands.capacity();
        queue.wait().unwrap();
        let expected = if count == 0 {
            [255, 0, 0, 255].repeat(4)
        } else {
            [0, 255, 255, 255, 0, 0, 255, 255].repeat(2)
        };
        assert_eq!(pixels(&image), expected);
        let trace = owner.trace.borrow();
        let arenas = count.div_ceil(slots) as usize;
        assert_eq!(trace.iter().filter(|c| matches!(c, Command::ClearBindGroup)).count(), arenas);
        assert_eq!(trace.iter().filter(|c| matches!(c, Command::UniformArena(_))).count(), arenas);
        assert_eq!(trace.iter().filter(|c| matches!(c, Command::BeginPass(..))).count(), 1);
        assert_eq!(trace.iter().filter(|c| matches!(c, Command::EndPass)).count(), 1);
    }
    textures.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
