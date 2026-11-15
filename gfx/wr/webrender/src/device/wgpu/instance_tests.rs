/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::tests::{device, finish, VALIDATION_ERRORS};
use super::*;
use crate::gpu_types::{MaskInstance, PrimitiveInstanceData, QuadInstance};
use crate::render_task::RenderTaskAddress;
use crate::renderer::desc;
use crate::transform::GpuTransformId;
use std::{convert::TryInto, fs, process::Command};
use webrender_build::vulkan::{ScalarType, VertexInput};

fn module(owner: &Device, stage: &str, source: &str) -> Arc<core::pipeline::ShaderModule> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "wr-instance-test-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    let input = directory.join(format!("test.{stage}"));
    let binary = directory.join("test.spv");
    fs::write(&input, source).unwrap();
    for (tool, arguments) in [
        (
            "GLSLANG_VALIDATOR",
            vec!["-V", "--target-env", "vulkan1.1", "-o"],
        ),
        ("SPIRV_VAL", vec!["--target-env", "vulkan1.1"]),
    ] {
        let fallback = if tool == "GLSLANG_VALIDATOR" {
            "glslangValidator"
        } else {
            "spirv-val"
        };
        let mut command = Command::new(std::env::var_os(tool).unwrap_or_else(|| fallback.into()));
        command.args(arguments).arg(&binary);
        if tool == "GLSLANG_VALIDATOR" {
            command.arg(&input);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{:?}: {:?}", command, output);
    }
    let bytes = fs::read(binary).unwrap();
    let words = bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    fs::remove_dir_all(directory).unwrap();
    // These test fixtures were just validated by spirv-val.
    let module = unsafe {
        owner.device.create_shader_module_passthrough(
            &core::pipeline::ShaderModuleDescriptorPassthrough {
                label: Some("instance byte-layout test".into()),
                spirv: Some(Cow::Owned(words)),
                entry_points: Cow::Owned(vec![wgt::PassthroughShaderEntryPoint {
                    name: "main".into(),
                    workgroup_size: (0, 0, 0),
                }]),
                ..Default::default()
            },
        )
    };
    owner.check().unwrap();
    module
}

fn pipeline(owner: &Arc<Device>, mask: bool) -> Arc<core::pipeline::RenderPipeline> {
    let extra = if mask {
        "layout(location=1) in ivec4 clip;"
    } else {
        ""
    };
    let sum = if mask {
        "+ clip.x * 11 + clip.y * 13 + clip.z * 17 + clip.w * 19"
    } else {
        ""
    };
    let vertex = module(
        owner,
        "vert",
        &format!(
            r#"#version 450
layout(location=0) in ivec4 data;
{extra}
layout(location=0) flat out vec4 color;
void main() {{
    int i = gl_VertexIndex;
    vec2 p = vec2(i == 1 || i == 4 || i == 5 ? 1.0 : 0.0,
                  i == 2 || i == 3 || i == 5 ? 1.0 : 0.0);
    gl_Position = vec4((p + vec2(data.x, 0)) / vec2(4, 0.5) - 1, 0, 1);
    int sum = (data.x + data.y * 3 + data.z * 5 + data.w * 7) {sum};
    color = vec4(sum % 251, sum / 251, 0, 255) / 255.0;
}}"#
        ),
    );
    let fragment = module(
        owner,
        "frag",
        r#"#version 450
layout(location=0) flat in vec4 color;
layout(location=0) out vec4 output_color;
void main() { output_color = color; }
"#,
    );
    let descriptor = if mask {
        &desc::MASK
    } else {
        &desc::PRIM_INSTANCES
    };
    static INPUTS: [VertexInput; 2] = [
        VertexInput {
            name: "aData",
            location: 0,
            scalar: ScalarType::Sint,
            components: 4,
        },
        VertexInput {
            name: "aClipData",
            location: 1,
            scalar: ScalarType::Sint,
            components: 4,
        },
    ];
    let inputs = if mask { &INPUTS[..] } else { &INPUTS[..1] };
    let artifact = webrender_build::vulkan::ShaderArtifact {
        name: "test",
        features: "",
        buffer_tables: false,
        varying_locations: 1,
        vertex: &[],
        fragment: &[],
        inputs,
        textures: &[],
        storage_buffers: &[],
        projection_stages: 0,
        digest: 0,
    };
    let (_, attributes, _, stride) =
        super::super::vertex_layout::vertex_layouts(descriptor, &artifact).unwrap();
    assert_eq!(
        stride as usize,
        if mask {
            std::mem::size_of::<MaskInstance>()
        } else {
            std::mem::size_of::<PrimitiveInstanceData>()
        }
    );
    let layout =
        owner
            .device
            .create_pipeline_layout(&core::binding_model::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: Cow::Borrowed(&[]),
                immediate_size: 0,
            });
    let stage = |module| core::pipeline::ProgrammableStageDescriptor {
        module,
        entry_point: Some("main".into()),
        constants: Default::default(),
        zero_initialize_workgroup_memory: true,
    };
    let pipeline = owner.device.create_render_pipeline(
        core::pipeline::RenderPipelineDescriptor {
            label: Some("instance byte-layout test".into()),
            layout: Some(layout),
            vertex: core::pipeline::VertexState {
                stage: stage(vertex),
                buffers: Cow::Owned(vec![Some(core::pipeline::VertexBufferLayout {
                    array_stride: stride,
                    step_mode: wgt::VertexStepMode::Instance,
                    attributes: Cow::Owned(attributes),
                })]),
            },
            fragment: Some(core::pipeline::FragmentState {
                stage: stage(fragment),
                targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                    format: wgt::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })]),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        }
        .into(),
    );
    owner.check().unwrap();
    pipeline
}

