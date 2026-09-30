/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::bindings::DrawBindings;
use api::units::{DeviceIntRect, DeviceIntSize};
use crate::device::RenderState;
use super::super::shader::{create_draw_shader_layouts, create_shader_module};

#[test]
fn draw_shader_selection_preserves_variants_and_accepts_feature_order() {
    use super::super::shader::{draw_vertex_descriptor, select_draw_shader};

    for artifact in shaders::SHADERS {
        let mut features: Vec<_> = artifact
            .features
            .split(',')
            .filter(|f| !f.is_empty())
            .collect();
        for rect in [false, true] {
            features.reverse();
            if rect {
                for feature in &mut features {
                    if *feature == "TEXTURE_2D" {
                        *feature = "TEXTURE_RECT";
                    }
                }
            }
            let selected =
                select_draw_shader(artifact.name, &features, artifact.buffer_tables).unwrap();
            assert!(std::ptr::eq(selected, artifact));
            vertex_layouts(draw_vertex_descriptor(selected).unwrap(), selected).unwrap();
        }
        assert!(!artifact.buffer_tables);
        assert!(select_draw_shader(artifact.name, &features, true).is_err());
    }
}

#[test]
fn draw_shader_selection_rejects_missing_or_ambiguous_variants() {
    use super::super::shader::{draw_vertex_descriptor, select_draw_shader};

    for (name, features) in [
        ("missing", &[][..]),
        ("ps_clear", &["TEXTURE_2D"][..]),
        ("ps_clear", &[""][..]),
        ("cs_scale", &["TEXTURE_2D", "TEXTURE_2D"][..]),
        ("cs_scale", &["TEXTURE_RECT", "TEXTURE_2D"][..]),
        ("cs_scale", &["UNKNOWN_TEXTURE_RECT"][..]),
        ("ps_text_run", &["TEXTURE_2D,DUAL_SOURCE_BLENDING"][..]),
    ] {
        for buffer_tables in [false, true] {
            let error = select_draw_shader(name, features, buffer_tables)
                .err()
                .unwrap();
            assert!(error.contains(name), "{}", error);
            assert!(error.contains(&features.join(",")), "{}", error);
            assert!(error.contains(if buffer_tables {
                "buffer tables"
            } else {
                "texture tables"
            }));
        }
    }
    let unknown = webrender_build::vulkan::ShaderArtifact {
        name: "future_primitive_shader",
        ..shaders::SHADERS[0]
    };
    assert!(draw_vertex_descriptor(&unknown).is_err());
}

