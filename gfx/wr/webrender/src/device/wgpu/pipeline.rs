/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::shader::{prepare_shader, draw_vertex_descriptor};
use super::textures::supports_float_color_format;
use super::{hal, vertex_layouts, wgt, Device};
use crate::device::{BlendMode, DepthFunction, RenderState};
use std::rc::Rc;
use webrender_build::vulkan::ShaderArtifact;

pub(super) struct DrawPipeline {
    pub shader: &'static ShaderArtifact,
    pub(super) _prepared: Rc<super::shader::PreparedShader>,
    pub raw: Owned<dyn hal::DynRenderPipeline>,
    pub layouts: Rc<super::shader::ShaderLayouts>,
    pub format: wgt::TextureFormat,
    pub has_depth: bool,
    pub instance_stride: u64,
}

impl DrawPipeline {
    pub fn new(
        owner: &Rc<Device>,
        shader: &'static ShaderArtifact,
        format: wgt::TextureFormat,
        has_depth: bool,
        state: RenderState,
    ) -> Result<Rc<Self>, String> {
        if owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let dual_source = shader.features.contains("DUAL_SOURCE_BLENDING");
        if dual_source && !owner.features.contains(wgt::Features::DUAL_SOURCE_BLENDING) {
            return Err("Vulkan adapter has no dual-source blending support".into());
        }
        if state.blend_mode == BlendMode::SubpixelDualSource && !dual_source {
            return Err("Dual-source blending requires a dual-source shader".into());
        }
        let blend = blend_state(state.blend_mode)?;
        let mut required = hal::TextureFormatCapabilities::COLOR_ATTACHMENT;
        if blend.is_some() {
            required |= hal::TextureFormatCapabilities::COLOR_ATTACHMENT_BLEND;
        }
        let capabilities = unsafe { owner.adapter.texture_format_capabilities(format) };
        if !supports_float_color_format(format, owner.features, capabilities, required) {
            return Err(format!("Unsupported Vulkan draw target format {format:?}"));
        }
        let (vertex, instances, stride) = vertex_layouts(draw_vertex_descriptor(shader)?, shader)?;
        let prepared = prepare_shader(owner, shader)?;
        let layouts = prepared.layouts.clone();
        let constants = Default::default();
        let stage = |module| hal::ProgrammableStage {
            module,
            entry_point: "main",
            constants: &constants,
            zero_initialize_workgroup_memory: false,
        };
        let raw = unsafe {
            owner
                .open
                .device
                .create_render_pipeline(&hal::RenderPipelineDescriptor {
                    label: Some(shader.name),
                    layout: &*layouts.layout,
                    vertex_processor: hal::VertexProcessor::Standard {
                        vertex_buffers: &[
                            Some(hal::VertexBufferLayout {
                                array_stride: 4,
                                step_mode: wgt::VertexStepMode::Vertex,
                                attributes: &vertex,
                            }),
                            Some(hal::VertexBufferLayout {
                                array_stride: stride,
                                step_mode: wgt::VertexStepMode::Instance,
                                attributes: &instances,
                            }),
                        ],
                        vertex_stage: stage(&*prepared.vertex),
                    },
                    fragment_stage: Some(stage(&*prepared.fragment)),
                    primitive: wgt::PrimitiveState {
                        topology: wgt::PrimitiveTopology::TriangleStrip,
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
                    color_targets: &[Some(wgt::ColorTargetState {
                        format,
                        blend,
                        write_mask: if state.color_write {
                            wgt::ColorWrites::ALL
                        } else {
                            wgt::ColorWrites::empty()
                        },
                    })],
                    multiview_mask: None,
                    cache: None,
                })
        }
        .map_err(|error| {
            format!(
                "Creating pipeline {} {}: {error:?}",
                shader.name, shader.features
            )
        })?;
        Ok(Rc::new(Self {
            shader,
            _prepared: prepared,
            raw: Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_render_pipeline),
            layouts,
            format,
            has_depth,
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
        BlendMode::PremultipliedAlpha => Some(wgt::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
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
