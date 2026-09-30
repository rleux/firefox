/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{shaders, wgt, BufferPool, Device, Options, Samplers, SubmissionQueue};
use super::super::shader::select_draw_shader;
use super::super::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntPoint, DeviceIntSize};
use crate::device::RenderState;
use std::sync::atomic::Ordering;

fn device() -> Rc<Device> {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    device
}

fn create_pipeline(device: &Rc<Device>, shader: &'static ShaderArtifact) -> Rc<DrawPipeline> {
    let format = if shader.name == "ps_quad_mask" || shader.features == "ALPHA_TARGET" {
        wgt::TextureFormat::R8Unorm
    } else {
        wgt::TextureFormat::Rgba8Unorm
    };
    DrawPipeline::new(device, shader, format, false, RenderState::default()).unwrap()
}

fn pass(target: &Rc<Texture>) -> DrawPass<'_> {
    DrawPass {
            viewport: None,
        target,
        origin: DeviceIntPoint::zero(),
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    }
}

#[test]
fn program_sampler_assignments_follow_active_shader_names() {
    for shader in shaders::SHADERS {
        let mut program = ProgramState::new(shader);
        assert!(program
            .texture_slots
            .iter()
            .chain(&program.storage_slots)
            .all(|slot| *slot == 0));
        let names: Vec<_> = shader
            .textures
            .iter()
            .map(|binding| binding.name)
            .chain(shader.storage_buffers.iter().map(|binding| binding.name))
            .collect();
        let bindings: Vec<_> = names
            .iter()
            .enumerate()
            .rev()
            .map(|(index, name)| (*name, TextureSlot(index + 7)))
            .collect();
        program.bind_samplers(&bindings);
        program.bind_samplers(&[("optimized_out_sampler", TextureSlot(usize::MAX))]);
        assert_eq!(
            program
                .texture_slots
                .iter()
                .chain(&program.storage_slots)
                .copied()
                .collect::<Vec<_>>(),
            (7..7 + names.len()).collect::<Vec<_>>()
        );
        if let Some(name) = names.first() {
            program.bind_samplers(&[(name, TextureSlot(2)), (name, TextureSlot(3))]);
            let slots: Vec<_> = program
                .texture_slots
                .iter()
                .chain(&program.storage_slots)
                .copied()
                .collect();
            assert_eq!(slots[0], 3);
            assert_eq!(&slots[1..], &(8..7 + names.len()).collect::<Vec<_>>());
        }
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn resolved_programs_preserve_uniforms_and_textures_for_queued_draws() {
    let device = device();
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
    let pipeline = create_pipeline(&device, shader);
    let mut program = ProgramState::new(shader);
    let target =
        Texture::new(&device, 2, 2, pipeline.format, TextureFilter::Nearest, true).unwrap();
    let pass = pass(&target);
    let mut slots = vec![None];
    for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
        let texture = Texture::new(
            &device,
            1,
            1,
            target.format(),
            TextureFilter::Nearest,
            false,
        )
        .unwrap();
        texture
            .upload(
                &queue,
                DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
                &color,
                None,
                0,
                None,
            )
            .unwrap();
        slots.push(Some(ShaderResource::Texture {
            texture,
            filter: Some(TextureFilter::Linear),
        }));
    }
    program.bind_samplers(&[("sColor0", TextureSlot(1)), ("unused", TextureSlot(99))]);
    let mut transform = Transform3D::ortho(0.0, 2.0, 0.0, 2.0, -1.0, 1.0);
    transform.m11 *= 0.5;
    program.set_transform(&transform);
    let mut resolved_slots = Vec::new();
    let left = program.resolve(&pipeline, &pass, |slot| {
        resolved_slots.push(slot);
        slots.get(slot).cloned()
    }).unwrap();
    assert_eq!(resolved_slots, [1]);
    assert!(!left.textures.spilled() && !left.buffers.spilled());
    assert_eq!(left.textures[0].1, TextureFilter::Linear);
    program.bind_samplers(&[("sColor0", TextureSlot(2))]);
    transform.m41 = 0.0;
    program.set_transform(&transform);
    let right = program.resolve(&pipeline, &pass, |slot| slots.get(slot).cloned()).unwrap();
    let weak_left = Rc::downgrade(&left.textures[0].0);
    let weak_right = Rc::downgrade(&right.textures[0].0);
    slots.clear();
    program.set_transform(&Transform3D::identity());
    program.bind_samplers(&[("sColor0", TextureSlot(usize::MAX))]);
    let error = program.resolve(&pipeline, &pass, |slot| slots.get(slot).cloned()).err().unwrap();
    assert!(error.contains("sColor0") && error.contains("slot"));
    let other = select_draw_shader(
        "cs_scale",
        &["TEXTURE_2D", "HAL_LEGACY_BRILINEAR"],
        false,
    )
    .unwrap();
    let other = create_pipeline(&device, other);
    assert!(program
        .resolve(&other, &pass, |slot| slots.get(slot).cloned())
        .err()
        .unwrap()
        .contains("variant"));
    let quad = Buffer::new(
        &device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let values = [0.0f32, 0.0, 2.0, 2.0, 0.0, 0.0, 1.0, 1.0, 0.0];
    let instances: Vec<_> = values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let full = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
    pass.record_batches(
        &mut queue.recording().unwrap(),
        &queue,
        &quad,
        Some(&samplers),
        &[
            left.batch(&instances, 1, full),
            right.batch(&instances, 1, full),
        ],
    )
    .unwrap();
    drop(left);
    drop(right);
    assert!(weak_left.upgrade().is_some() && weak_right.upgrade().is_some());
    queue.wait().unwrap();
    assert!(weak_left.upgrade().is_none() && weak_right.upgrade().is_none());
    assert_eq!(
        target.readback(full).unwrap().wait().unwrap(),
        [255, 0, 0, 255, 0, 0, 255, 255].repeat(2)
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
