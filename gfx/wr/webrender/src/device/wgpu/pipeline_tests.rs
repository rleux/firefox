/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{
    pipeline::DrawPipeline, samplers::Samplers, shader::select_draw_shader, shaders,
};
use super::*;
use crate::device::{BlendMode, RenderState, TextureFilter};
use webrender_build::vulkan::ScalarType;

fn inputs(
    owner: &Device,
    pipeline: &DrawPipeline,
) -> (
    Option<Arc<core::resource::Buffer>>,
    Vec<(Arc<core::resource::TextureView>, TextureFilter)>,
) {
    let projection = (pipeline.shader.projection_stages != 0).then(|| {
        owner
            .create_buffer_with_data("projection snapshot", &[0; 64], wgt::BufferUsages::UNIFORM)
            .unwrap()
    });
    let textures = pipeline
        .shader
        .textures
        .iter()
        .map(|binding| {
            let format = match binding.scalar {
                ScalarType::Float if binding.name.starts_with("sColor") => {
                    wgt::TextureFormat::Rgba8Unorm
                }
                ScalarType::Float => wgt::TextureFormat::Rgba32Float,
                ScalarType::Sint => wgt::TextureFormat::Rgba32Sint,
                ScalarType::Uint => wgt::TextureFormat::Rgba32Uint,
            };
            let texture = owner
                .create_texture(&core::resource::TextureDescriptor {
                    label: Some(binding.name.into()),
                    size: wgt::Extent3d {
                        width: 4,
                        height: 4,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgt::TextureDimension::D2,
                    format,
                    usage: wgt::TextureUsages::TEXTURE_BINDING,
                    view_formats: vec![],
                })
                .unwrap();
            let view = texture.create_view(&Default::default());
            owner.check().unwrap();
            (view, TextureFilter::Linear)
        })
        .collect();
    (projection, textures)
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_catalog_pipelines_and_bindings_validate_actual_wr_interfaces() {
    let owner = Arc::new(device());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let samplers = Samplers::new(&owner).unwrap();
    assert!(owner
        .features()
        .contains(wgt::Features::PASSTHROUGH_SHADERS));
    let mut created = 0;
    let mut unsupported = 0;
    for shader in shaders::SHADERS {
        let features: Vec<_> = shader
            .features
            .split(',')
            .filter(|s| !s.is_empty())
            .collect();
        assert!(std::ptr::eq(
            shader,
            select_draw_shader(shader.name, &features, false).unwrap()
        ));
        let dual = features.contains(&"DUAL_SOURCE_BLENDING");
        let state = RenderState {
            blend_mode: if dual && !features.contains(&"DEBUG_OVERDRAW") {
                BlendMode::SubpixelDualSource
            } else {
                BlendMode::None
            },
            ..Default::default()
        };
        let format = if shader.name == "ps_quad_mask" || features.contains(&"ALPHA_TARGET") {
            wgt::TextureFormat::R8Unorm
        } else {
            wgt::TextureFormat::Bgra8Unorm
        };
        if dual
            && !owner
                .features()
                .contains(wgt::Features::DUAL_SOURCE_BLENDING)
        {
            let failure = DrawPipeline::new(
                &owner,
                shader,
                format,
                false,
                state,
                wgt::PrimitiveTopology::TriangleList,
            )
            .err()
            .unwrap();
            assert_eq!(failure.kind, ErrorType::Validation);
            assert!(failure.message.contains("dual-source"));
            unsupported += 1;
            continue;
        }
        for has_depth in [false, true] {
            let pipeline = DrawPipeline::new(
                &owner,
                shader,
                format,
                has_depth,
                state,
                wgt::PrimitiveTopology::TriangleList,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "{} [{}], depth={has_depth}: {e}",
                    shader.name, shader.features
                )
            });
            let (projection, textures) = inputs(&owner, &pipeline);
            pipeline
                .create_bindings(projection, &textures, &[], &samplers)
                .unwrap();
            assert_eq!(
                pipeline.instance_stride == 0,
                shader.name.starts_with("debug_")
            );
            assert!(Arc::ptr_eq(&pipeline.owner, &owner));
            assert_eq!((pipeline.format, pipeline.has_depth), (format, has_depth));
            eprintln!(
                "Created {} [{}], depth={has_depth}",
                shader.name, shader.features
            );
            created += 1;
        }
    }
    assert_eq!(created / 2 + unsupported, shaders::SHADERS.len());
    eprintln!("Created {created} catalog pipelines; {unsupported} variants rejected for unavailable features");
    owner.check().unwrap();
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_pipeline_variants_reuse_shader_modules_and_core_retains_bindings() {
    let owner = Arc::new(device());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let shader = select_draw_shader("ps_clear", &[], false).unwrap();
    let first = DrawPipeline::new(
        &owner,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        Default::default(),
        wgt::PrimitiveTopology::TriangleList,
    )
    .unwrap();
    let second = DrawPipeline::new(
        &owner,
        shader,
        wgt::TextureFormat::Bgra8Unorm,
        true,
        RenderState {
            blend_mode: BlendMode::PremultipliedAlpha,
            ..Default::default()
        },
        wgt::PrimitiveTopology::TriangleList,
    )
    .unwrap();
    assert!(Arc::ptr_eq(&first.prepared, &second.prepared));
    let samplers = Samplers::new(&owner).unwrap();
    let (projection, textures) = inputs(&owner, &first);
    let weak = Arc::downgrade(projection.as_ref().unwrap());
    let group = first
        .create_bindings(projection, &textures, &[], &samplers)
        .unwrap();
    assert!(weak.upgrade().is_some());
    let index = owner.submit(&[]).unwrap();
    owner.wait_for(index).unwrap();
    let prepared = Arc::downgrade(&first.prepared);
    drop(first);
    drop(second);
    assert!(prepared.upgrade().is_none());
    assert!(weak.upgrade().is_some());
    drop(group);
    idle(&owner);
    assert!(weak.upgrade().is_none());
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_pipeline_and_binding_errors_are_reported_at_creation() {
    let owner = Arc::new(device());
    let shader = select_draw_shader("ps_clear", &[], false).unwrap();
    for mode in [BlendMode::SubpixelDualSource] {
        assert_eq!(
            DrawPipeline::new(
                &owner,
                shader,
                wgt::TextureFormat::Rgba8Unorm,
                false,
                RenderState {
                    blend_mode: mode,
                    ..Default::default()
                },
                wgt::PrimitiveTopology::TriangleList,
            )
            .err()
            .unwrap()
            .kind,
            ErrorType::Validation
        );
    }
    assert!(DrawPipeline::new(
        &owner,
        shader,
        wgt::TextureFormat::Depth32Float,
        false,
        Default::default(),
        wgt::PrimitiveTopology::TriangleList,
    )
    .is_err());
    let pipeline = DrawPipeline::new(
        &owner,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        Default::default(),
        wgt::PrimitiveTopology::TriangleList,
    )
    .unwrap();
    let samplers = Samplers::new(&owner).unwrap();
    assert!(pipeline.create_bindings(None, &[], &[], &samplers).is_err());
    let undersized = owner
        .create_buffer_with_data("undersized projection", &[0; 4], wgt::BufferUsages::UNIFORM)
        .unwrap();
    let failure = pipeline
        .create_bindings(Some(undersized), &[], &[], &samplers)
        .unwrap_err();
    assert_eq!(failure.kind, ErrorType::Validation);
    assert_eq!(owner.failure(), Some(failure));
    assert!(owner.device.is_valid());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_bindings_validate_sample_types_and_device_ownership() {
    for foreign in [false, true] {
        let owner = Arc::new(device());
        let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
        let pipeline = DrawPipeline::new(
            &owner,
            shader,
            wgt::TextureFormat::Rgba8Unorm,
            false,
            Default::default(),
            wgt::PrimitiveTopology::TriangleList,
        )
        .unwrap();
        let samplers = Samplers::new(&owner).unwrap();
        let (projection, _) = inputs(&owner, &pipeline);
        let other = device();
        let texture_owner = if foreign { &other } else { &owner };
        let mut desc = texture_desc();
        desc.format = if foreign {
            wgt::TextureFormat::Rgba8Unorm
        } else {
            wgt::TextureFormat::Rgba32Uint
        };
        let texture = texture_owner.create_texture(&desc).unwrap();
        let view = texture.create_view(&Default::default());
        let failure = pipeline
            .create_bindings(projection, &[(view, TextureFilter::Linear)], &[], &samplers)
            .unwrap_err();
        assert_eq!(failure.kind, ErrorType::Validation);
        assert!(owner.device.is_valid());
    }
}

fn draw_pixels(
    owner: &Arc<Device>,
    name: &str,
    topology: wgt::PrimitiveTopology,
    blend_mode: BlendMode,
    vertices: &[u8],
    instances: Option<&[u8]>,
    indices: Option<(&[u8], wgt::IndexFormat)>,
    count: u32,
) -> Vec<u8> {
    let shader = select_draw_shader(name, &[], false).unwrap();
    let pipeline = DrawPipeline::new(
        owner,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState {
            blend_mode,
            ..Default::default()
        },
        topology,
    )
    .unwrap();
    assert_eq!(pipeline.topology, topology);
    let samplers = Samplers::new(owner).unwrap();
    let matrix = [
        0.25f32, 0., 0., 0., 0., -0.25, 0., 0., 0., 0., 1., 0., -1., 1., 0., 1.,
    ];
    let projection: Vec<_> = matrix
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let projection = owner
        .create_buffer_with_data("draw projection", &projection, wgt::BufferUsages::UNIFORM)
        .unwrap();
    let bindings = pipeline
        .create_bindings(Some(projection), &[], &[], &samplers)
        .unwrap();
    let vertex = owner
        .create_buffer_with_data("draw vertices", vertices, wgt::BufferUsages::VERTEX)
        .unwrap();
    let instance = instances.map(|data| {
        owner
            .create_buffer_with_data("draw instances", data, wgt::BufferUsages::VERTEX)
            .unwrap()
    });
    let index = indices.map(|(data, format)| {
        (
            owner
                .create_buffer_with_data("draw indices", data, wgt::BufferUsages::INDEX)
                .unwrap(),
            format,
        )
    });
    let mut desc = texture_desc();
    desc.size.width = 8;
    desc.size.height = 8;
    desc.usage = wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC;
    let target = owner.create_texture(&desc).unwrap();
    let view = target.create_view(&Default::default());
    let encoder = owner
        .create_command_encoder("topology and overdraw pixel test")
        .unwrap();
    let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
        color_attachments: std::borrow::Cow::Owned(vec![Some(
            core::command::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Clear(wgt::Color::BLUE),
                store_op: wgt::StoreOp::Store,
            },
        )]),
        ..Default::default()
    });
    pass.set_pipeline(pipeline.raw.clone());
    pass.set_bind_group(0, Some(bindings), &[0]);
    pass.set_vertex_buffer(0, Some(vertex), 0, None);
    if let Some(instance) = instance {
        pass.set_vertex_buffer(1, Some(instance), 0, None);
    }
    if let Some((index, format)) = index {
        pass.set_index_buffer(index, format, 0, None);
        pass.draw_indexed(count, 1, 0, 0, 0);
    } else {
        pass.draw(count, 1, 0, 0);
    }
    pass.end();
    let readback = owner
        .encode_readback(
            &encoder,
            &target,
            0,
            api::units::DeviceIntRect::from_size(api::units::DeviceIntSize::new(8, 8)),
            None,
        )
        .unwrap();
    let index = owner.submit(&[finish(&encoder)]).unwrap();
    readback.map(owner, index).unwrap().wait(owner).unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_indexed_quads_debug_triangles_lines_and_overdraw_match_wr_draws() {
    let owner = Arc::new(device());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let quad = [0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0];
    let instance: Vec<_> = [0f32, 0., 8., 8., 0.5, 0., 0., 0.5]
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let quad_indices: Vec<_> = [0u16, 1, 2, 2, 1, 3]
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let debug_vertex = |x: f32, y: f32| {
        let mut bytes = Vec::new();
        bytes.extend(x.to_ne_bytes());
        bytes.extend(y.to_ne_bytes());
        bytes.extend([255, 0, 0, 128]);
        bytes
    };
    let debug: Vec<_> = [(0., 0.), (8., 0.), (0., 8.), (8., 8.)]
        .iter()
        .flat_map(|&(x, y)| debug_vertex(x, y))
        .collect();
    let debug_indices: Vec<_> = [0u32, 1, 2, 2, 1, 3]
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    for mode in [BlendMode::PremultipliedAlpha, BlendMode::ShowOverdraw] {
        for pixels in [
            draw_pixels(
                &owner,
                "ps_clear",
                wgt::PrimitiveTopology::TriangleList,
                mode,
                &quad,
                Some(&instance),
                Some((&quad_indices, wgt::IndexFormat::Uint16)),
                6,
            ),
            draw_pixels(
                &owner,
                "debug_color",
                wgt::PrimitiveTopology::TriangleList,
                mode,
                &debug,
                None,
                Some((&debug_indices, wgt::IndexFormat::Uint32)),
                6,
            ),
        ] {
            for pixel in pixels.chunks_exact(4) {
                for (&actual, expected) in pixel.iter().zip([128u8, 0, 128, 255]) {
                    assert!(actual.abs_diff(expected) <= 1, "{:?}: {:?}", mode, pixel);
                }
            }
        }
    }
    let line: Vec<_> = [(0., 4.), (8., 4.)]
        .iter()
        .flat_map(|&(x, y)| debug_vertex(x, y))
        .collect();
    let pixels = draw_pixels(
        &owner,
        "debug_color",
        wgt::PrimitiveTopology::LineList,
        BlendMode::ShowOverdraw,
        &line,
        None,
        None,
        2,
    );
    let colored_rows = pixels.chunks_exact(8 * 4).filter(|row| row[0] != 0).count();
    assert_eq!(colored_rows, 1);
    for row in pixels.chunks_exact(8 * 4) {
        for pixel in row.chunks_exact(4) {
            let expected: [u8; 4] = if row[0] == 0 {
                [0, 0, 255, 255]
            } else {
                [128, 0, 127, 255]
            };
            assert_eq!(pixel, expected);
        }
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
