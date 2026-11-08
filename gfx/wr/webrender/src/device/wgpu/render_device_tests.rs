/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::tests::{device, VALIDATION_ERRORS};
use super::*;
use crate::device as wr;
use crate::internal_types::{RenderTargetInfo, Swizzle};
use crate::renderer::desc;
use api::ImageBufferKind;
use std::convert::TryInto;

fn backend() -> wr::Device {
    wr::Device {
        backend: Box::new(RenderDevice::new(Arc::new(device())).unwrap()),
        pending_state: RenderState::default(),
        #[cfg(debug_assertions)]
        pipeline_bound: false,
    }
}
fn pass(size: DeviceIntSize) -> RenderPassDescriptor {
    RenderPassDescriptor {
        target: DrawTarget::new_default(size, true),
        render_area: None,
        color_load: LoadOp::Clear([0., 0., 1., 1.]),
        depth_load: LoadOp::Load,
    }
}
fn pixels(device: &mut wr::Device, size: DeviceIntSize) -> Vec<u8> {
    let mut bytes = vec![0; size.width as usize * size.height as usize * 4];
    device.read_pixels_into(
        ReadTarget::Default,
        DeviceIntRect::from_size(size).cast_unit(),
        ImageFormat::RGBA8,
        &mut bytes,
    );
    assert!(device.failure().is_none(), "{:?}", device.failure());
    bytes
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_snapshots_uniforms_instances_and_orders_scissored_clears() {
    let mut device = backend();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let size = DeviceIntSize::new(4, 2);
    device.begin_frame();
    let program = device
        .create_program_linked("ps_clear", &[], &desc::CLEAR, &[])
        .unwrap();
    let vertices = device.create_buffer(BufferKind::Vertex);
    let mut instances = device.create_buffer(BufferKind::Vertex);
    let array = device.create_vertex_array(&desc::CLEAR, &vertices, Some(&instances), None, 1);
    device.bind_vertex_array(&array);
    device.begin_render_pass(&pass(size));
    device.bind_program(&program);
    device.set_uniforms(&program, &Transform3D::ortho(0., 4., 0., 2., -1., 1.));
    device.write_buffer(
        &mut instances,
        &[0f32, 0., 2., 2., 1., 0., 0., 1.],
        VertexUsageHint::Stream,
    );
    device.draw_indexed_triangles_instanced_u16(6, 1);
    device.set_uniforms(&program, &Transform3D::ortho(-2., 2., 0., 2., -1., 1.));
    device.write_buffer_unsynchronized(&instances, 17, &crate::device::as_bytes(&[0f32, 1.])[1..]);
    device.draw_indexed_triangles_instanced_u16(6, 1);
    device.set_scissor(Some(DeviceIntRect::zero().cast_unit()));
    device.clear_rect(
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(1, 0), DeviceIntSize::new(2, 1))
            .cast_unit(),
        Some([1.; 4]),
        None,
    );
    device.end_render_pass(StoreOp::Store);
    device.end_frame();
    let actual = pixels(&mut device, size);
    assert_eq!(
        actual,
        [
            vec![255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0, 255, 0, 255],
            vec![255, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255]
        ]
        .concat()
    );
    device.delete_program(program);
    device.delete_vertex_array(array);
    device.delete_buffer(vertices);
    device.delete_buffer(instances);
    device.deinit();
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

fn check_streamed_instances(primary_mapping: Option<bool>) {
    let selected = device();
    let owner = Arc::new(if let Some(enabled) = primary_mapping {
        Device::new(&super::super::Options {
            adapter_name: Some(selected.info().name),
            validation: true,
            primary_mapping: Some(enabled),
            ..Default::default()
        })
        .unwrap()
    } else {
        selected
    });
    let mapping = if owner
        .features()
        .contains(wgt::Features::MAPPABLE_PRIMARY_BUFFERS)
    {
        wgt::BufferUsages::MAP_WRITE
    } else {
        wgt::BufferUsages::empty()
    };
    if let Some(enabled) = primary_mapping {
        assert_eq!(!mapping.is_empty(), enabled);
    }
    eprintln!(
        "Streaming adapter {:?}, primary mapping {}",
        owner.info(),
        !mapping.is_empty()
    );
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let mut native = RenderDevice::new(owner).unwrap();
    native.begin_frame().unwrap();
    let size = DeviceIntSize::new(6, 2);
    let mut program = GpuBackend::create_program(&mut native, "ps_clear", &[]).unwrap();
    GpuBackend::link_program(&mut native, &mut program, &desc::CLEAR, &[]).unwrap();
    let vertices = GpuBackend::create_buffer(&mut native, BufferKind::Vertex);
    let mut instances = GpuBackend::create_buffer(&mut native, BufferKind::Vertex);
    GpuBackend::write_buffer(
        &mut native,
        &mut instances,
        &[13; 64],
        VertexUsageHint::Stream,
    );
    let array = GpuBackend::create_vertex_array(
        &mut native,
        &desc::CLEAR,
        &vertices,
        Some(&instances),
        None,
        1,
    );
    GpuBackend::bind_vertex_array(&mut native, &array);
    native.begin_render_pass(&pass(size)).unwrap();
    assert!(GpuBackend::bind_pipeline(
        &mut native,
        &program,
        &RenderState::default()
    ));
    let first_transform = Transform3D::ortho(0., 6., 0., 2., -1., 1.);
    GpuBackend::set_uniforms(&native, &program, &first_transform);
    let mut data = vec![0f32, 0., 2., 2., 1., 0., 0., 1.];
    let other = GpuBackend::create_buffer(&mut native, BufferKind::Vertex);
    for (buffer, bytes) in [
        (&other, crate::device::as_bytes(&data)),
        (&instances, &crate::device::as_bytes(&data)[..31]),
    ] {
        assert!(native
            .draw(
                0,
                6,
                1,
                0,
                Some(wgt::IndexFormat::Uint16),
                wgt::PrimitiveTopology::TriangleList,
                Some((buffer, bytes)),
            )
            .is_err());
    }
    assert!(native.pass.as_ref().unwrap().pending.is_empty());
    GpuBackend::delete_buffer(&mut native, other);
    GpuBackend::draw_instanced_batch(
        &mut native,
        &mut instances,
        crate::device::as_bytes(&data),
        1,
        VertexUsageHint::Stream,
    );
    let first_projection = native
        .programs
        .state(&program)
        .unwrap()
        .projection
        .borrow()
        .clone()
        .unwrap();
    assert_eq!(
        first_projection.usage(),
        wgt::BufferUsages::UNIFORM | mapping
    );
    assert!(first_projection.get_mapped_range(0, Some(64)).is_err());
    data.copy_from_slice(&[2., 0., 4., 2., 0., 1., 0., 1.]);
    GpuBackend::set_uniforms(&native, &program, &first_transform);
    GpuBackend::draw_instanced_batch(
        &mut native,
        &mut instances,
        crate::device::as_bytes(&data),
        1,
        VertexUsageHint::Stream,
    );
    let repeated_projection = native
        .programs
        .state(&program)
        .unwrap()
        .projection
        .borrow()
        .clone()
        .unwrap();
    assert!(Arc::ptr_eq(&first_projection, &repeated_projection));
    GpuBackend::set_uniforms(
        &native,
        &program,
        &Transform3D::ortho(-4., 2., 0., 2., -1., 1.),
    );
    assert!(native
        .programs
        .state(&program)
        .unwrap()
        .projection
        .borrow()
        .is_none());
    data.copy_from_slice(&[0., 0., 2., 2., 1., 1., 1., 1.]);
    GpuBackend::draw_instanced_batch(
        &mut native,
        &mut instances,
        crate::device::as_bytes(&data),
        1,
        VertexUsageHint::Stream,
    );
    let changed_projection = native
        .programs
        .state(&program)
        .unwrap()
        .projection
        .borrow()
        .clone()
        .unwrap();
    assert!(!Arc::ptr_eq(&first_projection, &changed_projection));
    assert_eq!(
        changed_projection.usage(),
        wgt::BufferUsages::UNIFORM | mapping
    );
    assert!(changed_projection.get_mapped_range(0, Some(64)).is_err());
    data.fill(f32::NAN);
    drop(data);
    drop(first_projection);
    drop(repeated_projection);
    drop(changed_projection);
    assert_eq!(native.vertex_arrays.buffers[&instances.id].bytes, [13; 64]);
    assert_eq!(instances.size, 64);
    let draws = &native.pass.as_ref().unwrap().pending;
    assert_eq!(draws.len(), 3);
    assert!(Arc::ptr_eq(&draws[0].bindings, &draws[1].bindings));
    assert!(!Arc::ptr_eq(&draws[1].bindings, &draws[2].bindings));
    for draw in draws {
        let buffer = draw.instances.as_ref().unwrap();
        assert_eq!(buffer.size(), 32);
        assert_eq!(buffer.usage(), wgt::BufferUsages::VERTEX | mapping);
        assert!(buffer.get_mapped_range(0, Some(32)).is_err());
    }
    assert!(!Arc::ptr_eq(
        draws[0].instances.as_ref().unwrap(),
        draws[1].instances.as_ref().unwrap()
    ));
    assert!(!Arc::ptr_eq(
        draws[1].instances.as_ref().unwrap(),
        draws[2].instances.as_ref().unwrap()
    ));
    assert_eq!(native.submitted, 0);
    GpuBackend::delete_program(&mut native, program);
    GpuBackend::delete_vertex_array(&mut native, array);
    GpuBackend::delete_buffer(&mut native, vertices);
    GpuBackend::delete_buffer(&mut native, instances);
    native.end_render_pass(StoreOp::Store).unwrap();
    native.end_frame().unwrap();
    let mut device = wr::Device {
        backend: Box::new(native),
        pending_state: RenderState::default(),
        #[cfg(debug_assertions)]
        pipeline_bound: false,
    };
    assert_eq!(
        pixels(&mut device, size),
        [
            255u8, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 255, 255, 255, 255,
            255, 255, 255, 255,
        ]
        .repeat(2)
    );
    device.deinit();
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_streams_immutable_instances_and_reuses_projections() {
    check_streamed_instances(None);
}

#[test]
#[ignore = "Requires Vulkan and supported primary mapping with the Khronos validation layer"]
fn native_backend_streams_primary_mapped_instances() {
    check_streamed_instances(Some(true));
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_streams_instances_with_staging_fallback() {
    check_streamed_instances(Some(false));
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_orders_texture_uploads_between_draws() {
    let mut device = backend();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let size = DeviceIntSize::new(4, 2);
    device.begin_frame();
    let source = device.create_texture(
        ImageBufferKind::Texture2D,
        ImageFormat::RGBA8,
        2,
        2,
        TextureFilter::Nearest,
        None,
    );
    device.upload_texture_immediate(&source, &[255u8, 0, 0, 255].repeat(4));
    let program = device
        .create_program_linked(
            "cs_scale",
            &["TEXTURE_2D"],
            &desc::SCALE,
            &[("sColor0", TextureSlot(0))],
        )
        .unwrap();
    let vertices = device.create_buffer(BufferKind::Vertex);
    let mut instances = device.create_buffer(BufferKind::Vertex);
    let array = device.create_vertex_array(&desc::SCALE, &vertices, Some(&instances), None, 1);
    device.write_buffer(
        &mut instances,
        &[0f32, 0., 4., 2., 0., 0., 1., 1., 0.],
        VertexUsageHint::Stream,
    );
    device.bind_vertex_array(&array);
    device.begin_render_pass(&pass(size));
    device.bind_texture(TextureSlot(0), &source, Swizzle::default());
    device.bind_program(&program);
    device.set_uniforms(&program, &Transform3D::ortho(0., 4., 0., 2., -1., 1.));
    device.set_scissor(Some(
        DeviceIntRect::from_size(DeviceIntSize::new(2, 2)).cast_unit(),
    ));
    device.draw_indexed_triangles_instanced_u16(6, 1);
    device.upload_texture_immediate(&source, &[0u8, 255, 0, 255].repeat(4));
    device.set_scissor(Some(
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(2, 0), DeviceIntSize::new(2, 2))
            .cast_unit(),
    ));
    device.draw_indexed_triangles_instanced_u16(6, 1);
    device.end_render_pass(StoreOp::Store);
    device.end_frame();
    assert_eq!(
        pixels(&mut device, size),
        [255u8, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255].repeat(2)
    );
    device.delete_program(program);
    device.delete_vertex_array(array);
    device.delete_buffer(vertices);
    device.delete_buffer(instances);
    device.delete_texture(source);
    device.deinit();
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_caches_only_identical_draw_bindings_across_frames() {
    let owner = Arc::new(device());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let mut native = RenderDevice::new(owner.clone()).unwrap();
    let size = DeviceIntSize::new(8, 1);
    let mut program = GpuBackend::create_program(&mut native, "cs_scale", &["TEXTURE_2D"]).unwrap();
    GpuBackend::link_program(
        &mut native,
        &mut program,
        &desc::SCALE,
        &[("sColor0", TextureSlot(0))],
    )
    .unwrap();
    let vertices = GpuBackend::create_buffer(&mut native, BufferKind::Vertex);
    let mut instances = GpuBackend::create_buffer(&mut native, BufferKind::Vertex);
    let array = GpuBackend::create_vertex_array(
        &mut native,
        &desc::SCALE,
        &vertices,
        Some(&instances),
        None,
        1,
    );
    let source = super::super::images::Image::new(
        &owner,
        2,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let encoder = native.commands().unwrap();
    owner
        .upload_texture(
            &encoder,
            &source.raw,
            0,
            bounds(&source),
            &[255, 0, 0, 255, 0, 255, 0, 255],
            None,
            0,
            None,
        )
        .unwrap();
    let transform = Transform3D::ortho(0., 8., 0., 1., -1., 1.);
    let mut readbacks = Vec::new();
    let mut previous = None;
    for frame in 0..3 {
        native.begin_frame().unwrap();
        GpuBackend::bind_vertex_array(&mut native, &array);
        native.begin_render_pass(&pass(size)).unwrap();
        assert!(GpuBackend::bind_pipeline(
            &mut native,
            &program,
            &RenderState {
                blend_mode: if frame % 2 == 0 {
                    BlendMode::None
                } else {
                    BlendMode::PremultipliedAlpha
                },
                ..RenderState::default()
            }
        ));
        GpuBackend::set_uniforms(&native, &program, &transform);
        for x in 0..8 {
            let replacement = if x == 3 {
                let image = super::super::images::Image::new(
                    &owner,
                    2,
                    1,
                    wgt::TextureFormat::Rgba8Unorm,
                    TextureFilter::Nearest,
                    false,
                )
                .unwrap();
                native.flush_pass(StoreOp::Store).unwrap();
                let encoder = native.commands().unwrap();
                owner
                    .upload_texture(
                        &encoder,
                        &image.raw,
                        0,
                        bounds(&image),
                        &[0, 0, 255, 255].repeat(2),
                        None,
                        0,
                        None,
                    )
                    .unwrap();
                Some(image)
            } else {
                None
            };
            native
                .textures
                .bind_external_image(
                    TextureSlot(0),
                    replacement.unwrap_or_else(|| source.clone()),
                    if x == 2 {
                        api::ImageRendering::Auto
                    } else {
                        api::ImageRendering::Pixelated
                    },
                )
                .unwrap();
            if x == 5 {
                GpuBackend::set_uniforms(
                    &native,
                    &program,
                    &Transform3D::ortho(-1., 7., 0., 1., -1., 1.),
                );
            } else if x == 6 {
                GpuBackend::set_uniforms(&native, &program, &transform);
            } else if x == 7 {
                assert!(GpuBackend::bind_pipeline(
                    &mut native,
                    &program,
                    &RenderState {
                        blend_mode: if frame % 2 == 0 {
                            BlendMode::PremultipliedAlpha
                        } else {
                            BlendMode::None
                        },
                        ..RenderState::default()
                    }
                ));
            }
            let left = if x == 5 { 4. } else { x as f32 };
            GpuBackend::draw_instanced_batch(
                &mut native,
                &mut instances,
                crate::device::as_bytes(&[left, 0., left + 1., 1., 0.375f32, 0.5, 0.375, 0.5, 0.]),
                1,
                VertexUsageHint::Stream,
            );
            let current = native
                .pass
                .as_ref()
                .unwrap()
                .pending
                .last()
                .unwrap()
                .bindings
                .clone();
            if let Some(old) = previous.replace(current.clone()) {
                assert_eq!(
                    Arc::ptr_eq(&old, &current),
                    x == 1 || (x == 0 && frame > 0),
                    "frame {frame}, draw {x}"
                );
            }
        }
        native.end_render_pass(StoreOp::Store).unwrap();
        let encoder = native.commands().unwrap();
        let readback = owner
            .encode_readback(
                &encoder,
                &native.textures.output().unwrap().raw,
                0,
                DeviceIntRect::from_size(size),
                Some(ImageFormat::RGBA8),
            )
            .unwrap();
        let index = native.end_frame().unwrap();
        readbacks.push(readback.map(&owner, index).unwrap());
    }
    for mut readback in readbacks {
        let actual = readback.wait(&owner).unwrap();
        let expected = [
            255u8, 0, 0, 255, 255, 0, 0, 255, 191, 64, 0, 255, 0, 0, 255, 255, 255, 0, 0, 255, 255,
            0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ];
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (*actual as i16 - expected as i16).abs() <= 1,
                "{:?} versus {:?}",
                actual,
                expected
            );
        }
    }
    native.begin_frame().unwrap();
    GpuBackend::bind_vertex_array(&mut native, &array);
    native.begin_render_pass(&pass(size)).unwrap();
    assert!(GpuBackend::bind_pipeline(
        &mut native,
        &program,
        &RenderState::default()
    ));
    native
        .textures
        .bind_external_image(TextureSlot(0), source, api::ImageRendering::Pixelated)
        .unwrap();
    GpuBackend::draw_instanced_batch(
        &mut native,
        &mut instances,
        crate::device::as_bytes(&[0f32, 0., 8., 1., 0., 0., 1., 1., 0.]),
        1,
        VertexUsageHint::Stream,
    );
    let retained = Arc::downgrade(&native.last_bindings.as_ref().unwrap().raw);
    drop(previous);
    assert!(native
        .record_result::<()>(Err("aborted frame".into()))
        .is_none());
    assert!(native.last_bindings.is_none());
    assert!(native.pass.is_none());
    assert!(!native.inside_frame);
    owner.wait_for(native.submitted).unwrap();
    owner.poll().unwrap();
    assert!(retained.upgrade().is_none());
    GpuBackend::delete_program(&mut native, program);
    GpuBackend::delete_vertex_array(&mut native, array);
    GpuBackend::delete_buffer(&mut native, vertices);
    GpuBackend::delete_buffer(&mut native, instances);
    GpuBackend::deinit(&mut native);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_partial_clear_preserves_atlas_and_writes_requested_depth() {
    let owner = Arc::new(device());
    let mut native = RenderDevice::new(owner.clone()).unwrap();
    native.begin_frame().unwrap();
    let mut texture = native
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(4, 2),
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: true }),
        )
        .unwrap();
    let (color, depth) = native
        .textures
        .render_target(texture.target_id, true)
        .unwrap();
    native
        .begin_render_pass(&RenderPassDescriptor {
            target: DrawTarget::from_texture(&texture, true),
            render_area: None,
            color_load: LoadOp::Clear([0., 0., 1., 1.]),
            depth_load: LoadOp::Clear(0.75),
        })
        .unwrap();
    native.end_render_pass(StoreOp::Store).unwrap();
    native
        .begin_render_pass(&RenderPassDescriptor {
            target: DrawTarget::from_texture(&texture, true),
            render_area: Some(DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(1, 0),
                DeviceIntSize::new(2, 1),
            )),
            color_load: LoadOp::Clear([1., 0., 0., 1.]),
            depth_load: LoadOp::Clear(0.25),
        })
        .unwrap();
    native.end_render_pass(StoreOp::Store).unwrap();
    let mut pixels = vec![0; 32];
    native
        .capture_into(
            color,
            DeviceIntRect::from_size(DeviceIntSize::new(4, 2)),
            ImageFormat::RGBA8,
            &mut pixels,
        )
        .unwrap();
    assert_eq!(
        pixels,
        [
            vec![0, 0, 255, 255, 255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255],
            vec![0, 0, 255, 255].repeat(4)
        ]
        .concat()
    );
    let output = owner
        .create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 512,
            usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
        .unwrap();
    let encoder = owner
        .create_command_encoder("depth clear readback")
        .unwrap();
    encoder.copy_texture_to_buffer(
        &wgt::TexelCopyTextureInfo {
            texture: depth.unwrap().raw.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::DepthOnly,
        },
        &wgt::TexelCopyBufferInfo {
            buffer: output.clone(),
            layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: None,
            },
        },
        &wgt::Extent3d {
            width: 4,
            height: 2,
            depth_or_array_layers: 1,
        },
    );
    owner
        .submit(&[super::super::tests::finish(&encoder)])
        .unwrap();
    let bytes = super::super::tests::read(&owner, &output, 512);
    let values: Vec<_> = [0usize, 4, 8, 12, 256, 260, 264, 268]
        .iter()
        .map(|&i| f32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap()))
        .collect();
    assert_eq!(values, [0.75, 0.25, 0.25, 0.75, 0.75, 0.75, 0.75, 0.75]);
    native.textures.delete(&mut texture).unwrap();
    native.end_frame().unwrap();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_generates_mips_and_uses_scratch_for_attachment_feedback() {
    let owner = Arc::new(device());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let mut native = RenderDevice::new(owner.clone()).unwrap();
    native.begin_frame().unwrap();
    let mut texture = native
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(4, 4),
            TextureFilter::Trilinear,
            Some(RenderTargetInfo { has_depth: false }),
        )
        .unwrap();
    let colors = [
        [255u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 255],
    ];
    let bytes: Vec<_> = (0..4)
        .flat_map(|y| (0..4).flat_map(move |x| colors[(y / 2) * 2 + x / 2]))
        .collect();
    native.upload_texture_immediate(&texture, &bytes).unwrap();
    let image = native.textures.image(&texture).unwrap();
    let read = |native: &mut RenderDevice, level, width| {
        let encoder = native.commands().unwrap();
        let readback = owner
            .encode_readback(
                &encoder,
                &image.raw,
                level,
                DeviceIntRect::from_size(DeviceIntSize::new(width, width)),
                None,
            )
            .unwrap();
        let index = native.submit().unwrap();
        readback.map(&owner, index).unwrap().wait(&owner).unwrap()
    };
    assert_eq!(read(&mut native, 1, 2), colors.concat());
    let average = read(&mut native, 2, 1);
    for (&actual, expected) in average.iter().zip([128u8, 128, 128, 255]) {
        assert!(actual.abs_diff(expected) <= 1);
    }
    native
        .blit_render_target(
            ReadTarget::Texture {
                texture: texture.target_id,
            },
            DeviceIntRect::from_size(DeviceIntSize::new(2, 2)).cast_unit(),
            DrawTarget::from_texture(&texture, false),
            DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(2, 2),
                DeviceIntSize::new(2, 2),
            )
            .cast_unit(),
            TextureFilter::Nearest,
        )
        .unwrap();
    let pixels = read(&mut native, 0, 4);
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(
                &pixels[(y * 4 + x) * 4..][..4],
                if x >= 2 && y >= 2 {
                    &colors[0]
                } else {
                    &colors[(y / 2) * 2 + x / 2]
                }
            );
        }
    }
    assert_eq!(read(&mut native, 2, 1), average);
    native.textures.delete(&mut texture).unwrap();
    native.end_frame().unwrap();
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_uploads_bgra_directly_before_producer_bytes_are_released() {
    let mut device = backend();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    assert!(matches!(device.upload_method(), UploadMethod::Immediate));
    assert!(!device.get_capabilities().supports_persistent_upload_buffers);
    let size = DeviceIntSize::new(3, 2);
    let texture = device.create_texture(
        ImageBufferKind::Texture2D,
        ImageFormat::BGRA8,
        size.width,
        size.height,
        TextureFilter::Nearest,
        None,
    );
    let mut pool = wr::UploadBufferPool::new(&mut device, 1024);
    for epoch in 0..3u8 {
        device.begin_frame();
        let mut uploader = wr::TextureUploader::new(&mut device, &mut pool);
        let mut source = vec![0xCC; 28];
        let mut expected = Vec::new();
        for y in 0..2 {
            for x in 0..3 {
                let pixel = [11 + x as u8, 29 + y as u8, 71 + epoch, 255];
                source[y * 16 + x * 4..y * 16 + x * 4 + 4].copy_from_slice(&pixel);
                expected.extend_from_slice(&pixel);
            }
        }
        assert_eq!(
            uploader.upload(
                &mut device,
                &texture,
                DeviceIntRect::from_size(size),
                Some(16),
                None,
                source.as_ptr(),
                source.len(),
            ),
            24
        );
        source.fill(0xDD);
        let mut patch = [47u8, 53, 97 + epoch, 255];
        let area = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(1, 1),
            DeviceIntSize::new(1, 1),
        );
        uploader.upload(
            &mut device,
            &texture,
            area,
            None,
            None,
            patch.as_ptr(),
            patch.len(),
        );
        expected[16..20].copy_from_slice(&patch);
        patch.fill(0xDD);
        uploader.flush(&mut device);
        pool.end_frame(&mut device);
        device.end_frame();
        let mut actual = vec![0; expected.len()];
        device.read_texture(&texture, ImageFormat::BGRA8, &mut actual);
        assert_eq!(actual, expected, "epoch {epoch}");
        assert!(device.failure().is_none(), "{:?}", device.failure());
        assert_eq!(pool.report_memory().texture_upload_pbos, 0);
    }
    pool.deinit(&mut device);
    device.delete_texture(texture);
    device.deinit();
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_persistent_cpu_uploads_copy_at_flush_and_transfer_views_remain_valid() {
    use crate::device::GpuBackend;
    let owner = Arc::new(device());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let mut native = RenderDevice::new(owner).unwrap();
    native.begin_frame().unwrap();
    let mut first = native
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(2, 1),
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: false }),
        )
        .unwrap();
    let mut second = native
        .textures
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(2, 1),
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: false }),
        )
        .unwrap();
    let mut buffer = GpuBackend::create_transfer_buffer(&mut native);
    let mapping = GpuBackend::allocate_upload_buffer(
        &mut native,
        &mut buffer,
        64,
        VertexUsageHint::Stream,
        true,
    )
    .unwrap();
    let UploadBufferMapping::Persistent(ptr) = mapping else {
        panic!("Expected persistent CPU upload storage")
    };
    let rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 1));
    let red = [255u8, 0, 0, 255].repeat(2);
    let green = [0u8, 255, 0, 255].repeat(2);
    unsafe {
        std::ptr::copy_nonoverlapping(red.as_ptr(), ptr.as_ptr().cast(), 8);
    }
    GpuBackend::flush_upload_buffer(
        &mut native,
        &buffer,
        &mapping,
        8,
        &[UploadChunk {
            rect,
            stride: None,
            offset: 0,
            format_override: None,
            texture: &first,
        }],
    );
    unsafe {
        std::ptr::copy_nonoverlapping(green.as_ptr(), ptr.as_ptr().cast(), 8);
    }
    GpuBackend::flush_upload_buffer(
        &mut native,
        &buffer,
        &mapping,
        8,
        &[UploadChunk {
            rect,
            stride: None,
            offset: 0,
            format_override: None,
            texture: &second,
        }],
    );
    unsafe {
        std::ptr::write_bytes(ptr.as_ptr().cast::<u8>(), 0, 64);
    }
    native.end_frame().unwrap();
    for (texture, expected) in [(&first, red), (&second, green)] {
        let readback = GpuBackend::create_transfer_buffer_with_size(&mut native, 16);
        GpuBackend::read_pixels_into_transfer_buffer(
            &mut native,
            ReadTarget::Texture {
                texture: texture.target_id,
            },
            rect,
            ImageFormat::RGBA8,
            &readback,
        );
        {
            let mapped = GpuBackend::map_transfer_buffer(&mut native, &readback).unwrap();
            assert_eq!(&mapped.data[..8], expected);
            assert_eq!(&mapped.data[8..], &[0; 8]);
        }
        GpuBackend::delete_transfer_buffer(&mut native, readback);
    }
    GpuBackend::delete_transfer_buffer(&mut native, buffer);
    native.textures.delete(&mut first).unwrap();
    native.textures.delete(&mut second).unwrap();
    GpuBackend::deinit(&mut native);
    assert!(native.failure().is_none());
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_backend_failure_discards_deferred_passes_and_unsubmitted_work() {
    use crate::device::GpuBackend;
    let owner = Arc::new(device());
    let mut native = RenderDevice::new(owner.clone()).unwrap();
    native.begin_frame().unwrap();
    native
        .begin_render_pass(&pass(DeviceIntSize::new(4, 2)))
        .unwrap();
    native.clear_target(Some([1.; 4]), None, None).unwrap();
    owner.device.destroy();
    let texture = GpuBackend::create_texture(
        &mut native,
        ImageBufferKind::Texture2D,
        ImageFormat::RGBA8,
        2,
        1,
        TextureFilter::Nearest,
        None,
    );
    assert!(native.failure().is_some());
    assert!(native.pass.is_none());
    assert!(native.encoder.is_none());
    assert_eq!(native.submitted, 0);
    GpuBackend::delete_texture(&mut native, texture);
    let mut wrapper = wr::Device {
        backend: Box::new(native),
        pending_state: RenderState::default(),
        #[cfg(debug_assertions)]
        pipeline_bound: false,
    };
    wrapper.set_color_write(false);
    wrapper.draw_indexed_triangles_instanced_u16(6, 1);
    assert!(wrapper.failure().is_some());
    wrapper.deinit();
}

