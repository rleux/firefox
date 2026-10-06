/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Device};
use api::ImageFormat;
use crate::device::{
    Capabilities, GraphicsApi, GraphicsApiInfo, TextureFormatPair, UploadMethod, VertexUsageHint,
};
use std::convert::TryFrom;
use webrender_build::shader::ShaderFeatureFlags;

pub(super) const MAX_DEPTH_IDS: i32 = 1 << 22;

pub(super) struct RendererProperties {
    pub capabilities: Capabilities,
    pub api_info: GraphicsApiInfo,
    pub max_texture_size: i32,
    pub upload_method: UploadMethod,
    pub color_formats: TextureFormatPair<ImageFormat>,
}

impl RendererProperties {
    pub fn new(owner: &Device) -> Self {
        let info = owner.info();
        Self {
            capabilities: Capabilities {
                supports_multisampling: false,
                supports_persistent_upload_buffers: true,
                supports_advanced_blend_equation: false,
                supports_advanced_blend_equation_coherent: false,
                supports_dual_source_blending: owner
                    .features()
                    .contains(wgt::Features::DUAL_SOURCE_BLENDING),
                supports_upload_buffer_offsets: true,
                supports_render_target_partial_update: true,
                supports_shader_storage_object: false,
                supports_alpha_target_clears: true,
                requires_alpha_target_full_clear: false,
                prefers_clear_scissor: false,
                supports_r8_texture_upload: true,
                uses_native_clip_mask: false,
                uses_native_antialiasing: false,
                supports_external_textures_in_all_shaders: false,
                supports_texture_rect: true,
                supports_texture_external: false,
                supports_texture_external_bt709: false,
                readback_rows_top_down: true,
                supports_bgra_read: true,
                supports_base_instance: true,
                renderer_name: info.name.clone(),
            },
            api_info: GraphicsApiInfo {
                kind: GraphicsApi::Vulkan,
                renderer: info.name.clone(),
                version: info.driver_info.clone(),
            },
            max_texture_size: i32::try_from(owner.capabilities().limits.max_texture_dimension_2d)
                .unwrap_or(i32::MAX),
            upload_method: UploadMethod::PixelBuffer(VertexUsageHint::Stream),
            color_formats: ImageFormat::BGRA8.into(),
        }
    }

    pub fn shader_feature_flags(&self) -> ShaderFeatureFlags {
        ShaderFeatureFlags::GL
    }

    pub fn max_depth_ids(&self) -> i32 {
        MAX_DEPTH_IDS
    }

    pub fn ortho_near_plane(&self) -> f32 {
        -(MAX_DEPTH_IDS as f32)
    }

    pub fn ortho_far_plane(&self) -> f32 {
        (MAX_DEPTH_IDS - 1) as f32
    }

    pub fn surface_origin_is_top_left(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "renderer_property_tests.rs"]
mod tests;
