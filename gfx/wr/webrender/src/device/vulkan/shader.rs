/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::device::VertexDescriptor;
use crate::renderer::desc;
use webrender_build::vulkan::ShaderArtifact;

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
