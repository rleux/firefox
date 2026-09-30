/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::tests::{validation_logging, ERRORS};
use super::super::Options;
use api::{ImageBufferKind, ImageFormat, units::DeviceIntPoint};
use crate::device::{DrawTarget, LoadOp, TextureSlot};
use crate::renderer::desc;
use euclid::default::Transform3D;
use std::sync::atomic::Ordering;

#[path = "frame_tests.rs"]
mod frame;

#[path = "texture_update_tests.rs"]
mod texture_update;

fn device() -> RenderDevice {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    RenderDevice::new(&device).unwrap()
}

fn descriptor() -> RenderPassDescriptor {
    RenderPassDescriptor {
        target: DrawTarget::new_default(DeviceIntSize::new(2, 1), true),
        render_area: None,
        color_load: LoadOp::Clear([0.0, 0.0, 0.0, 1.0]),
        depth_load: LoadOp::Load,
    }
}

fn floats(data: &[f32]) -> Vec<u8> {
    data.iter().flat_map(|v| v.to_ne_bytes()).collect()
}

fn scissor(device: &RenderDevice, x: i32) {
    device.passes.set_scissor_rect(
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(x, 0), DeviceIntSize::new(1, 1))
            .cast_unit(),
    );
    device.passes.enable_scissor();
}

fn pixels(texture: &Rc<Texture>) -> Vec<u8> {
    texture
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 1)))
        .unwrap()
        .wait()
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_device_orders_texture_updates_between_instanced_draws() {
    let mut device = device();
    device.begin_frame().unwrap();
    let mut source = device
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(4, 4),
            TextureFilter::Trilinear,
            None,
        )
        .unwrap();
    device
        .upload_texture_immediate(&source, &[255, 0, 0, 255].repeat(16))
        .unwrap();
    let mut program = device
        .programs
        .create("cs_scale", &["TEXTURE_2D"], false)
        .unwrap();
    device.programs.link(&mut program, &desc::SCALE).unwrap();
    device
        .programs
        .state_mut(&program)
        .unwrap()
        .bind_samplers(&[("sColor0", TextureSlot(0))]);
    device
        .programs
        .state(&program)
        .unwrap()
        .set_transform(&Transform3D::ortho(0.0, 2.0, 0.0, 1.0, -1.0, 1.0));
    let mut vao = device.vertex_arrays.create(&desc::SCALE, 1).unwrap();
    device
        .vertex_arrays
        .update_instances(
            &vao,
            &floats(&[0.0, 0.0, 2.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0]),
            36,
            None,
        )
        .unwrap();
    device.vertex_arrays.bind(&vao).unwrap();
    device.begin_render_pass(&descriptor()).unwrap();
    device.textures.bind(TextureSlot(0), &source).unwrap();
    assert!(device
        .bind_pipeline(&program, RenderState::default())
        .unwrap());
    scissor(&device, 0);
    device.draw_instanced(0, 1).unwrap();
    device
        .upload_texture_immediate(&source, &[0, 0, 255, 255].repeat(16))
        .unwrap();
    scissor(&device, 1);
    assert!(!device
        .bind_pipeline(&program, RenderState::default())
        .unwrap());
    device.draw_instanced(0, 1).unwrap();
    device.textures.delete(&mut source).unwrap();
    device.programs.delete(&mut program).unwrap();
    device.vertex_arrays.delete(&mut vao).unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    device.end_frame().unwrap();
    let output = device.textures.output().unwrap();
    device.submissions.wait().unwrap();
    drop(device);
    assert_eq!(pixels(&output), [255, 0, 0, 255, 0, 0, 255, 255]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_device_preserves_instance_versions_and_clear_order() {
    let mut device = device();
    device.begin_frame().unwrap();
    let mut program = device.programs.create("ps_clear", &[], false).unwrap();
    device.programs.link(&mut program, &desc::CLEAR).unwrap();
    let mut vao = device.vertex_arrays.create(&desc::CLEAR, 1).unwrap();
    device
        .vertex_arrays
        .update_instances(
            &vao,
            &floats(&[
                -1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, -1.0, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0,
            ]),
            32,
            None,
        )
        .unwrap();
    device.vertex_arrays.bind(&vao).unwrap();
    device.begin_render_pass(&descriptor()).unwrap();
    device
        .bind_pipeline(&program, RenderState::default())
        .unwrap();
    scissor(&device, 0);
    device.draw_instanced(1, 1).unwrap();
    device
        .vertex_arrays
        .update_range(vao.instance_vbo_id(), 48, &floats(&[0.0, 0.0, 1.0, 1.0]))
        .unwrap();
    scissor(&device, 1);
    device.draw_instanced(1, 1).unwrap();
    device
        .clear_target(Some([1.0, 0.0, 0.0, 1.0]), None, None)
        .unwrap();
    device.programs.delete(&mut program).unwrap();
    device.vertex_arrays.delete(&mut vao).unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    device.end_frame().unwrap();
    device.submissions.wait().unwrap();
    assert_eq!(
        pixels(&device.textures.output().unwrap()),
        [0, 255, 0, 255, 255, 0, 0, 255]
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_device_validates_state_and_supplies_unbound_color_fallback() {
    let mut device = device();
    device.begin_frame().unwrap();
    assert!(device.draw_instanced(0, 0).is_err());
    assert!(device.end_render_pass(StoreOp::Store).is_err());
    device.begin_render_pass(&descriptor()).unwrap();
    device.draw_instanced(u32::MAX, 0).unwrap();
    assert!(device.draw_instanced(0, 1).is_err());
    let mut program = device
        .programs
        .create("cs_scale", &["TEXTURE_2D"], false)
        .unwrap();
    device.programs.link(&mut program, &desc::SCALE).unwrap();
    device
        .programs
        .state(&program)
        .unwrap()
        .set_transform(&Transform3D::ortho(0.0, 2.0, 0.0, 1.0, -1.0, 1.0));
    let mut vao = device.vertex_arrays.create(&desc::SCALE, 1).unwrap();
    device.vertex_arrays.bind(&vao).unwrap();
    device
        .vertex_arrays
        .update_instances(
            &vao,
            &floats(&[0.0, 0.0, 2.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0]),
            36,
            None,
        )
        .unwrap();
    assert!(device.draw_instanced(0, 1).is_err());
    device
        .bind_pipeline(&program, RenderState::default())
        .unwrap();
    assert!(device.draw_instanced(1, 1).is_err());
    device.draw_instanced(0, 1).unwrap();
    device
        .programs
        .state_mut(&program)
        .unwrap()
        .bind_samplers(&[("sColor0", TextureSlot(99))]);
    assert!(device.draw_instanced(0, 1).is_err());
    device.programs.delete(&mut program).unwrap();
    device.vertex_arrays.delete(&mut vao).unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    device.end_frame().unwrap();
    device.submissions.wait().unwrap();
    assert_eq!(pixels(&device.textures.output().unwrap()), [255; 8]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
