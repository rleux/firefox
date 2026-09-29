/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::resources::Owned;
use api::units::{DeviceIntRect, DeviceIntSize};
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
    let shader = super::super::shader::select_draw_shader("ps_clear", &[], false).unwrap();
    assert!(shader.textures.is_empty() && shader.storage_buffers.is_empty());
    assert_eq!(shader.inputs.len(), 3);
    let (vertex, instances_layout, instance_stride) =
        vertex_layouts(super::super::shader::draw_vertex_descriptor(shader).unwrap(), shader).unwrap();
    let quad = Buffer::new(
        &device,
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
    let instances = Buffer::new(
        &device,
        &bytes(&[-1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0]),
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let transform = Buffer::new(
        &device,
        &bytes(&[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]),
        wgt::BufferUses::UNIFORM,
    )
    .unwrap();
    let target = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    unsafe {
        let raw = device.raw_device();
        let (layout, bindings) = create_draw_shader_layouts(&device, shader).unwrap();
        let vs = create_shader_module(&device, shader, false).unwrap();
        let fs = create_shader_module(&device, shader, true).unwrap();
        let constants = Default::default();
        let stage = |module| hal::ProgrammableStage {
            module,
            entry_point: "main",
            constants: &constants,
            zero_initialize_workgroup_memory: false,
        };
        let pipeline = raw
            .create_render_pipeline(&hal::RenderPipelineDescriptor {
                label: Some("WR generated clear"),
                layout: &layout,
                vertex_processor: hal::VertexProcessor::Standard {
                    vertex_buffers: &[
                        Some(hal::VertexBufferLayout {
                            array_stride: 4,
                            step_mode: wgt::VertexStepMode::Vertex,
                            attributes: &vertex,
                        }),
                        Some(hal::VertexBufferLayout {
                            array_stride: instance_stride,
                            step_mode: wgt::VertexStepMode::Instance,
                            attributes: &instances_layout,
                        }),
                    ],
                    vertex_stage: stage(&*vs),
                },
                fragment_stage: Some(stage(&*fs)),
                primitive: wgt::PrimitiveState {
                    topology: wgt::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                color_targets: &[Some(wgt::ColorTargetState {
                    format: target.format(),
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })],
                multiview_mask: None,
                cache: None,
            })
            .unwrap();
        let pipeline = Owned::new(
            &device,
            pipeline,
            hal::vulkan::Device::destroy_render_pipeline,
        );
        let group = raw
            .create_bind_group(&hal::BindGroupDescriptor {
                label: Some("WR generated clear group"),
                layout: &bindings,
                buffers: &[transform.binding()],
                samplers: &[],
                textures: &[],
                acceleration_structures: &[],
                external_textures: &[],
                entries: &[hal::BindGroupEntry {
                    binding: 0,
                    resource_index: 0,
                    count: 1,
                }],
            })
            .unwrap();
        let group = Owned::new(&device, group, hal::vulkan::Device::destroy_bind_group);
        let mut commands_submission = Submission::new(&device).unwrap();
        let mut commands = commands_submission.recording().unwrap();
        quad.transition(&mut commands, wgt::BufferUses::VERTEX)
            .unwrap();
        instances
            .transition(&mut commands, wgt::BufferUses::VERTEX)
            .unwrap();
        transform
            .transition(&mut commands, wgt::BufferUses::UNIFORM)
            .unwrap();
        target
            .transition(&mut commands, wgt::TextureUses::COLOR_TARGET)
            .unwrap();
        let encoder = commands.encoder();
        encoder
            .begin_render_pass(&hal::RenderPassDescriptor {
                label: Some("WR generated clear"),
                extent: target.size(),
                sample_count: 1,
                color_attachments: &[Some(hal::ColorAttachment {
                    target: hal::Attachment {
                        view: target.target_view().unwrap(),
                        usage: wgt::TextureUses::COLOR_TARGET,
                    },
                    depth_slice: None,
                    resolve_target: None,
                    ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                    clear_value: wgt::Color::TRANSPARENT,
                })],
                depth_stencil_attachment: None,
                multiview_mask: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .unwrap();
        encoder.set_render_pipeline(&pipeline);
        encoder.set_bind_group(&layout, 0, &group, &[]);
        encoder.set_viewport(
            &hal::Rect {
                x: 0.0,
                y: 0.0,
                w: 2.0,
                h: 2.0,
            },
            0.0..1.0,
        );
        encoder.set_scissor_rect(&hal::Rect {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
        });
        encoder.set_vertex_buffer(0, quad.binding());
        encoder.set_vertex_buffer(1, instances.binding());
        encoder.draw(0, 4, 0, 1);
        encoder.end_render_pass();
        target.initialize(&mut commands).unwrap();
        drop(commands);
        commands_submission.submit().unwrap();
        commands_submission.wait(None).unwrap();
    }
    assert_eq!(
        target
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
            .unwrap()
            .wait()
            .unwrap(),
        [255, 0, 0, 255].repeat(4)
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
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
        let (layout, bindings) = create_draw_shader_layouts(&device, shader)
            .unwrap_or_else(|error| panic!("{} {}: {error}", shader.name, shader.features));
        let vertex = create_shader_module(&device, shader, false).unwrap();
        let fragment = create_shader_module(&device, shader, true).unwrap();
        retained = Some((layout, bindings, vertex, fragment));
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
