/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{ImageFormat, MixBlendMode};
use crate::profiler::GpuProfileTag;
use webrender_build::shader::ShaderLogLine;

pub struct TextureSlot(pub usize);

#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub enum TextureFilter {
    Nearest,
    Linear,
    Trilinear,
}

/// A structure defining a particular workflow of texture transfers.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct TextureFormatPair<T> {
    /// Format the GPU natively stores texels in.
    pub internal: T,
    /// Format we expect the users to provide the texels in.
    pub external: T,
}

impl<T: Copy> From<T> for TextureFormatPair<T> {
    fn from(value: T) -> Self {
        TextureFormatPair {
            internal: value,
            external: value,
        }
    }
}

#[derive(Debug)]
pub enum VertexAttributeKind {
    F32,
    U8Norm,
    U16Norm,
    I32,
    U16,
}

#[derive(Debug)]
pub struct VertexAttribute {
    pub name: &'static str,
    pub count: u32,
    pub kind: VertexAttributeKind,
}

impl VertexAttribute {
    pub const fn quad_instance_vertex() -> Self {
        VertexAttribute {
            name: "aPosition",
            count: 2,
            kind: VertexAttributeKind::U8Norm,
        }
    }

    pub const fn gpu_buffer_address(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 1,
            kind: VertexAttributeKind::I32,
        }
    }

    pub const fn f32x4(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 4,
            kind: VertexAttributeKind::F32,
        }
    }

    pub const fn f32x3(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 3,
            kind: VertexAttributeKind::F32,
        }
    }

    pub const fn f32x2(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 2,
            kind: VertexAttributeKind::F32,
        }
    }

    pub const fn f32(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 1,
            kind: VertexAttributeKind::F32,
        }
    }

    pub const fn i32x4(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 4,
            kind: VertexAttributeKind::I32,
        }
    }

    pub const fn i32x2(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 2,
            kind: VertexAttributeKind::I32,
        }
    }

    pub const fn i32(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 1,
            kind: VertexAttributeKind::I32,
        }
    }

    pub const fn u16x2(name: &'static str) -> Self {
        VertexAttribute {
            name,
            count: 2,
            kind: VertexAttributeKind::U16,
        }
    }
}

#[derive(Debug)]
pub struct VertexDescriptor {
    pub vertex_attributes: &'static [VertexAttribute],
    pub instance_attributes: &'static [VertexAttribute],
}

/// Plain old data that can be used to initialize a texture.
pub unsafe trait Texel: Copy + Default {
    fn image_format() -> ImageFormat;
}

unsafe impl Texel for u8 {
    fn image_format() -> ImageFormat {
        ImageFormat::R8
    }
}

#[derive(Debug, Copy, Clone)]
pub enum VertexUsageHint {
    Static,
    Dynamic,
    Stream,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GraphicsApi {
    OpenGL,
}

/// How a draw is blended with the contents of the bound draw target.
#[derive(Debug, Copy, Clone, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub enum BlendMode {
    None,
    Alpha,
    PremultipliedAlpha,
    PremultipliedDestOut,
    /// Destination scaled by source, used to intersect clip masks.
    Multiply,
    SubpixelDualSource,
    Advanced(MixBlendMode),
    Screen,
    Exclusion,
    PlusLighter,
    /// Debug visualisation that accumulates overdraw.
    ShowOverdraw,
}

/// Describes the graphics API and driver a device is running on.
#[derive(Clone, Debug)]
pub struct GraphicsApiInfo {
    pub kind: GraphicsApi,
    pub renderer: String,
    pub version: String,
}

