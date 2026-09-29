/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, wgt, Device};
use crate::device::VertexDescriptor;
use crate::renderer::desc;
use std::{convert::TryInto, rc::Rc};
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
) -> Result<Owned<dyn hal::DynShaderModule>, String> {
    let data = if fragment {
        artifact.fragment
    } else {
        artifact.vertex
    };
    let words: Vec<_> = data
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    let raw = owner.create_shader_module(artifact.name, &words)?;
    Ok(Owned::new(
        owner,
        raw,
        <dyn hal::DynDevice>::destroy_shader_module,
    ))
}

pub(super) struct ShaderLayouts {
    pub layout: Owned<dyn hal::DynPipelineLayout>,
    pub bindings: Owned<dyn hal::DynBindGroupLayout>,
}

pub(super) fn create_draw_shader_layouts(
    owner: &Rc<Device>,
    artifact: &ShaderArtifact,
) -> Result<Rc<ShaderLayouts>, String> {
    let native = owner.open.device.as_ref();
    let mut entries = Vec::new();
    if artifact.projection_stages != 0 {
        entries.push(wgt::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgt::ShaderStagesWebGPU::from_bits_retain(artifact.projection_stages).into(),
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
            visibility: wgt::ShaderStagesWebGPU::from_bits_retain(binding.stages).into(),
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
                visibility: wgt::ShaderStagesWebGPU::from_bits_retain(binding.sampler_stages).into(),
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
            visibility: wgt::ShaderStagesWebGPU::from_bits_retain(binding.stages).into(),
            ty: wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: std::num::NonZeroU64::new(16),
            },
            count: None,
        });
    }
    entries.sort_by_key(|entry| entry.binding);
    let mut cache = owner.shader_layouts.borrow_mut();
    if let Some(layouts) = cache.get(&entries).and_then(std::rc::Weak::upgrade) {
        return Ok(layouts);
    }
    cache.retain(|_, layouts| layouts.strong_count() != 0);
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
        <dyn hal::DynDevice>::destroy_bind_group_layout,
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
        <dyn hal::DynDevice>::destroy_pipeline_layout,
    );
    let layouts = Rc::new(ShaderLayouts { layout, bindings });
    cache.insert(entries, Rc::downgrade(&layouts));
    Ok(layouts)
}


pub(super) struct PreparedShader {
    pub vertex: Owned<dyn hal::DynShaderModule>,
    pub fragment: Owned<dyn hal::DynShaderModule>,
    pub layouts: Rc<ShaderLayouts>,
}

pub(super) fn prepare_shader(owner: &Rc<Device>, artifact: &'static ShaderArtifact) -> Result<Rc<PreparedShader>, String> {
    let mut cache = owner.prepared_shaders.borrow_mut();
    if let Some(shader) = cache.get(&(artifact as *const _)).and_then(std::rc::Weak::upgrade) {
        return Ok(shader);
    }
    let shader = Rc::new(PreparedShader {
        vertex: create_shader_module(owner, artifact, false)?,
        fragment: create_shader_module(owner, artifact, true)?,
        layouts: create_draw_shader_layouts(owner, artifact)?,
    });
    cache.retain(|_, shader| shader.strong_count() != 0);
    cache.insert(artifact, Rc::downgrade(&shader));
    Ok(shader)
}