#[test]
fn generated_shader_bindings_are_dense_and_stage_qualified() {
    let mut identities = std::collections::HashSet::new();
    assert!(!shaders::SHADERS.is_empty());
    for shader in shaders::SHADERS {
        assert!(!shader.buffer_tables && shader.storage_buffers.is_empty());
        assert!(identities.insert((shader.name, shader.features, shader.buffer_tables)));
        for binary in [shader.vertex, shader.fragment] {
            assert_eq!(&binary[..4], &0x07230203u32.to_le_bytes());
            assert_eq!(&binary[4..8], &0x00010300u32.to_le_bytes());
        }
        let mut bindings = Vec::new();
        if shader.projection_stages != 0 {
            bindings.push(0);
        }
        for texture in shader.textures {
            assert!(texture.stages > 0 && texture.stages < 4);
            assert_eq!(texture.sampler_stages & !texture.stages, 0);
            bindings.push(texture.binding);
            if texture.sampler_stages != 0 {
                bindings.push(texture.binding + 1);
            }
        }
        for buffer in shader.storage_buffers {
            assert!(shader.buffer_tables);
            assert!(buffer.stages > 0 && buffer.stages < 4);
            bindings.push(buffer.binding);
        }
        bindings.sort_unstable();
        assert_eq!(
            bindings,
            (0..bindings.len() as u32).collect::<Vec<_>>(),
            "{} {}",
            shader.name,
            shader.features
        );
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn generated_clear_shader_draws_through_reflected_interfaces() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    assert_eq!(
        draw_clear(
            &device,
            &[(RenderState::default(), [1.0, 0.0, 0.0, 1.0])],
            None
        ),
        [255, 0, 0, 255].repeat(4),
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

pub(super) fn draw_clear(
    device: &Rc<Device>,
    draws: &[(RenderState, [f32; 4])],
    depth_clear: Option<f32>,
) -> Vec<u8> {
    draw_quads(device, draws, depth_clear, None)
}

pub(in crate::device::wgpu) fn draw_scaled_texture(
    device: &Rc<Device>,
    texture: Rc<Texture>,
    samplers: Rc<Samplers>,
) -> Vec<u8> {
    draw_quads(
        device,
        &[(RenderState::default(), [0.0, 0.0, 1.0, 1.0])],
        None,
        Some((texture, samplers)),
    )
}

fn draw_quads(
    device: &Rc<Device>,
    draws: &[(RenderState, [f32; 4])],
    depth_clear: Option<f32>,
    source: Option<(Rc<Texture>, Rc<Samplers>)>,
) -> Vec<u8> {
    assert!(!draws.is_empty());
    let sampled = source.is_some();
    let shader = super::super::shader::select_draw_shader(
        if sampled { "cs_scale" } else { "ps_clear" },
        if sampled { &["TEXTURE_2D"] } else { &[] },
        false,
    )
    .unwrap();
    let instance_stride = if sampled { 36 } else { 32 };
    let quad = Buffer::new(
        device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let bytes = |values: &[f32]| {
        values
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect::<Vec<_>>()
    };
    let mut instance_data = Vec::new();
    for (_, data) in draws {
        instance_data.extend_from_slice(&bytes(&[-1.0, -1.0, 1.0, 1.0]));
        instance_data.extend_from_slice(&bytes(data));
        if sampled {
            instance_data.extend_from_slice(&0.0f32.to_ne_bytes());
        }
    }
    let instances = Buffer::new(device, &instance_data, wgt::BufferUses::VERTEX).unwrap();
    let transform = Buffer::new(
        device,
        &bytes(&[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]),
        wgt::BufferUses::UNIFORM,
    )
    .unwrap();
    let target = Texture::new(
        device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let depth = depth_clear.map(|_| {
        Texture::new(
            device,
            2,
            2,
            wgt::TextureFormat::Depth32Float,
            TextureFilter::Nearest,
            true,
        )
        .unwrap()
    });
    let pipelines: Vec<_> = draws
        .iter()
        .map(|(state, _)| {
            DrawPipeline::new(
                device,
                shader,
                target.format(),
                depth.is_some(),
                *state,
            )
            .unwrap()
        })
        .collect();
    let pending: Vec<_> = pipelines.iter().map(Rc::downgrade).collect();
    let (textures, samplers) = match source {
        Some((texture, samplers)) => (vec![(texture, TextureFilter::Linear)], Some(samplers)),
        None => (Vec::new(), None),
    };
    let recorded_draws: Vec<_> = pipelines
        .iter()
        .enumerate()
        .map(|(index, pipeline)| super::super::draw::Draw {
            bindings: DrawBindings::new(
                pipeline,
                Some(transform.clone()),
                textures.clone(),
                Vec::new(),
                samplers.clone(),
            )
            .unwrap(),
            instances: instances.clone(),
            instance_offset: index as u64 * instance_stride,
            instance_count: 1,
            scissor: DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
        })
        .collect();
    let mut submission = Submission::new(device).unwrap();
    let mut commands = submission.recording().unwrap();
    super::super::draw::DrawPass {
            viewport: None,
        target: &target,
        origin: api::units::DeviceIntPoint::zero(),
        depth: depth.as_ref(),
        clear_color: Some(wgt::Color::BLUE),
        clear_depth: depth_clear,
        // ps_clear writes the far depth; use an interior depth for comparisons.
        depth_range: 0.0..0.5,
    }
    .record(&mut commands, &quad, &recorded_draws)
    .unwrap();
    drop(recorded_draws);
    drop(pipelines);
    assert!(pending.iter().all(|pipeline| pipeline.upgrade().is_some()));
    drop(commands);
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert!(pending.iter().all(|pipeline| pipeline.upgrade().is_none()));
    target
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
        .unwrap()
        .wait()
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn generated_draw_shaders_create_modules_and_layouts() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let owner = Rc::downgrade(&device);
    let mut retained = None;
    let mut count = 0;
    for shader in shaders::SHADERS {
        if shader.features.contains("DUAL_SOURCE_BLENDING")
            && !device
                .features()
                .contains(wgt::Features::DUAL_SOURCE_BLENDING)
        {
            continue;
        }
        let layouts = create_draw_shader_layouts(&device, shader)
            .unwrap_or_else(|error| panic!("{} {}: {error}", shader.name, shader.features));
        let vertex = create_shader_module(&device, shader, false).unwrap();
        let fragment = create_shader_module(&device, shader, true).unwrap();
        retained = Some((layouts, vertex, fragment));
        count += 1;
    }
    assert!(count > 1);
    eprintln!("Created modules and layouts for {count} shader pairs");
    drop(device);
    assert!(owner.upgrade().is_some());
    drop(retained);
    assert!(owner.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
