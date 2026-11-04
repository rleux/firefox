/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, wgt, Device, Failure};
use crate::device::VertexDescriptor;
use crate::renderer::desc;
use std::{borrow::Cow, convert::TryInto, sync::Arc};
use webrender_build::vulkan::{ScalarType, ShaderArtifact};

pub(super) fn select_draw_shader(
    name: &str,
    features: &[&str],
    buffer_tables: bool,
) -> Result<&'static ShaderArtifact, String> {
    super::shaders::SHADERS
        .iter()
        .find(|shader| {
            if shader.name != name || shader.buffer_tables != buffer_tables {
                return false;
            }
            let mut compiled = shader
                .features
                .split(',')
                .filter(|feature| !feature.is_empty());
            compiled.clone().count() == features.len()
                && compiled.all(|feature| {
                    features.iter().any(|requested| {
                        feature
                            == match *requested {
                                "TEXTURE_RECT" => "TEXTURE_2D",
                                other => other,
                            }
                    })
                })
        })
        .ok_or_else(|| {
            format!(
                "No Vulkan draw shader for {name} [{}] with {} tables",
                features.join(","),
                if buffer_tables { "buffer" } else { "texture" },
            )
        })
}

pub(super) fn draw_vertex_descriptor(
    shader: &ShaderArtifact,
) -> Result<&'static VertexDescriptor, String> {
    Ok(match shader.name {
        "debug_color" => &desc::DEBUG_COLOR,
        "debug_font" => &desc::DEBUG_FONT,
        "cs_blur" => &desc::BLUR,
        "cs_scale" => &desc::SCALE,
        "cs_path_tile" => &desc::PATH_TILE,
        "cs_line_decoration" => &desc::LINE,
        "cs_border_segment" | "cs_border_solid" => &desc::BORDER,
        "cs_svg_filter_node" => &desc::SVG_FILTER_NODE,
        "ps_quad_mask" => &desc::MASK,
        "composite" => &desc::COMPOSITE,
        "ps_clear" => &desc::CLEAR,
        "ps_copy" => &desc::COPY,
        "ps_path_tile" | "ps_text_run" | "ps_split_composite" | "ps_quad_textured" | "ps_quad_repeat"
        | "ps_quad_box_shadow" | "ps_quad_yuv" | "ps_quad_backdrop" | "ps_quad_blend"
        | "ps_quad_mix_blend" | "ps_quad_gradient" => &desc::PRIM_INSTANCES,
        name => return Err(format!("No Vulkan vertex descriptor for {name}")),
    })
}

pub(super) struct ShaderLayouts {
    pub layout: Arc<core::binding_model::PipelineLayout>,
    pub bindings: Arc<core::binding_model::BindGroupLayout>,
}

pub(super) struct PreparedShader {
    pub vertex: Arc<core::pipeline::ShaderModule>,
    pub fragment: Arc<core::pipeline::ShaderModule>,
    pub layouts: ShaderLayouts,
}

pub(super) fn prepare_shader(
    owner: &Device,
    artifact: &'static ShaderArtifact,
) -> Result<Arc<PreparedShader>, Failure> {
    validate_shader_interface(
        artifact,
        owner.limits(),
        owner.adapter.downlevel_capabilities().flags,
    )
    .map_err(Failure::validation)?;
    let mut cache = owner.shaders.lock().unwrap();
    let key = artifact as *const ShaderArtifact as usize;
    if let Some(shader) = cache.get(&key).and_then(std::sync::Weak::upgrade) {
        return Ok(shader);
    }
    let module = |bytes: &[u8], stage: &str| {
        if bytes.len() % 4 != 0 {
            return Err(Failure::validation("Unaligned Vulkan shader artifact"));
        }
        let label = format!("{} [{}] {stage}", artifact.name, artifact.features);
        let words = bytes
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        // Only the trusted built-in catalog, validated and reflected at build time.
        let module = unsafe {
            owner.device.create_shader_module_passthrough(
                &core::pipeline::ShaderModuleDescriptorPassthrough {
                    label: Some(label.into()),
                    entry_points: Cow::Owned(vec![wgt::PassthroughShaderEntryPoint {
                        name: "main".into(),
                        workgroup_size: (0, 0, 0),
                    }]),
                    spirv: Some(Cow::Owned(words)),
                    ..Default::default()
                },
            )
        };
        owner.check()?;
        Ok(module)
    };
    let shader = Arc::new(PreparedShader {
        vertex: module(artifact.vertex, "vertex")?,
        fragment: module(artifact.fragment, "fragment")?,
        layouts: create_layouts(owner, artifact)?,
    });
    cache.retain(|_, shader| shader.strong_count() != 0);
    cache.insert(key, Arc::downgrade(&shader));
    Ok(shader)
}