#[derive(Debug)]
pub struct Capabilities {
    /// Whether multisampled render targets are supported.
    pub supports_multisampling: bool,
    /// Whether the function `glCopyImageSubData` is available.
    pub supports_copy_image_sub_data: bool,
    /// Whether the device supports persistently mapped buffers, via glBufferStorage.
    pub supports_buffer_storage: bool,
    /// Whether advanced blend equations are supported.
    pub supports_advanced_blend_equation: bool,
    /// Whether advanced blend equations are coherent, meaning no barrier is
    /// required between overlapping draws.
    pub supports_advanced_blend_equation_coherent: bool,
    /// Whether dual-source blending is supported.
    pub supports_dual_source_blending: bool,
    /// Whether KHR_debug is supported for getting debug messages from
    /// the driver.
    pub supports_khr_debug: bool,
    /// Whether we can configure texture units to do swizzling on sampling.
    pub supports_texture_swizzle: bool,
    /// Whether the driver supports uploading to textures from a non-zero
    /// offset within a PBO.
    pub supports_nonzero_pbo_offsets: bool,
    /// Whether the driver supports specifying the texture usage up front.
    pub supports_texture_usage: bool,
    /// Whether offscreen render targets can be partially updated.
    pub supports_render_target_partial_update: bool,
    /// Whether we can use SSBOs.
    pub supports_shader_storage_object: bool,
    /// Whether to enforce that texture uploads be batched regardless of what
    /// the pref says.
    pub requires_batched_texture_uploads: Option<bool>,
    /// Whether we are able to ue glClear to clear regions of an alpha render target.
    /// If false, we must use a shader to clear instead.
    pub supports_alpha_target_clears: bool,
    /// Whether we must perform a full unscissored glClear on alpha targets
    /// prior to rendering.
    pub requires_alpha_target_full_clear: bool,
    /// Whether clearing a render target (immediately after binding it) is faster using a scissor
    /// rect to clear just the required area, or clearing the entire target without a scissor rect.
    pub prefers_clear_scissor: bool,
    /// Whether the driver can correctly invalidate render targets. This can be
    /// a worthwhile optimization, but is buggy on some devices.
    pub supports_render_target_invalidate: bool,
    /// Whether the driver can reliably upload data to R8 format textures.
    pub supports_r8_texture_upload: bool,
    /// Whether the extension QCOM_tiled_rendering is supported.
    pub supports_qcom_tiled_rendering: bool,
    /// Whether clip-masking is supported natively by the GL implementation
    /// rather than emulated in shaders.
    pub uses_native_clip_mask: bool,
    /// Whether anti-aliasing is supported natively by the GL implementation
    /// rather than emulated in shaders.
    pub uses_native_antialiasing: bool,
    /// Whether the extension GL_OES_EGL_image_external_essl3 is supported. If true, external
    /// textures can be used as normal. If false, external textures can only be rendered with
    /// certain shaders, and must first be copied in to regular textures for others.
    pub supports_image_external_essl3: bool,
    /// Whether rectangle textures (GL_TEXTURE_RECTANGLE) can be sampled.
    pub supports_texture_rect: bool,
    /// Whether external textures (GL_TEXTURE_EXTERNAL_OES) can be sampled.
    pub supports_texture_external: bool,
    /// Whether external textures can be sampled as BT.709 YUV, via GL_EXT_YUV_target.
    pub supports_texture_external_bt709: bool,
    /// Whether pixels read back from the default framebuffer arrive with the
    /// top row first.
    pub readback_rows_top_down: bool,
    /// Whether the VAO must be rebound after an attached VBO has been orphaned.
    pub requires_vao_rebind_after_orphaning: bool,
    /// Whether glReadPixels can read back BGRA directly (e.g. on GLES this
    /// requires GL_EXT_read_format_bgra). If false, callers must read RGBA
    /// instead and swap the red and blue channels themselves.
    pub supports_bgra_read: bool,
    /// Whether glDrawElementsInstancedBaseInstance and friends are supported,
    /// via ARB_base_instance (or GL 4.2) on desktop or EXT_base_instance on GLES.
    pub supports_base_instance: bool,
    /// The name of the renderer, as reported by GL
    pub renderer_name: String,
}

#[derive(Clone, Debug)]
pub enum ShaderError {
    /// Variant name, the driver's raw log, and the log parsed into per-line
    /// diagnostics with locations resolved back to the `.glsl` sources.
    Compilation(String, String, Vec<ShaderLogLine>),
    /// Variant name, the driver's raw log, and its parsed diagnostics. Link
    /// logs rarely carry locations, so the diagnostics are usually unmapped.
    Link(String, String, Vec<ShaderLogLine>),
}

impl ShaderError {
    pub fn name(&self) -> &str {
        match self {
            ShaderError::Compilation(name, ..) | ShaderError::Link(name, ..) => name,
        }
    }

    pub fn log(&self) -> &str {
        match self {
            ShaderError::Compilation(_, log, _) | ShaderError::Link(_, log, _) => log,
        }
    }

    pub fn diagnostics(&self) -> &[ShaderLogLine] {
        match self {
            ShaderError::Compilation(.., diagnostics) | ShaderError::Link(.., diagnostics) => {
                diagnostics
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct GpuTimer {
    pub tag: GpuProfileTag,
    pub time_ns: u64,
}

#[derive(Debug, Clone)]
pub struct GpuSampler {
    pub tag: GpuProfileTag,
    pub count: u64,
}