#[test]
#[ignore = "Requires Vulkan and Khronos validation"]
fn native_backend_completion_publication_survives_concurrent_core_polling() {
    use std::sync::atomic::AtomicBool;
    let owner = Arc::new(device());
    let mut backend = RenderDevice::new(owner.clone()).unwrap();
    let stop = AtomicBool::new(false);
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Acquire) {
                owner.poll().unwrap();
                std::thread::yield_now();
            }
        });
        let mut last = 0;
        for _ in 0..128 {
            backend.begin_frame().unwrap();
            last = backend.end_frame().unwrap();
        }
        owner.wait_for(last).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.completed.load(Ordering::Acquire) < last
            && std::time::Instant::now() < deadline
        {
            owner.poll().unwrap();
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        assert!(backend.completed.load(Ordering::Acquire) >= last);
    });
    backend.deinit();
    assert!(backend.failure().is_none());
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[cfg(all(target_os = "linux", feature = "debugger"))]
#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_surface_capture_requests_are_consumed_without_output() {
    use super::super::{surface_testing, Options, X11Window};
    use crate::device::GpuBackend;

    super::super::tests::validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let mut native = RenderDevice::new_windowed(&Options {
        window: Some(window),
        validation: true,
        ..Default::default()
    })
    .unwrap();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let draw = |native: &mut RenderDevice| {
        native.begin_frame().unwrap();
        native
            .begin_render_pass(&pass(DeviceIntSize::new(64, 48)))
            .unwrap();
        native.end_render_pass(StoreOp::Store).unwrap();
        native.end_frame().unwrap();
    };

    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    draw(&mut native);
    let snapshot = native.textures.output().unwrap();
    native.begin_frame().unwrap();
    native.end_frame().unwrap();
    assert!(Arc::ptr_eq(&snapshot, &native.textures.output().unwrap()));
    let mut pixels = [0; 12];
    native
        .capture_into(
            snapshot,
            DeviceIntRect::from_origin_and_size(
                api::units::DeviceIntPoint::new(1, 1),
                DeviceIntSize::new(3, 1),
            ),
            ImageFormat::BGRA8,
            &mut pixels,
        )
        .unwrap();
    assert_eq!(pixels, [255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255]);

    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    native.begin_frame().unwrap();
    native.end_frame().unwrap();
    assert!(!GpuBackend::readback_ready(&native));
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    draw(&mut native);
    assert!(GpuBackend::readback_ready(&native));
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));

    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    surface_testing::ACQUIRE.with(|s| s.set(Some(wgt::SurfaceStatus::Timeout)));
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    draw(&mut native);
    assert!(GpuBackend::readback_ready(&native));

    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    GpuBackend::set_surface_paused(&mut native, true).unwrap();
    GpuBackend::set_surface_paused(&mut native, false).unwrap();
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));
    GpuBackend::set_surface_paused(&mut native, true).unwrap();
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));
    GpuBackend::set_surface_paused(&mut native, false).unwrap();
    draw(&mut native);
    assert!(!GpuBackend::readback_ready(&native));
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    draw(&mut native);
    assert!(GpuBackend::readback_ready(&native));
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    native.begin_frame().unwrap();
    assert!(GpuBackend::prepare_readback(&mut native).unwrap());
    native.owner.device.destroy();
    assert!(native.operation(RenderDevice::end_frame).is_none());
    assert!(!native.capture_pending);
    assert!(!native.capture_this_frame);
    assert!(!GpuBackend::readback_ready(&native));
    GpuBackend::deinit(&mut native);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
