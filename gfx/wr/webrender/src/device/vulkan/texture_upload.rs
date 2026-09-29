/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{hal, wgt, SubmissionQueue};
use super::Texture;
use api::{
    ImageFormat,
    units::{DeviceIntRect, DeviceIntSize},
};
use std::convert::TryFrom;
use std::rc::Rc;
use wgpu_hal::CommandEncoder as _;

fn texture_format(format: ImageFormat) -> wgt::TextureFormat {
    match format {
        ImageFormat::RGBA8 => wgt::TextureFormat::Rgba8Unorm,
        ImageFormat::BGRA8 => wgt::TextureFormat::Bgra8Unorm,
        ImageFormat::R8 => wgt::TextureFormat::R8Unorm,
        ImageFormat::RG8 => wgt::TextureFormat::Rg8Unorm,
        ImageFormat::R16 => wgt::TextureFormat::R16Unorm,
        ImageFormat::RG16 => wgt::TextureFormat::Rg16Unorm,
        ImageFormat::RGBAF32 => wgt::TextureFormat::Rgba32Float,
        ImageFormat::RGBAI32 => wgt::TextureFormat::Rgba32Sint,
    }
}

impl Texture {
    pub fn upload(
        self: &Rc<Self>,
        queue: &SubmissionQueue,
        rect: DeviceIntRect,
        data: &[u8],
        stride: Option<i32>,
        offset: i32,
        source_format: Option<ImageFormat>,
    ) -> Result<(), String> {
        let mut commands = queue.recording()?;
        let owner = &self.raw.owner;
        let recording = commands.recording_id(owner)?;
        for state in self.states() {
            state.check_recording(&recording)?;
        }
        if rect.min.x < 0
            || rect.min.y < 0
            || rect.max.x <= rect.min.x
            || rect.max.y <= rect.min.y
            || rect.max.x as u32 > self.size.width
            || rect.max.y as u32 > self.size.height
            || offset < 0
        {
            return Err("Invalid Vulkan upload rectangle or offset".into());
        }
        let bpp = match self.format {
            wgt::TextureFormat::R8Unorm => 1,
            wgt::TextureFormat::Rg8Unorm | wgt::TextureFormat::R16Unorm => 2,
            wgt::TextureFormat::Rgba8Unorm
            | wgt::TextureFormat::Bgra8Unorm
            | wgt::TextureFormat::Rg16Unorm => 4,
            wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
            _ => return Err("Vulkan uploads require a color texture".into()),
        };
        let row_bytes = (rect.width() as usize)
            .checked_mul(bpp)
            .ok_or("Upload row overflow")?;
        let source_stride = match stride {
            Some(stride) => usize::try_from(stride).map_err(|_| "Invalid upload stride")?,
            None => row_bytes,
        };
        let end = source_stride
            .checked_mul(rect.height() as usize - 1)
            .and_then(|size| size.checked_add(offset as usize))
            .and_then(|size| size.checked_add(row_bytes))
            .ok_or("Upload source size overflow")?;
        if source_stride < row_bytes || end > data.len() {
            return Err("Upload source is too short".into());
        }
        let source_format = source_format.map(texture_format).unwrap_or(self.format);
        let swizzle = source_format != self.format;
        if swizzle
            && !matches!(
                (source_format, self.format),
                (
                    wgt::TextureFormat::Rgba8Unorm,
                    wgt::TextureFormat::Bgra8Unorm
                ) | (
                    wgt::TextureFormat::Bgra8Unorm,
                    wgt::TextureFormat::Rgba8Unorm
                )
            )
        {
            return Err("Unsupported Vulkan upload conversion".into());
        }
        let destination = if self.initialized() {
            rect
        } else {
            DeviceIntRect::from_size(DeviceIntSize::new(
                i32::try_from(self.size.width)
                    .map_err(|_| "Upload width exceeds coordinate range")?,
                i32::try_from(self.size.height)
                    .map_err(|_| "Upload height exceeds coordinate range")?,
            ))
        };
        let alignment = usize::try_from(owner.capabilities.alignments.buffer_copy_pitch.get())
            .map_err(|_| "Upload alignment exceeds address space")?;
        let pitch = (destination.width() as usize)
            .checked_mul(bpp)
            .and_then(|row| row.div_ceil(alignment).checked_mul(alignment))
            .ok_or("Upload pitch overflow")?;
        let pitch_u32 = u32::try_from(pitch).map_err(|_| "Upload pitch exceeds u32")?;
        let packed_size = pitch
            .checked_mul(destination.height() as usize)
            .ok_or("Upload size overflow")?;
        let staging = queue.upload_in_recording(
            &mut commands,
            packed_size,
            wgt::BufferUses::COPY_SRC,
            |packed| {
                if !swizzle
                    && destination == rect
                    && source_stride == row_bytes
                    && pitch == row_bytes
                {
                    packed.copy_from_slice(&data[offset as usize..end]);
                    return Ok(());
                }
                if destination != rect || pitch != row_bytes {
                    packed.fill(0);
                }
                for y in 0..rect.height() as usize {
                    let start = (y + (rect.min.y - destination.min.y) as usize) * pitch
                        + (rect.min.x - destination.min.x) as usize * bpp;
                    let source = offset as usize + y * source_stride;
                    let source = &data[source..source + row_bytes];
                    let target = &mut packed[start..start + row_bytes];
                    target.copy_from_slice(source);
                    if swizzle {
                        for pixel in target.chunks_exact_mut(4) {
                            pixel.swap(0, 2);
                        }
                    }
                }
                Ok(())
            },
        )?;
        staging.transition(&mut commands, wgt::BufferUses::COPY_SRC)?;
        self.transition(&mut commands, wgt::TextureUses::COPY_DST)?;
        unsafe {
            commands.encoder().copy_buffer_to_texture(
                &staging.raw,
                self.raw_texture(),
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch_u32),
                        rows_per_image: Some(destination.height() as u32),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: self.base_mip,
                        array_layer: 0,
                        origin: wgt::Origin3d {
                            x: destination.min.x as u32,
                            y: destination.min.y as u32,
                            z: 0,
                        },
                        aspect: hal::FormatAspects::COLOR,
                    },
                    size: wgt::Extent3d {
                        width: destination.width() as u32,
                        height: destination.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
        }
        self.transition(&mut commands, wgt::TextureUses::RESOURCE)?;
        self.initialize(&mut commands)
    }
}