fn create_layouts(owner: &Device, artifact: &ShaderArtifact) -> Result<ShaderLayouts, Failure> {
    let mut entries = Vec::new();
    let stages = |bits| wgt::ShaderStagesWebGPU::from_bits_retain(bits).into();
    if artifact.projection_stages != 0 {
        entries.push(wgt::BindGroupLayoutEntry {
            binding: 0,
            visibility: stages(artifact.projection_stages),
            ty: wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: std::num::NonZeroU64::new(64),
            },
            count: None,
        });
    }
    for binding in artifact.textures {
        let filtering = binding.name.starts_with("sColor");
        let sample_type = match binding.scalar {
            ScalarType::Float => wgt::TextureSampleType::Float {
                filterable: filtering,
            },
            ScalarType::Sint => wgt::TextureSampleType::Sint,
            ScalarType::Uint => wgt::TextureSampleType::Uint,
        };
        entries.push(wgt::BindGroupLayoutEntry {
            binding: binding.binding,
            visibility: stages(binding.stages),
            ty: wgt::BindingType::Texture {
                sample_type,
                view_dimension: wgt::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
        if binding.sampler_stages != 0 {
            entries.push(wgt::BindGroupLayoutEntry {
                binding: binding.binding + 1,
                visibility: stages(binding.sampler_stages),
                ty: wgt::BindingType::Sampler(if filtering {
                    wgt::SamplerBindingType::Filtering
                } else {
                    wgt::SamplerBindingType::NonFiltering
                }),
                count: None,
            });
        }
    }
    for binding in artifact.storage_buffers {
        entries.push(wgt::BindGroupLayoutEntry {
            binding: binding.binding,
            visibility: stages(binding.stages),
            ty: wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: std::num::NonZeroU64::new(16),
            },
            count: None,
        });
    }
    entries.sort_by_key(|entry| entry.binding);
    let bindings =
        owner
            .device
            .create_bind_group_layout(&core::binding_model::BindGroupLayoutDescriptor {
                label: Some("WR draw bindings".into()),
                entries: Cow::Owned(entries),
            });
    owner.check()?;
    let layout =
        owner
            .device
            .create_pipeline_layout(&core::binding_model::PipelineLayoutDescriptor {
                label: Some("WR draw pipeline".into()),
                bind_group_layouts: Cow::Owned(vec![Some(bindings.clone())]),
                immediate_size: 0,
            });
    owner.check()?;
    Ok(ShaderLayouts { layout, bindings })
}

fn validate_shader_interface(
    artifact: &ShaderArtifact,
    limits: &wgt::Limits,
    downlevel: wgt::DownlevelFlags,
) -> Result<(), String> {
    if artifact.varying_locations > limits.max_inter_stage_shader_variables {
        return Err(format!(
            "{} [{}] requires {} varying locations; device supports {}",
            artifact.name,
            artifact.features,
            artifact.varying_locations,
            limits.max_inter_stage_shader_variables,
        ));
    }
    for (stage, required) in [
        (1, wgt::DownlevelFlags::VERTEX_STORAGE),
        (2, wgt::DownlevelFlags::FRAGMENT_STORAGE),
    ] {
        if artifact
            .storage_buffers
            .iter()
            .any(|binding| binding.stages & stage != 0)
            && !downlevel.contains(required)
        {
            return Err(format!("Vulkan shader requires {required:?}"));
        }
    }
    Ok(())
}

#[test]
fn reflected_shader_interface_limits_are_checked_before_passthrough() {
    let artifact = ShaderArtifact {
        varying_locations: 17,
        ..super::shaders::SHADERS[0]
    };
    let limits = wgt::Limits::default();
    assert!(validate_shader_interface(&artifact, &limits, wgt::DownlevelFlags::all()).is_err());
    assert!(validate_shader_interface(
        &ShaderArtifact {
            varying_locations: 16,
            ..artifact
        },
        &limits,
        wgt::DownlevelFlags::all()
    )
    .is_ok());
    for storage_buffers in [
        &[webrender_build::vulkan::StorageBinding {
            name: "table",
            binding: 0,
            scalar: ScalarType::Float,
            stages: 1,
        }][..],
        &[webrender_build::vulkan::StorageBinding {
            name: "table",
            binding: 0,
            scalar: ScalarType::Float,
            stages: 2,
        }][..],
    ] {
        let artifact = ShaderArtifact {
            varying_locations: 0,
            storage_buffers,
            ..artifact
        };
        assert!(
            validate_shader_interface(&artifact, &limits, wgt::DownlevelFlags::empty()).is_err()
        );
        assert!(validate_shader_interface(&artifact, &limits, wgt::DownlevelFlags::all()).is_ok());
    }
}
