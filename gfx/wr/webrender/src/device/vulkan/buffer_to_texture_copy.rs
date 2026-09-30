/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{hal, wgt, Buffer, Recording};
use super::Texture;
use api::units::DeviceIntRect;
use std::rc::Rc;
use wgpu_hal::CommandEncoder as _;

impl Texture {
    pub fn copy_from_buffer(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        source: &Rc<Buffer>,
        rect: DeviceIntRect,
        offset: u64,
        stride: u32,
    ) -> Result<(), String> {
        let owner = &self.raw.owner;
        let recording = commands.recording_id(owner)?;
        commands.recording_id(&source.raw.owner)?;
        for state in self.states() {
            state.check_recording(&recording)?;
        }
        if rect.min.x < 0
            || rect.min.y < 0
            || rect.max.x <= rect.min.x
            || rect.max.y <= rect.min.y
            || rect.max.x as u32 > self.size.width
            || rect.max.y as u32 > self.size.height
        {
            return Err("Invalid Vulkan buffer-to-texture copy rectangle".into());
        }
        let bpp = match self.format {
            wgt::TextureFormat::R8Unorm => 1,
            wgt::TextureFormat::Rg8Unorm | wgt::TextureFormat::R16Unorm => 2,
            wgt::TextureFormat::Rgba8Unorm
            | wgt::TextureFormat::Bgra8Unorm
            | wgt::TextureFormat::Rg16Unorm => 4,
            wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
            _ => return Err("Vulkan buffer-to-texture copies require a color texture".into()),
        };
        let row_bytes = rect.width() as u64 * bpp;
        let end = u64::from(stride)
            .checked_mul(rect.height() as u64 - 1)
            .and_then(|size| size.checked_add(offset))
            .and_then(|size| size.checked_add(row_bytes))
            .ok_or("Vulkan buffer-to-texture copy size overflow")?;
        let alignments = &owner.capabilities.alignments;
        if !source.usage.contains(wgt::BufferUses::COPY_SRC)
            || u64::from(stride) < row_bytes
            || u64::from(stride) % bpp != 0
            || u64::from(stride) % alignments.buffer_copy_pitch.get() != 0
            || offset % bpp != 0
            || offset % alignments.buffer_copy_offset.get() != 0
            || end > source.binding_size()
        {
            return Err("Invalid Vulkan buffer-to-texture copy layout or source".into());
        }
        source.transition(commands, wgt::BufferUses::COPY_SRC)?;
        self.prepare_copy_destination(commands, rect)?;
        unsafe {
            commands.encoder().copy_buffer_to_texture(
                &source.raw,
                self.raw_texture(),
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset,
                        bytes_per_row: Some(stride),
                        rows_per_image: Some(rect.height() as u32),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: self.base_mip,
                        array_layer: 0,
                        origin: wgt::Origin3d {
                            x: rect.min.x as u32,
                            y: rect.min.y as u32,
                            z: 0,
                        },
                        aspect: hal::FormatAspects::COLOR,
                    },
                    size: wgt::Extent3d {
                        width: rect.width() as u32,
                        height: rect.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
        }
        self.transition(commands, wgt::TextureUses::RESOURCE)?;
        self.initialize(commands)
    }
}

#[cfg(test)]
#[path = "buffer_texture_copy_tests.rs"]
mod tests;
