/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScalarType {
    Float,
    Sint,
    Uint,
}

#[derive(Debug)]
pub struct TextureBinding {
    pub name: &'static str,
    pub binding: u32,
    pub scalar: ScalarType,
    pub stages: u32,
    pub sampler_stages: u32,
}

#[derive(Debug)]
pub struct StorageBinding {
    pub name: &'static str,
    pub binding: u32,
    pub scalar: ScalarType,
    pub stages: u32,
}

#[derive(Debug)]
pub struct VertexInput {
    pub name: &'static str,
    pub location: u32,
    pub scalar: ScalarType,
    pub components: u32,
}

pub struct ShaderArtifact {
    pub name: &'static str,
    pub features: &'static str,
    pub buffer_tables: bool,
    pub vertex: &'static [u8],
    pub fragment: &'static [u8],
    pub inputs: &'static [VertexInput],
    pub textures: &'static [TextureBinding],
    pub storage_buffers: &'static [StorageBinding],
    pub projection_stages: u32,
    pub digest: u64,
}
