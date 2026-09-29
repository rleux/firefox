/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::resources::Owned;
use api::units::{DeviceIntRect, DeviceIntSize};
use std::convert::TryInto;

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
    let shader = shaders::SHADERS
        .iter()
        .find(|s| s.name == "ps_clear" && !s.buffer_tables)
        .unwrap();
    assert!(shader.textures.is_empty() && shader.storage_buffers.is_empty());
    assert_eq!(shader.inputs.len(), 3);
    let location = |name| {
        shader
            .inputs
            .iter()
            .find(|input| input.name == name)
            .unwrap()
            .location
    };
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
        let raw = device.open.device.as_ref();
        let bindings = raw
            .create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                label: Some("WR generated clear bindings"),
                flags: hal::BindGroupLayoutFlags::empty(),
                entries: &[wgt::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgt::ShaderStagesWebGPU::from_bits(shader.projection_stages).unwrap().into(),
                    ty: wgt::BindingType::Buffer {
                        ty: wgt::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            })
            .unwrap();
        let bindings = Owned::new(
            &device,
            bindings,
            <dyn hal::DynDevice>::destroy_bind_group_layout,
        );
        let layout = raw
            .create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                label: Some("WR generated clear layout"),
                flags: hal::PipelineLayoutFlags::empty(),
                bind_group_layouts: &[Some(&*bindings)],
                immediate_size: 0,
            })
            .unwrap();
        let layout = Owned::new(
            &device,
            layout,
            <dyn hal::DynDevice>::destroy_pipeline_layout,
        );
        let module = |data: &[u8]| {
            let words: Vec<_> = data
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            let module = raw
                .create_shader_module(
                    &hal::ShaderModuleDescriptor {
                        label: Some("WR generated clear"),
                        runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
                    },
                    hal::ShaderInput::SpirV(&words),
                )
                .unwrap();
            Owned::new(&device, module, <dyn hal::DynDevice>::destroy_shader_module)
        };
        let vs = module(shader.vertex);
        let fs = module(shader.fragment);
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
                layout: &*layout,
                vertex_processor: hal::VertexProcessor::Standard {
                    vertex_buffers: &[
                        Some(hal::VertexBufferLayout {
                            array_stride: 4,
                            step_mode: wgt::VertexStepMode::Vertex,
                            attributes: &[wgt::VertexAttribute {
                                format: wgt::VertexFormat::Unorm8x2,
                                offset: 0,
                                shader_location: location("aPosition"),
                            }],
                        }),
                        Some(hal::VertexBufferLayout {
                            array_stride: 32,
                            step_mode: wgt::VertexStepMode::Instance,
                            attributes: &[
                                wgt::VertexAttribute {
                                    format: wgt::VertexFormat::Float32x4,
                                    offset: 0,
                                    shader_location: location("aRect"),
                                },
                                wgt::VertexAttribute {
                                    format: wgt::VertexFormat::Float32x4,
                                    offset: 16,
                                    shader_location: location("aColor"),
                                },
                            ],
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
            <dyn hal::DynDevice>::destroy_render_pipeline,
        );
        let group = raw
            .create_bind_group(&hal::BindGroupDescriptor {
                label: Some("WR generated clear group"),
                layout: &*bindings,
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
        let group = Owned::new(&device, group, <dyn hal::DynDevice>::destroy_bind_group);
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
        encoder.set_render_pipeline(&*pipeline);
        encoder.set_bind_group(&*layout, 0, &*group, &[]);
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
