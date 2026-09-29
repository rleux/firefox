/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::wgt;
use crate::device::{VertexAttributeKind, VertexDescriptor};
use webrender_build::vulkan::{ScalarType, ShaderArtifact};

pub fn vertex_layouts(
    descriptor: &VertexDescriptor,
    shader: &ShaderArtifact,
) -> Result<(Vec<wgt::VertexAttribute>, Vec<wgt::VertexAttribute>, u64), String> {
    let mut output = [Vec::new(), Vec::new()];
    let mut stride = 0;
    for (slot, attributes) in [descriptor.vertex_attributes, descriptor.instance_attributes]
        .iter()
        .enumerate()
    {
        let mut offset = 0;
        for attribute in *attributes {
            let (scalar, format, size) = match (&attribute.kind, attribute.count) {
                (VertexAttributeKind::U8Norm, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm8x2, 2)
                }
                (VertexAttributeKind::F32, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x2, 8)
                }
                (VertexAttributeKind::F32, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x4, 16)
                }
                (VertexAttributeKind::I32, 4) => {
                    (ScalarType::Sint, wgt::VertexFormat::Sint32x4, 16)
                }
                (VertexAttributeKind::F32, 1) => (ScalarType::Float, wgt::VertexFormat::Float32, 4),
                (VertexAttributeKind::F32, 3) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x3, 12)
                }
                (VertexAttributeKind::I32, 1) => (ScalarType::Sint, wgt::VertexFormat::Sint32, 4),
                (VertexAttributeKind::I32, 2) => (ScalarType::Sint, wgt::VertexFormat::Sint32x2, 8),
                (VertexAttributeKind::I32, 3) => {
                    (ScalarType::Sint, wgt::VertexFormat::Sint32x3, 12)
                }
                (VertexAttributeKind::U16, 2) => (ScalarType::Uint, wgt::VertexFormat::Uint16x2, 4),
                (VertexAttributeKind::U16, 4) => (ScalarType::Uint, wgt::VertexFormat::Uint16x4, 8),
                (VertexAttributeKind::U8Norm, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm8x4, 4)
                }
                (VertexAttributeKind::U16Norm, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm16x2, 4)
                }
                (VertexAttributeKind::U16Norm, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm16x4, 8)
                }
                _ => return Err(format!("Unsupported Vulkan vertex attribute {attribute:?}")),
            };
            if let Some(input) = shader
                .inputs
                .iter()
                .find(|input| input.name == attribute.name)
            {
                if input.scalar != scalar || input.components != attribute.count {
                    return Err(format!(
                        "Vulkan vertex interface mismatch for {}",
                        input.name
                    ));
                }
                output[slot].push(wgt::VertexAttribute {
                    format,
                    offset,
                    shader_location: input.location,
                });
            }
            offset += size;
        }
        if slot == 1 {
            stride = offset;
        }
    }
    if output[0].len() + output[1].len() != shader.inputs.len() {
        return Err("Missing Vulkan shader vertex attributes".into());
    }
    let [vertex, instances] = output;
    Ok((vertex, instances, stride))
}

#[cfg(test)]
#[path = "vertex_layout_tests.rs"]
mod tests;
