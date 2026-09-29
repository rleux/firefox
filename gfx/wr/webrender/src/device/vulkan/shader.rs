/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, wgt, Device};
use crate::device::VertexDescriptor;
use crate::renderer::desc;
use std::{convert::TryInto, rc::Rc};
use webrender_build::vulkan::{ScalarType, ShaderArtifact};
use wgpu_hal::Device as _;

#[cfg(wr_vulkan_shaders)]
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
        "cs_blur" => &desc::BLUR,
        "cs_scale" => &desc::SCALE,
        "cs_line_decoration" => &desc::LINE,
        "cs_border_segment" | "cs_border_solid" => &desc::BORDER,
        "cs_svg_filter_node" => &desc::SVG_FILTER_NODE,
        "ps_quad_mask" => &desc::MASK,
        "composite" => &desc::COMPOSITE,
        "ps_clear" => &desc::CLEAR,
        "ps_copy" => &desc::COPY,
        "ps_text_run" | "ps_split_composite" | "ps_quad_textured" | "ps_quad_repeat"
        | "ps_quad_box_shadow" | "ps_quad_yuv" | "ps_quad_backdrop" | "ps_quad_blend"
        | "ps_quad_mix_blend" | "ps_quad_gradient" => &desc::PRIM_INSTANCES,
        name => return Err(format!("No Vulkan vertex descriptor for {name}")),
    })
}

pub(super) fn create_shader_module(
    owner: &Rc<Device>,
    artifact: &ShaderArtifact,
    fragment: bool,
) -> Result<Owned<hal::vulkan::ShaderModule>, String> {
    let data = if fragment {
        artifact.fragment
    } else {
        artifact.vertex
    };
    let words: Vec<_> = data
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    let raw = unsafe {
        owner.open.device.create_shader_module(
            &hal::ShaderModuleDescriptor {
                label: Some(artifact.name),
                runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
            },
            hal::ShaderInput::SpirV(&words),
        )
    }
    .map_err(|error| format!("Creating shader module {}: {error:?}", artifact.name))?;
    Ok(Owned::new(
        owner,
        raw,
        hal::vulkan::Device::destroy_shader_module,
    ))
}

pub(super) fn create_draw_shader_layouts(
    owner: &Rc<Device>,
    artifact: &ShaderArtifact,
) -> Result<
    (
        Owned<hal::vulkan::PipelineLayout>,
        Owned<hal::vulkan::BindGroupLayout>,
    ),
    String,
> {
    let native = &owner.open.device;
    let mut entries = Vec::new();
    if artifact.projection_stages != 0 {
        entries.push(wgt::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgt::ShaderStages::from_bits_retain(artifact.projection_stages),
            ty: wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Uniform,
                has_dynamic_offset: false,
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
            visibility: wgt::ShaderStages::from_bits_retain(binding.stages),
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
                visibility: wgt::ShaderStages::from_bits_retain(binding.sampler_stages),
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
            visibility: wgt::ShaderStages::from_bits_retain(binding.stages),
            ty: wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: std::num::NonZeroU64::new(16),
            },
            count: None,
        });
    }
    entries.sort_by_key(|entry| entry.binding);
    let bindings = Owned::new(
        owner,
        unsafe {
            native.create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                label: Some("WR bindings"),
                flags: hal::BindGroupLayoutFlags::empty(),
                entries: &entries,
            })
        }
        .map_err(|e| format!("Creating binding layout: {e:?}"))?,
        hal::vulkan::Device::destroy_bind_group_layout,
    );
    let layout = Owned::new(
        owner,
        unsafe {
            native.create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                label: Some("WR pipelines"),
                flags: hal::PipelineLayoutFlags::empty(),
                bind_group_layouts: &[Some(&*bindings)],
                immediate_size: 0,
            })
        }
        .map_err(|e| format!("Creating pipeline layout: {e:?}"))?,
        hal::vulkan::Device::destroy_pipeline_layout,
    );
    Ok((layout, bindings))
}
