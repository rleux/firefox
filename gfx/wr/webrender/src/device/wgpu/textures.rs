/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, wgt, Device};
use crate::device::TextureFilter;
use std::rc::Rc;

pub struct Texture {
    view: Owned<dyn hal::DynTextureView>,
    target: Option<Owned<dyn hal::DynTextureView>>,
    pub(super) raw: Rc<Owned<dyn hal::DynTexture>>,
    size: wgt::Extent3d,
    format: wgt::TextureFormat,
    filter: TextureFilter,
    mip_count: u32,
}

pub(super) fn supports_float_color_format(
    format: wgt::TextureFormat,
    features: wgt::Features,
    capabilities: hal::TextureFormatCapabilities,
    required: hal::TextureFormatCapabilities,
) -> bool {
    features.contains(format.required_features())
        && matches!(
            format.sample_type(None, Some(features)),
            Some(wgt::TextureSampleType::Float { .. })
        )
        && capabilities.contains(required)
}

impl Texture {
    pub fn new(
        owner: &Rc<Device>,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        filter: TextureFilter,
        renderable: bool,
    ) -> Result<Rc<Self>, String> {
        if width == 0
            || height == 0
            || width > owner.capabilities.limits.max_texture_dimension_2d
            || height > owner.capabilities.limits.max_texture_dimension_2d
        {
            return Err("Invalid Vulkan texture dimensions".into());
        }
        match format {
            wgt::TextureFormat::Rgba8Unorm
            | wgt::TextureFormat::Bgra8Unorm
            | wgt::TextureFormat::R8Unorm
            | wgt::TextureFormat::Rg8Unorm
            | wgt::TextureFormat::R16Unorm
            | wgt::TextureFormat::Rg16Unorm
            | wgt::TextureFormat::Rgba32Float
            | wgt::TextureFormat::Rgba32Sint
            | wgt::TextureFormat::Depth32Float => {}
            _ => return Err(format!("Unsupported Vulkan texture format: {format:?}")),
        }
        let depth = format == wgt::TextureFormat::Depth32Float;
        if depth && (!renderable || filter != TextureFilter::Nearest) {
            return Err("Depth textures require a nearest-filtered render target".into());
        }
        let mip_count = if filter == TextureFilter::Trilinear {
            32 - width.max(height).leading_zeros()
        } else {
            1
        };
        let renderable = renderable || mip_count > 1;
        let target_usage = if depth {
            wgt::TextureUses::DEPTH_WRITE
        } else {
            wgt::TextureUses::COLOR_TARGET
        };
        let mut usage = wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
        let mut required =
            hal::TextureFormatCapabilities::COPY_SRC | hal::TextureFormatCapabilities::COPY_DST;
        if !depth {
            usage |= wgt::TextureUses::RESOURCE;
            required |= hal::TextureFormatCapabilities::SAMPLED;
        }
        if renderable {
            usage |= target_usage;
            required |= if depth {
                hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
            } else {
                hal::TextureFormatCapabilities::COLOR_ATTACHMENT
            };
        }
        if filter != TextureFilter::Nearest {
            required |= hal::TextureFormatCapabilities::SAMPLED_LINEAR;
        }
        let capabilities = unsafe { owner.adapter.texture_format_capabilities(format) };
        let supported = if filter == TextureFilter::Trilinear {
            supports_float_color_format(format, owner.features, capabilities, required)
        } else {
            owner.features.contains(format.required_features()) && capabilities.contains(required)
        };
        if !supported {
            return Err(format!(
                "Unsupported Vulkan format usages {format:?}: {required:?}"
            ));
        }
        let size = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let device = owner.open.device.as_ref();
        let raw = unsafe {
            device.create_texture(&hal::TextureDescriptor {
                label: Some("WR Vulkan texture"),
                size,
                mip_level_count: mip_count,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage,
                memory_flags: hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            })
        }
        .map_err(|error| format!("Creating {format:?} texture: {error:?}"))?;
        let raw = Rc::new(Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_texture));
        let make_view = |usage, levels| -> Result<_, String> {
            let view = unsafe {
                device.create_texture_view(
                    &**raw,
                    &hal::TextureViewDescriptor {
                        label: Some("WR Vulkan texture view"),
                        swizzle: Default::default(),
                        format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange {
                            mip_level_count: Some(levels),
                            array_layer_count: Some(1),
                            ..Default::default()
                        },
                    },
                )
            }
            .map_err(|error| format!("Creating texture view: {error:?}"))?;
            Ok(Owned::new(
                owner,
                view,
                <dyn hal::DynDevice>::destroy_texture_view,
            ))
        };
        let view = make_view(
            if depth {
                target_usage
            } else {
                wgt::TextureUses::RESOURCE
            },
            mip_count,
        )?;
        let target = if renderable {
            Some(make_view(target_usage, 1)?)
        } else {
            None
        };
        Ok(Rc::new(Self {
            view,
            target,
            raw,
            size,
            format,
            filter,
            mip_count,
        }))
    }

    pub fn view(&self) -> &dyn hal::DynTextureView {
        &*self.view
    }

    pub fn raw_texture(&self) -> &dyn hal::DynTexture {
        &**self.raw
    }

    pub fn target_view(&self) -> Option<&dyn hal::DynTextureView> {
        self.target.as_deref()
    }

    pub fn size(&self) -> wgt::Extent3d {
        self.size
    }

    pub fn format(&self) -> wgt::TextureFormat {
        self.format
    }

    pub fn filter(&self) -> TextureFilter {
        self.filter
    }

    pub fn mip_count(&self) -> u32 {
        self.mip_count
    }
}
