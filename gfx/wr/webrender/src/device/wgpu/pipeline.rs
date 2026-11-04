/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::shader::{draw_vertex_descriptor, prepare_shader, PreparedShader};
use super::vertex_layout::vertex_layouts;
use super::{core, wgt, Device, Failure};
use crate::device::{BlendMode, DepthFunction, RenderState};
use std::{borrow::Cow, sync::Arc};
use webrender_build::vulkan::ShaderArtifact;

pub(super) struct DrawPipeline {
    pub owner: Arc<Device>,
    pub shader: &'static ShaderArtifact,
    pub prepared: Arc<PreparedShader>,
    pub raw: Arc<core::pipeline::RenderPipeline>,
    pub format: wgt::TextureFormat,
    pub has_depth: bool,
    pub topology: wgt::PrimitiveTopology,
    pub vertex_stride: u64,
    pub instance_stride: u64,
}

impl DrawPipeline {
    pub fn new(
        owner: &Arc<Device>,
        shader: &'static ShaderArtifact,
        format: wgt::TextureFormat,
        has_depth: bool,
        state: RenderState,
        topology: wgt::PrimitiveTopology,
    ) -> Result<Arc<Self>, Failure> {
        let dual_source = shader
            .features
            .split(',')
            .any(|f| f == "DUAL_SOURCE_BLENDING");
        if dual_source
            && !owner
                .features()
                .contains(wgt::Features::DUAL_SOURCE_BLENDING)
        {
            return Err(Failure::validation(
                "Vulkan adapter has no dual-source blending support",
            ));
        }
        if state.blend_mode == BlendMode::SubpixelDualSource && !dual_source {
            return Err(Failure::validation(
                "Dual-source blending requires a dual-source shader",
            ));
        }
        let blend = blend_state(state.blend_mode).map_err(Failure::validation)?;
        let capabilities = owner.adapter.get_texture_format_features(format);
        if !matches!(
            format.sample_type(None, Some(owner.features())),
            Some(wgt::TextureSampleType::Float { .. })
        ) || !capabilities
            .allowed_usages
            .contains(wgt::TextureUsages::RENDER_ATTACHMENT)
            || (blend.is_some()
                && !capabilities
                    .flags
                    .contains(wgt::TextureFormatFeatureFlags::BLENDABLE))
        {
            return Err(Failure::validation(format!(
                "Unsupported Vulkan draw target format {format:?}"
            )));
        }
        let descriptor = draw_vertex_descriptor(shader).map_err(Failure::validation)?;
        let (vertex, instances, vertex_stride, stride) =
            vertex_layouts(descriptor, shader).map_err(Failure::validation)?;
        let vertex_stride = vertex_stride.max(4);
        let mut vertex_buffers = vec![Some(core::pipeline::VertexBufferLayout {
            array_stride: vertex_stride,
            step_mode: wgt::VertexStepMode::Vertex,
            attributes: Cow::Owned(vertex),
        })];
        if stride != 0 {
            vertex_buffers.push(Some(core::pipeline::VertexBufferLayout {
                array_stride: stride,
                step_mode: wgt::VertexStepMode::Instance,
                attributes: Cow::Owned(instances),
            }));
        }
        let prepared = prepare_shader(owner, shader)?;
        let stage = |module| core::pipeline::ProgrammableStageDescriptor {
            module,
            entry_point: Some("main".into()),
            constants: Default::default(),
            zero_initialize_workgroup_memory: true,
        };
        let raw = owner.device.create_render_pipeline(
            core::pipeline::RenderPipelineDescriptor {
                label: Some(format!("{} [{}]", shader.name, shader.features).into()),
                layout: Some(prepared.layouts.layout.clone()),
                vertex: core::pipeline::VertexState {
                    stage: stage(prepared.vertex.clone()),
                    buffers: Cow::Owned(vertex_buffers),
                },
                fragment: Some(core::pipeline::FragmentState {
                    stage: stage(prepared.fragment.clone()),
                    targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                        format,
                        blend,
                        write_mask: if state.color_write {
                            wgt::ColorWrites::ALL
                        } else {
                            wgt::ColorWrites::empty()
                        },
                    })]),
                }),
                primitive: wgt::PrimitiveState {
                    topology,
                    ..Default::default()
                },
                depth_stencil: has_depth.then(|| wgt::DepthStencilState {
                    format: wgt::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(state.depth_write && state.depth_test.is_some()),
                    depth_compare: Some(match state.depth_test {
                        None | Some(DepthFunction::Always) => wgt::CompareFunction::Always,
                        Some(DepthFunction::Less) => wgt::CompareFunction::Less,
                        Some(DepthFunction::LessEqual) => wgt::CompareFunction::LessEqual,
                    }),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            }
            .into(),
        );
        owner.check()?;
        Ok(Arc::new(Self {
            owner: owner.clone(),
            shader,
            prepared,
            raw,
            format,
            has_depth,
            topology,
            vertex_stride,
            instance_stride: stride,
        }))
    }
}

fn blend_state(mode: BlendMode) -> Result<Option<wgt::BlendState>, String> {
    use wgt::BlendFactor as Factor;
    let component = |src_factor, dst_factor| wgt::BlendComponent {
        src_factor,
        dst_factor,
        operation: wgt::BlendOperation::Add,
    };
    Ok(match mode {
        BlendMode::None => None,
        BlendMode::PremultipliedAlpha | BlendMode::ShowOverdraw => {
            Some(wgt::BlendState::PREMULTIPLIED_ALPHA_BLENDING)
        }
        BlendMode::Alpha => Some(wgt::BlendState::ALPHA_BLENDING),
        BlendMode::Multiply => Some(wgt::BlendState {
            color: component(Factor::Zero, Factor::Src),
            alpha: component(Factor::Zero, Factor::SrcAlpha),
        }),
        BlendMode::PremultipliedDestOut => Some(wgt::BlendState {
            color: component(Factor::Zero, Factor::OneMinusSrcAlpha),
            alpha: component(Factor::Zero, Factor::OneMinusSrcAlpha),
        }),
        BlendMode::SubpixelDualSource => Some(wgt::BlendState {
            color: component(Factor::One, Factor::OneMinusSrc1),
            alpha: component(Factor::One, Factor::OneMinusSrc1Alpha),
        }),
        BlendMode::Screen => Some(wgt::BlendState {
            color: component(Factor::One, Factor::OneMinusSrc),
            alpha: component(Factor::One, Factor::OneMinusSrcAlpha),
        }),
        BlendMode::Exclusion => Some(wgt::BlendState {
            color: component(Factor::OneMinusDst, Factor::OneMinusSrc),
            alpha: component(Factor::One, Factor::OneMinusSrcAlpha),
        }),
        BlendMode::PlusLighter => Some(wgt::BlendState {
            color: component(Factor::One, Factor::One),
            alpha: component(Factor::One, Factor::One),
        }),
        mode => return Err(format!("Unsupported Vulkan blend mode {mode:?}")),
    })
}
