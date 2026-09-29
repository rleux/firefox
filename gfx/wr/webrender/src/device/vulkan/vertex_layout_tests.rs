/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::VertexAttribute;
use crate::device::vulkan::shaders;
use crate::renderer::desc;
use webrender_build::vulkan::VertexInput;

#[test]
fn compiled_vertex_interfaces_match_wr_descriptors() {
    for shader in shaders::SHADERS {
        let descriptor = super::super::shader::draw_vertex_descriptor(shader).unwrap();
        vertex_layouts(descriptor, shader)
            .unwrap_or_else(|error| panic!("{} {}: {error}", shader.name, shader.features));
    }
}

#[test]
fn unused_attributes_keep_offsets_and_instance_stride() {
    const DESCRIPTOR: VertexDescriptor = VertexDescriptor {
        vertex_attributes: &[
            VertexAttribute::f32x4("unused_vertex"),
            VertexAttribute::quad_instance_vertex(),
        ],
        instance_attributes: &[
            VertexAttribute::f32x2("unused_prefix"),
            VertexAttribute::f32x4("color"),
            VertexAttribute::f32x2("unused_suffix"),
        ],
    };
    let shader = ShaderArtifact {
        inputs: &[
            VertexInput {
                name: "color",
                location: 1,
                scalar: ScalarType::Float,
                components: 4,
            },
            VertexInput {
                name: "aPosition",
                location: 7,
                scalar: ScalarType::Float,
                components: 2,
            },
        ],
        ..shaders::SHADERS[0]
    };
    let (vertex, instances, stride) = vertex_layouts(&DESCRIPTOR, &shader).unwrap();
    assert_eq!(
        vertex,
        [wgt::VertexAttribute {
            format: wgt::VertexFormat::Unorm8x2,
            offset: 16,
            shader_location: 7,
        }]
    );
    assert_eq!(
        instances,
        [wgt::VertexAttribute {
            format: wgt::VertexFormat::Float32x4,
            offset: 8,
            shader_location: 1,
        }]
    );
    assert_eq!(stride, 32);
}

#[test]
fn incompatible_inputs_are_rejected() {
    for (inputs, expected) in [
        (
            &[VertexInput {
                name: "aRect",
                location: 0,
                scalar: ScalarType::Sint,
                components: 4,
            }][..],
            "interface mismatch",
        ),
        (
            &[VertexInput {
                name: "aRect",
                location: 0,
                scalar: ScalarType::Float,
                components: 2,
            }][..],
            "interface mismatch",
        ),
        (
            &[VertexInput {
                name: "unknown",
                location: 0,
                scalar: ScalarType::Float,
                components: 4,
            }][..],
            "Missing",
        ),
    ] {
        let shader = ShaderArtifact {
            inputs,
            ..shaders::SHADERS[0]
        };
        let error = vertex_layouts(&desc::CLEAR, &shader).unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}
