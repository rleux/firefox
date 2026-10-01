/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Device};

#[derive(Clone, Copy, Debug)]
pub struct SurfaceOptions {
    pub vsync: bool,
    pub transparent: bool,
}

impl Default for SurfaceOptions {
    fn default() -> Self {
        Self {
            vsync: true,
            transparent: false,
        }
    }
}

impl Device {
    pub(super) fn surface_configuration(
        &self,
        caps: &hal::SurfaceCapabilities,
        size: [u32; 2],
        options: SurfaceOptions,
    ) -> Result<hal::SurfaceConfiguration, String> {
        negotiate(
            caps,
            size,
            options,
            self.capabilities.limits.max_texture_dimension_2d,
        )
    }
}

fn negotiate(
    caps: &hal::SurfaceCapabilities,
    size: [u32; 2],
    options: SurfaceOptions,
    limit: u32,
) -> Result<hal::SurfaceConfiguration, String> {
    if size.contains(&0) || size.iter().any(|&size| size > limit) {
        return Err("Invalid Vulkan surface dimensions".into());
    }
    if !caps.usage.contains(wgt::TextureUses::COLOR_TARGET) {
        return Err("Vulkan surface does not support direct rendering".into());
    }
    let format = [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ]
    .iter()
    .copied()
    .find(|&format| {
        caps.formats.iter().any(|entry| {
            entry.format == format && entry.color_spaces.contains(wgt::SurfaceColorSpaces::SRGB)
        })
    })
    .ok_or("Direct Vulkan rendering requires a UNORM surface in the sRGB color space")?;
    let alpha = if options.transparent {
        if caps
            .composite_alpha_modes
            .contains(&wgt::CompositeAlphaMode::PreMultiplied)
        {
            wgt::CompositeAlphaMode::PreMultiplied
        } else {
            wgt::CompositeAlphaMode::Inherit
        }
    } else {
        wgt::CompositeAlphaMode::Opaque
    };
    if !caps.composite_alpha_modes.contains(&alpha) {
        return Err("Requested Vulkan surface alpha mode is unsupported".into());
    }
    let preferred = if options.vsync {
        wgt::PresentMode::Fifo
    } else {
        wgt::PresentMode::Immediate
    };
    let present_mode = if caps.present_modes.contains(&preferred) {
        preferred
    } else {
        wgt::PresentMode::Fifo
    };
    if !caps.present_modes.contains(&present_mode) {
        return Err("Vulkan surface has no supported present mode".into());
    }
    let extent = caps.current_extent.unwrap_or(wgt::Extent3d {
        width: size[0],
        height: size[1],
        depth_or_array_layers: 1,
    });
    if extent.width == 0
        || extent.height == 0
        || extent.width > limit
        || extent.height > limit
        || extent.depth_or_array_layers != 1
    {
        return Err("Vulkan surface extent is unavailable or exceeds limits".into());
    }
    Ok(hal::SurfaceConfiguration {
        maximum_frame_latency: 2u32.clamp(
            *caps.maximum_frame_latency.start(),
            *caps.maximum_frame_latency.end(),
        ),
        present_mode,
        composite_alpha_mode: alpha,
        format,
        color_space: wgt::SurfaceColorSpace::Srgb,
        extent,
        usage: wgt::TextureUses::COLOR_TARGET,
        view_formats: Vec::new(),
    })
}

#[cfg(test)]
#[path = "surface_config_tests.rs"]
mod tests;