fn primitive(x: i32, epoch: i32) -> PrimitiveInstanceData {
    QuadInstance {
        dst_task_address: RenderTaskAddress(epoch + 3),
        prim_address_i: x,
        prim_address_f: epoch + 1,
        quad_flags: 0,
        edge_flags: 0,
        part_index: 0,
        segment_index: (epoch + 2) as u8,
    }
    .into()
}

fn check_instance_pages(primary: bool) {
    let selected = device();
    let owner = Arc::new(
        Device::new(&super::super::Options {
            adapter_name: Some(selected.info().name),
            validation: true,
            primary_mapping: Some(primary),
            ..Default::default()
        })
        .unwrap(),
    );
    assert_eq!(
        owner
            .features()
            .contains(wgt::Features::MAPPABLE_PRIMARY_BUFFERS),
        primary
    );
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let pipelines = [pipeline(&owner, false), pipeline(&owner, true)];
    let mut readbacks = Vec::new();
    for epoch in 0..3 {
        let mut arena = InstanceArena::default();
        let mut slices = Vec::new();
        let mut expected = vec![0; 8 * 4];
        for x in 0..8 {
            let mask = x % 2 != 0;
            let mut source = if mask {
                crate::device::as_bytes(&[MaskInstance {
                    prim: primitive(x, epoch),
                    clip_transform_id: GpuTransformId((epoch + 4) as u32),
                    clip_address: epoch + 5,
                    clip_space: (epoch + 6) as u32,
                    unused: epoch + 7,
                }])
                .to_vec()
            } else {
                crate::device::as_bytes(&[primitive(x, epoch)]).to_vec()
            };
            let weights = [1, 3, 5, 7, 11, 13, 17, 19];
            let sum: i32 = source
                .chunks_exact(4)
                .zip(weights)
                .map(|(bytes, weight)| i32::from_ne_bytes(bytes.try_into().unwrap()) * weight)
                .sum();
            expected[x as usize * 4..][..4].copy_from_slice(&[
                (sum % 251) as u8,
                (sum / 251) as u8,
                0,
                255,
            ]);
            slices.push((mask, arena.append(&owner, &source).unwrap()));
            source.fill(0xDD);
        }
        assert!(Arc::ptr_eq(&slices[2].1.buffer, &slices[3].1.buffer));
        assert_eq!(slices[3].1.offset, 16);
        let padding = arena.append(&owner, &[0xDA; 36]).unwrap();
        let aligned = arena.append(&owner, &[0xBC; 16]).unwrap();
        assert_eq!(aligned.offset % wgt::MAP_ALIGNMENT, 0);
        if Arc::ptr_eq(&padding.buffer, &aligned.buffer) {
            assert_eq!(aligned.offset, padding.offset + 40);
        }
        let large = arena
            .append(&owner, &vec![0xAB; InstanceArena::PAGE_SIZE as usize + 4])
            .unwrap();
        assert_eq!(large.buffer.size(), InstanceArena::PAGE_SIZE + 4);
        assert_eq!(large.offset, 0);
        let pages = arena.pages.clone();
        arena.seal(&owner).unwrap();
        let split = arena.append(&owner, &[0xCE; 16]).unwrap();
        assert_eq!(split.offset, 0);
        assert_eq!(split.buffer.size(), 16);
        assert!(pages.iter().all(|page| !Arc::ptr_eq(page, &split.buffer)));
        drop(split);
        for page in &pages {
            assert!(page.get_mapped_range(0, Some(page.size())).is_err());
        }
        let target = Image::new(
            &owner,
            8,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            true,
        )
        .unwrap();
        let encoder = owner
            .create_command_encoder("instance byte-layout test")
            .unwrap();
        let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
            label: None,
            color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
                view: target.levels[0].clone(),
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Clear(wgt::Color::TRANSPARENT),
                store_op: wgt::StoreOp::Store,
            })]),
            ..Default::default()
        });
        for (mask, slice) in slices {
            pass.set_pipeline(pipelines[mask as usize].clone());
            pass.set_vertex_buffer(0, Some(slice.buffer), slice.offset, Some(slice.size));
            pass.draw(6, 1, 0, 0);
        }
        pass.end();
        let readback = owner
            .encode_readback(
                &encoder,
                &target.raw,
                0,
                bounds(&target),
                Some(ImageFormat::RGBA8),
            )
            .unwrap();
        drop(pages);
        drop(arena);
        drop(padding);
        drop(aligned);
        drop(large);
        let index = owner.submit(&[finish(&encoder)]).unwrap();
        readbacks.push((readback.map(&owner, index).unwrap(), expected));
    }
    for (mut readback, expected) in readbacks {
        assert_eq!(readback.wait(&owner).unwrap(), expected);
    }
    let mut alignment = InstanceArena::default();
    let tiny = alignment.append(&owner, &[0xDD; 36]).unwrap();
    assert_eq!(tiny.buffer.size(), 36);
    let overflow = alignment.append(&owner, &[0xCC; 36]).unwrap();
    let padded = alignment.append(&owner, &[0xBB; 16]).unwrap();
    assert!(Arc::ptr_eq(&overflow.buffer, &padded.buffer));
    assert_eq!(padded.offset, 40);
    assert_eq!(alignment.pages.len(), 2);
    assert_eq!(alignment.pages.iter().map(|p| p.size()).sum::<u64>(), 108);
    drop((alignment, tiny, overflow, padded));
    let mut arena = InstanceArena::default();
    let slice = arena.append(&owner, &[0xAC; 32]).unwrap();
    let abandoned = Arc::downgrade(&slice.buffer);
    drop(slice);
    drop(arena);
    owner.poll().unwrap();
    assert!(abandoned.upgrade().is_none());
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan, primary mapping, glslangValidator, spirv-val and Khronos validation"]
fn native_instance_pages_preserve_primitive_mask_data_with_primary_mapping() {
    check_instance_pages(true);
}

