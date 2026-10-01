/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{Options, Texture, TextureFilter};
use super::super::render_device::RenderDevice;
use super::super::tests::{validation_logging, ERRORS};
use api::{
    ImageBufferKind,
    units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize},
};
use crate::device::{
    DrawTarget, LoadOp, RenderPassDescriptor, RenderState, StoreOp, TextureSlot,
    UploadBufferMapping, UploadChunk,
};
use crate::renderer::desc;
use euclid::default::Transform3D;
use std::{rc::Rc, sync::atomic::Ordering};

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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn reported_capabilities_drive_mapped_upload_copy_and_base_instance_draws() {
    let owner = device();
    let mut device = RenderDevice::new(&owner).unwrap();
    assert_eq!(device.properties.api_info.kind, GraphicsApi::Vulkan);
    assert_eq!(device.properties.api_info.renderer, owner.info().name);
    assert_eq!(device.properties.api_info.version, owner.info().driver_info);
    assert!(device.properties.max_texture_size >= 2);
    assert!(matches!(
        device.properties.upload_method,
        UploadMethod::PixelBuffer(_)
    ));
    let format = device.properties.color_formats.external;
    assert_eq!(format, device.properties.color_formats.internal);
    let kind = if device.properties.capabilities.supports_texture_rect {
        ImageBufferKind::TextureRect
    } else {
        ImageBufferKind::Texture2D
    };
    let features: &[&'static str] = if kind == ImageBufferKind::TextureRect {
        &["TEXTURE_RECT"]
    } else {
        &["TEXTURE_2D"]
    };
    let size = DeviceIntSize::new(2, 1);
    device.begin_frame().unwrap();
    let mut source = device
        .textures
        .create(kind, format, size, TextureFilter::Nearest, None)
        .unwrap();
    let mut target = device
        .textures
        .create(kind, format, size, TextureFilter::Nearest, None)
        .unwrap();
    let (length, stride) = device.uploads.layout(size, format).unwrap();
    let offset = if device
        .properties
        .capabilities
        .supports_upload_buffer_offsets
    {
        length
    } else {
        0
    };
    let mut buffer = device.uploads.create().unwrap();
    let mapping = device
        .uploads
        .allocate(
            &mut buffer,
            length + offset,
            device
                .properties
                .capabilities
                .supports_persistent_upload_buffers,
        )
        .unwrap();
    let pointer = match mapping {
        UploadBufferMapping::Transient(p) | UploadBufferMapping::Persistent(p) => p,
        _ => unreachable!(),
    };
    let pixel = match format {
        ImageFormat::BGRA8 => [0, 0, 255, 255],
        ImageFormat::RGBA8 => [255, 0, 0, 255],
        _ => panic!("unexpected color format"),
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            pixel.repeat(2).as_ptr(),
            pointer.as_ptr().cast::<u8>().add(offset),
            8,
        );
    }
    device
        .flush_upload_buffer(
            &buffer,
            &mapping,
            length + offset,
            &[UploadChunk {
                texture: &source,
                rect: DeviceIntRect::from_size(size),
                stride: Some(stride as i32),
                offset,
                format_override: None,
            }],
        )
        .unwrap();
    device
        .copy_texture_sub_region(&source, 0, 0, &target, 0, 0, 2, 1)
        .unwrap();
    let mut program = device.programs.create("cs_scale", features, false).unwrap();
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
        .set_transform(&Transform3D::ortho(
            0.0,
            2.0,
            0.0,
            1.0,
            device.properties.ortho_near_plane(),
            device.properties.ortho_far_plane(),
        ));
    let base = u32::from(device.properties.capabilities.supports_base_instance);
    let mut bytes = vec![0; base as usize * 36];
    bytes.extend(
        [0.0f32, 0.0, 2.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0]
            .iter()
            .flat_map(|f| f.to_ne_bytes()),
    );
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
        .create(&desc::SCALE, &vertices, Some(&instances), None, 1)
        .unwrap();
    device
        .vertex_arrays
        .write_buffer(&mut instances, &bytes)
        .unwrap();
    device.vertex_arrays.bind(&vao).unwrap();
    device
        .begin_render_pass(&RenderPassDescriptor {
            target: DrawTarget::new_default(size, device.properties.surface_origin_is_top_left()),
            render_area: None,
            color_load: LoadOp::Clear([0.0; 4]),
            depth_load: LoadOp::Load,
        })
        .unwrap();
    device.textures.bind(TextureSlot(0), &target).unwrap();
    device
        .bind_pipeline(&program, RenderState::default())
        .unwrap();
    device.draw_instanced(base, 1).unwrap();
    device.end_render_pass(StoreOp::Store).unwrap();
    let serial = device.end_frame().unwrap();
    device.submissions.wait_for(serial).unwrap();
    assert_eq!(
        device
            .textures
            .output()
            .unwrap()
            .readback(DeviceIntRect::from_size(size))
            .unwrap()
            .wait()
            .unwrap(),
        [255, 0, 0, 255].repeat(2)
    );
    device.uploads.delete(&mut buffer).unwrap();
    device.textures.delete(&mut source).unwrap();
    device.textures.delete(&mut target).unwrap();
    device.programs.delete(&mut program).unwrap();
    device.vertex_arrays.delete(&mut vao).unwrap();
    device.vertex_arrays.delete_buffer(&mut vertices).unwrap();
    device.vertex_arrays.delete_buffer(&mut instances).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn reported_dual_source_support_and_depth_range_are_usable() {
    use super::super::{draw::DrawPass, pipeline::DrawPipeline, shaders};
    use crate::device::BlendMode;

    let owner = device();
    let properties = RendererProperties::new(&owner);
    if properties.capabilities.supports_dual_source_blending {
        let shader = shaders::SHADERS
            .iter()
            .find(|s| s.features.contains("DUAL_SOURCE_BLENDING"))
            .unwrap();
        DrawPipeline::new(
            &owner,
            shader,
            wgt::TextureFormat::Rgba8Unorm,
            false,
            RenderState {
                blend_mode: BlendMode::SubpixelDualSource,
                ..RenderState::default()
            },
        )
        .unwrap();
    }
    assert_eq!(properties.shader_feature_flags(), ShaderFeatureFlags::GL);
    let texture = Texture::new(
        &owner,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let pass = DrawPass {
        target: &texture,
        origin: DeviceIntPoint::zero(),
        viewport: None,
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let projection = pass.projection(&Transform3D::ortho(
        0.0,
        1.0,
        0.0,
        1.0,
        properties.ortho_near_plane(),
        properties.ortho_far_plane(),
    ));
    let max = properties.max_depth_ids();
    let values: Vec<_> = [0, 1, 2, max / 2 - 1, max / 2, max - 2, max - 1]
        .iter()
        .map(|&z| projection[10] * z as f32 + projection[14])
        .collect();
    assert!(values.iter().all(|z| (0.0..=1.0).contains(z)));
    assert!(values.windows(2).all(|z| z[0] > z[1]));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
