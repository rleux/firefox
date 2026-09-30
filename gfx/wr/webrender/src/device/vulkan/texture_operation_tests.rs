/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::internal_types::RenderTargetInfo;

fn texture(
    device: &mut RenderDevice,
    size: DeviceIntSize,
    format: ImageFormat,
    renderable: bool,
) -> TextureHandle {
    device
        .textures
        .create(
            ImageBufferKind::Texture2D,
            format,
            size,
            TextureFilter::Nearest,
            renderable.then_some(RenderTargetInfo { has_depth: false }),
        )
        .unwrap()
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> FramebufferIntRect {
    DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(x, y), DeviceIntSize::new(w, h))
        .cast_unit()
}

fn read(texture: &Rc<Texture>) -> Vec<u8> {
    let size = texture.size();
    texture
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(
            size.width as i32,
            size.height as i32,
        )))
        .unwrap()
        .wait()
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_handle_copies_preserve_atlas_regions_and_check_coordinates() {
    let mut device = device();
    device.begin_frame().unwrap();
    let mut source = texture(
        &mut device,
        DeviceIntSize::new(3, 2),
        ImageFormat::RGBA8,
        false,
    );
    let mut target = texture(
        &mut device,
        DeviceIntSize::new(5, 3),
        ImageFormat::RGBA8,
        false,
    );
    let bytes: Vec<_> = (0..6u8)
        .flat_map(|v| [v * 31, v * 17, v * 7, 255])
        .collect();
    device.upload_texture_immediate(&source, &bytes).unwrap();
    device
        .copy_texture_sub_region(&source, 1, 0, &target, 2, 1, 2, 2)
        .unwrap();
    device
        .copy_texture_sub_region(&source, 0, 0, &target, 0, 0, 1, 1)
        .unwrap();
    assert!(device
        .copy_texture_sub_region(&source, usize::MAX, 0, &target, 0, 0, 2, 2)
        .is_err());
    assert!(device
        .copy_texture_sub_region(&source, 0, 0, &target, i32::MAX as usize, 0, 2, 2)
        .is_err());
    device
        .copy_texture_sub_region(&source, usize::MAX, 0, &target, 0, 0, 0, 2)
        .unwrap();
    let image = device.textures.image(&target).unwrap();
    device.textures.delete(&mut source).unwrap();
    device.textures.delete(&mut target).unwrap();
    let serial = device.end_frame().unwrap();
    device.submissions.wait_for(serial).unwrap();
    let mut expected = vec![0; 5 * 3 * 4];
    expected[..4].copy_from_slice(&bytes[..4]);
    for y in 0..2 {
        expected[((y + 1) * 5 + 2) * 4..((y + 1) * 5 + 4) * 4]
            .copy_from_slice(&bytes[(y * 3 + 1) * 4..(y * 3 + 3) * 4]);
    }
    assert_eq!(read(&image), expected);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn target_blits_preserve_bound_program_pass_and_scissor() {
    let mut device = device();
    assert!(device.textures.read_target(ReadTarget::Default).is_err());
    device.begin_frame().unwrap();
    let mut source = texture(
        &mut device,
        DeviceIntSize::new(2, 2),
        ImageFormat::RGBA8,
        true,
    );
    let mut target = texture(
        &mut device,
        DeviceIntSize::new(4, 2),
        ImageFormat::BGRA8,
        true,
    );
    device
        .upload_texture_immediate(&source, &[255, 0, 0, 255, 0, 0, 255, 255].repeat(2))
        .unwrap();
    let mut program = device.programs.create("ps_clear", &[], false).unwrap();
    device.programs.link(&mut program, &desc::CLEAR).unwrap();
    let mut vao = device.vertex_arrays.create(&desc::CLEAR, 1).unwrap();
    device
        .vertex_arrays
        .update_instances(
            &vao,
            &floats(&[-1.0, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0]),
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
    device
        .blit_render_target(
            ReadTarget::from_texture(&source),
            rect(0, 0, 2, 2),
            descriptor().target,
            rect(0, 0, 2, 1),
            TextureFilter::Nearest,
        )
        .unwrap();
    assert!(!device
        .bind_pipeline(&program, RenderState::default())
        .unwrap());
    device.draw_instanced(0, 1).unwrap();
    device
        .blit_render_target(
            ReadTarget::Default,
            rect(0, 0, 2, 1),
            DrawTarget::from_texture(&target, false),
            rect(0, 0, 4, 2),
            TextureFilter::Nearest,
        )
        .unwrap();
    scissor(&device, 1);
    device.draw_instanced(0, 1).unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    let serial = device.end_frame().unwrap();
    device.submissions.wait_for(serial).unwrap();
    let g = [0, 255, 0, 255];
    let b = [255, 0, 0, 255];
    assert_eq!(
        read(&device.textures.image(&target).unwrap()),
        [g, g, b, b].concat().repeat(2)
    );
    assert_eq!(pixels(&device.textures.output().unwrap()), g.repeat(2));
    device.textures.delete(&mut source).unwrap();
    device.textures.delete(&mut target).unwrap();
    device.programs.delete(&mut program).unwrap();
    device.vertex_arrays.delete(&mut vao).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn overlapping_blits_and_invalidated_targets_keep_copy_order() {
    let mut device = device();
    device.begin_frame().unwrap();
    let mut target = texture(
        &mut device,
        DeviceIntSize::new(2, 2),
        ImageFormat::RGBA8,
        true,
    );
    let mut snapshot = texture(
        &mut device,
        DeviceIntSize::new(2, 2),
        ImageFormat::RGBA8,
        false,
    );
    let mut source = texture(
        &mut device,
        DeviceIntSize::new(1, 1),
        ImageFormat::RGBA8,
        false,
    );
    device
        .upload_texture_immediate(&target, &[255, 0, 0, 255, 0, 0, 255, 255].repeat(2))
        .unwrap();
    device
        .upload_texture_immediate(&source, &[0, 255, 0, 255])
        .unwrap();
    let draw_target = DrawTarget::from_texture(&target, false);
    device
        .begin_render_pass(&RenderPassDescriptor {
            target: draw_target,
            render_area: None,
            color_load: LoadOp::Load,
            depth_load: LoadOp::Load,
        })
        .unwrap();
    device
        .blit_render_target(
            ReadTarget::from_texture(&target),
            rect(0, 0, 1, 2),
            draw_target,
            rect(1, 0, 1, 2),
            TextureFilter::Nearest,
        )
        .unwrap();
    device
        .copy_texture_sub_region(&target, 0, 0, &snapshot, 0, 0, 2, 2)
        .unwrap();
    device.textures.bind(TextureSlot(5), &target).unwrap();
    device.invalidate_render_target(&target).unwrap();
    assert!(device.textures.bindings()[5].is_none());
    device
        .copy_texture_sub_region(&source, 0, 0, &target, 0, 0, 1, 1)
        .unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    let serial = device.end_frame().unwrap();
    device.submissions.wait_for(serial).unwrap();
    assert_eq!(
        read(&device.textures.image(&snapshot).unwrap()),
        [255, 0, 0, 255].repeat(4)
    );
    assert_eq!(
        read(&device.textures.image(&target).unwrap()),
        [[0, 255, 0, 255].to_vec(), vec![0; 12]].concat()
    );
    device.textures.delete(&mut target).unwrap();
    device.textures.delete(&mut snapshot).unwrap();
    device.textures.delete(&mut source).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