#[test]
#[ignore = "Requires Vulkan, glslangValidator, spirv-val and Khronos validation"]
fn native_instance_pages_preserve_primitive_mask_data_with_staging() {
    check_instance_pages(false);
}

#[test]
#[ignore = "Requires Vulkan and Khronos validation"]
fn native_instance_pages_allocate_exactly_and_bound_growth() {
    let owner = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for cap in [1024, 4096, 16384, 65536] {
        let mut arena = InstanceArena {
            page_size: Some(cap),
            ..Default::default()
        };
        assert!(arena.append(&owner, &[]).is_err());
        assert!(arena.pages.is_empty());
        let first = arena.append(&owner, &[0xCC; 36]).unwrap();
        assert_eq!(first.buffer.size(), 36);
        let mut aligned_bytes = 40;
        for i in 0..2000 {
            let size = if i == 137 {
                cap + 4
            } else {
                [16, 32, 36, 64][i % 4]
            };
            let slice = arena.append(&owner, &vec![0xBB; size as usize]).unwrap();
            aligned_bytes += (size + 7) / 8 * 8;
            assert_eq!(slice.offset % wgt::MAP_ALIGNMENT, 0);
            assert!(slice.offset + slice.size <= slice.buffer.size());
            assert!(slice.buffer.size() <= cap || slice.buffer.size() == size);
        }
        let allocated = arena.pages.iter().map(|page| page.size()).sum::<u64>();
        assert!(
            allocated <= aligned_bytes * 2 + cap,
            "cap {}, allocated {}, aligned {}",
            cap,
            allocated,
            aligned_bytes
        );
        arena.seal(&owner).unwrap();
        let fresh = arena.append(&owner, &[0xAA; 16]).unwrap();
        assert_eq!(fresh.buffer.size(), 16);
        assert!(!Arc::ptr_eq(&first.buffer, &fresh.buffer));
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
