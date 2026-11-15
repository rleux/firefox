/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, wgt, Device, Failure};
use core::resource::ParentDevice;
use api::{units::DeviceIntRect, ImageFormat};
use std::convert::TryFrom;
use std::sync::{mpsc, Arc};

pub(super) fn texture_format(format: ImageFormat) -> wgt::TextureFormat {
    match format {
        ImageFormat::R8 => wgt::TextureFormat::R8Unorm,
        ImageFormat::RG8 => wgt::TextureFormat::Rg8Unorm,
        ImageFormat::R16 => wgt::TextureFormat::R16Unorm,
        ImageFormat::RG16 => wgt::TextureFormat::Rg16Unorm,
        ImageFormat::RGBA8 => wgt::TextureFormat::Rgba8Unorm,
        ImageFormat::BGRA8 => wgt::TextureFormat::Bgra8Unorm,
        ImageFormat::RGBAF32 => wgt::TextureFormat::Rgba32Float,
        ImageFormat::RGBAI32 => wgt::TextureFormat::Rgba32Sint,
    }
}

fn conversion(native: wgt::TextureFormat, other: Option<ImageFormat>) -> Result<bool, Failure> {
    let other = other.map(texture_format).unwrap_or(native);
    if native == other {
        return Ok(false);
    }
    if matches!(
        (native, other),
        (
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm
        ) | (
            wgt::TextureFormat::Bgra8Unorm,
            wgt::TextureFormat::Rgba8Unorm
        )
    ) {
        return Ok(true);
    }
    Err(Failure::validation(
        "Unsupported Vulkan transfer format conversion",
    ))
}

fn region(
    texture: &Arc<core::resource::Texture>,
    mip_level: u32,
    rect: DeviceIntRect,
) -> Result<
    (
        wgt::TexelCopyTextureInfo<Arc<core::resource::Texture>>,
        wgt::Extent3d,
        u32,
    ),
    Failure,
> {
    let desc = texture.descriptor();
    let size = desc
        .mip_level_size(mip_level)
        .ok_or_else(|| Failure::validation("Invalid Vulkan transfer mip level"))?;
    if desc.dimension != wgt::TextureDimension::D2
        || size.depth_or_array_layers != 1
        || desc.sample_count != 1
        || rect.min.x < 0
        || rect.min.y < 0
        || rect.max.x <= rect.min.x
        || rect.max.y <= rect.min.y
        || rect.max.x as u32 > size.width
        || rect.max.y as u32 > size.height
    {
        return Err(Failure::validation(
            "Invalid Vulkan transfer rectangle or texture",
        ));
    }
    let bpp = match desc.format {
        wgt::TextureFormat::R8Unorm => 1,
        wgt::TextureFormat::Rg8Unorm | wgt::TextureFormat::R16Unorm => 2,
        wgt::TextureFormat::Rgba8Unorm
        | wgt::TextureFormat::Bgra8Unorm
        | wgt::TextureFormat::Rg16Unorm => 4,
        wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
        _ => {
            return Err(Failure::validation(
                "Vulkan transfers require a color texture",
            ))
        }
    };
    Ok((
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level,
            origin: wgt::Origin3d {
                x: rect.min.x as u32,
                y: rect.min.y as u32,
                z: 0,
            },
            aspect: wgt::TextureAspect::All,
        },
        wgt::Extent3d {
            width: rect.width() as u32,
            height: rect.height() as u32,
            depth_or_array_layers: 1,
        },
        bpp,
    ))
}

struct Layout {
    row_bytes: u32,
    pitch: u32,
    height: u32,
    size: u64,
}

impl Layout {
    fn new(size: wgt::Extent3d, bpp: u32, limit: u64) -> Result<Self, Failure> {
        let row_bytes = size
            .width
            .checked_mul(bpp)
            .ok_or_else(|| Failure::validation("Vulkan transfer row overflow"))?;
        let alignment = wgt::COPY_BYTES_PER_ROW_ALIGNMENT;
        let pitch = row_bytes
            .checked_add(alignment - 1)
            .map(|row| row / alignment * alignment)
            .ok_or_else(|| Failure::validation("Vulkan transfer pitch overflow"))?;
        let length = u64::from(pitch) * u64::from(size.height);
        if row_bytes == 0 || size.height == 0 || length > limit || length > isize::MAX as u64 {
            return Err(Failure::validation(
                "Vulkan transfer exceeds buffer or address-space limits",
            ));
        }
        Ok(Self {
            row_bytes,
            pitch,
            height: size.height,
            size: length,
        })
    }

    fn pixel_length(&self) -> usize {
        self.row_bytes as usize * self.height as usize
    }
}

impl Device {
    /// Create an immutable snapshot; padding belongs only to this new allocation.
    pub fn create_buffer_with_data(
        &self,
        label: &str,
        data: &[u8],
        usage: wgt::BufferUsages,
    ) -> Result<Arc<core::resource::Buffer>, Failure> {
        self.check()?;
        let size = (data.len() as u64)
            .checked_add(wgt::COPY_BUFFER_ALIGNMENT - 1)
            .map(|size| size / wgt::COPY_BUFFER_ALIGNMENT * wgt::COPY_BUFFER_ALIGNMENT)
            .filter(|size| *size <= self.limits().max_buffer_size && *size <= isize::MAX as u64)
            .ok_or_else(|| Failure::validation("Vulkan buffer upload exceeds limits"))?
            .max(wgt::COPY_BUFFER_ALIGNMENT);
        let usage = if !usage.is_empty()
            && self
                .features()
                .contains(wgt::Features::MAPPABLE_PRIMARY_BUFFERS)
        {
            usage | wgt::BufferUsages::MAP_WRITE
        } else {
            usage
        };
        let buffer = self.create_buffer(&core::resource::BufferDescriptor {
            label: Some(label.into()),
            size,
            usage,
            mapped_at_creation: true,
        })?;
        {
            let mapping = buffer
                .get_mapped_range(0, Some(size))
                .map_err(Failure::from_error)?;
            let mut output = mapping.write_slice();
            output.slice(..data.len()).copy_from_slice(data);
            output
                .slice(data.len()..)
                .write_iter(std::iter::repeat(0).take(size as usize - data.len()));
        }
        buffer.unmap();
        self.check()?;
        Ok(buffer)
    }

    /// Encode uploads at their position among draws and copies, so dropping commands cancels them.
    pub fn upload_texture(
        &self,
        encoder: &Arc<core::command::CommandEncoder>,
        texture: &Arc<core::resource::Texture>,
        mip_level: u32,
        rect: DeviceIntRect,
        data: &[u8],
        stride: Option<i32>,
        offset: i32,
        source_format: Option<ImageFormat>,
    ) -> Result<(), Failure> {
        self.check()?;
        encoder
            .same_device(&self.device)
            .map_err(Failure::from_error)?;
        texture
            .same_device(&self.device)
            .map_err(Failure::from_error)?;
        let (target, size, bpp) = region(texture, mip_level, rect)?;
        let layout = Layout::new(size, bpp, self.limits().max_buffer_size)?;
        let row_bytes = layout.row_bytes as usize;
        let stride = stride
            .map_or(Ok(row_bytes), usize::try_from)
            .map_err(|_| Failure::validation("Invalid Vulkan upload stride"))?;
        let offset = usize::try_from(offset)
            .map_err(|_| Failure::validation("Invalid Vulkan upload offset"))?;
        let end = stride
            .checked_mul(size.height as usize - 1)
            .and_then(|length| length.checked_add(offset))
            .and_then(|length| length.checked_add(row_bytes))
            .ok_or_else(|| Failure::validation("Vulkan upload source overflow"))?;
        if stride < row_bytes || end > data.len() {
            return Err(Failure::validation("Vulkan upload source is too short"));
        }
        let swizzle = conversion(texture.descriptor().format, source_format)?;
        let staging = self.create_buffer(&core::resource::BufferDescriptor {
            label: Some("WR texture upload".into()),
            size: layout.size,
            usage: wgt::BufferUsages::MAP_WRITE | wgt::BufferUsages::COPY_SRC,
            mapped_at_creation: true,
        })?;
        {
            let mapping = staging
                .get_mapped_range(0, Some(layout.size))
                .map_err(Failure::from_error)?;
            let mut output = mapping.write_slice();
            for y in 0..size.height as usize {
                let source = &data[offset + y * stride..][..row_bytes];
                let start = y * layout.pitch as usize;
                let mut row = output.slice(start..start + row_bytes);
                if swizzle {
                    row.write_iter(
                        source
                            .chunks_exact(4)
                            .flat_map(|p| [p[2], p[1], p[0], p[3]]),
                    );
                } else {
                    row.copy_from_slice(source);
                }
            }
        }
        // MAP_WRITE maps this core buffer directly; unmap flushes without an Init staging copy.
        staging.unmap();
        encoder.copy_buffer_to_texture(
            &wgt::TexelCopyBufferInfo {
                buffer: staging,
                layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(layout.pitch),
                    rows_per_image: None,
                },
            },
            &target,
            &size,
        );
        self.check()
    }

    /// Record into the caller's encoder; map only after submitting that encoder.
    pub fn encode_readback(
        &self,
        encoder: &Arc<core::command::CommandEncoder>,
        texture: &Arc<core::resource::Texture>,
        mip_level: u32,
        rect: DeviceIntRect,
        output_format: Option<ImageFormat>,
    ) -> Result<Readback, Failure> {
        self.check()?;
        encoder
            .same_device(&self.device)
            .map_err(Failure::from_error)?;
        texture
            .same_device(&self.device)
            .map_err(Failure::from_error)?;
        let (source, size, bpp) = region(texture, mip_level, rect)?;
        let layout = Layout::new(size, bpp, self.limits().max_buffer_size)?;
        let swizzle = conversion(texture.descriptor().format, output_format)?;
        let buffer = self.create_buffer(&core::resource::BufferDescriptor {
            label: Some("WR texture readback".into()),
            size: layout.size,
            usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })?;
        encoder.copy_texture_to_buffer(
            &source,
            &wgt::TexelCopyBufferInfo {
                buffer: buffer.clone(),
                layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(layout.pitch),
                    rows_per_image: None,
                },
            },
            &size,
        );
        self.check()?;
        Ok(Readback {
            buffer,
            layout,
            swizzle,
        })
    }
}

pub struct Readback {
    buffer: Arc<core::resource::Buffer>,
    layout: Layout,
    swizzle: bool,
}

pub struct PendingReadback {
    readback: Readback,
    submission: core::SubmissionIndex,
    callback: mpsc::Receiver<Result<(), Failure>>,
    status: Option<Result<(), Failure>>,
}

impl Readback {
    /// The submission must include the encoder which recorded this readback.
    pub fn map(
        self,
        device: &Device,
        submission: core::SubmissionIndex,
    ) -> Result<PendingReadback, Failure> {
        let _queue_access = device.queue_access();
        device.check()?;
        self.buffer
            .same_device(&device.device)
            .map_err(Failure::from_error)?;
        let (tx, callback) = mpsc::channel();
        self.buffer.map_async(
            wgt::MapMode::Read,
            0,
            Some(self.layout.size),
            Some(Box::new(move |result| {
                let _ = tx.send(result.map(|_| ()).map_err(Failure::from_error));
            })),
        );
        let pending = PendingReadback {
            readback: self,
            submission,
            callback,
            status: None,
        };
        device.check()?;
        Ok(pending)
    }
}

impl PendingReadback {
    fn ready(&mut self) -> Result<bool, Failure> {
        if self.status.is_none() {
            match self.callback.try_recv() {
                Ok(status) => self.status = Some(status),
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Failure {
                        kind: wgt::error::ErrorType::Internal,
                        message: "Vulkan readback mapping callback was lost".into(),
                    })
                }
            }
        }
        self.status.as_ref().unwrap().clone().map(|()| true)
    }

    fn copy_pixels(&self, pixels: &mut [u8]) -> Result<(), Failure> {
        let Readback {
            buffer,
            layout,
            swizzle,
        } = &self.readback;
        let mapping = buffer
            .get_mapped_range(0, Some(layout.size))
            .map_err(Failure::from_error)?;
        for (source, target) in mapping
            .read_slice()
            .chunks_exact(layout.pitch as usize)
            .zip(pixels.chunks_exact_mut(layout.row_bytes as usize))
        {
            target.copy_from_slice(&source[..layout.row_bytes as usize]);
        }
        if *swizzle {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        Ok(())
    }

    pub fn poll(&mut self, device: &Device) -> Result<Option<Vec<u8>>, Failure> {
        self.readback
            .buffer
            .same_device(&device.device)
            .map_err(Failure::from_error)?;
        device.poll()?;
        if !self.ready()? {
            return Ok(None);
        }
        let mut pixels = vec![0; self.readback.layout.pixel_length()];
        self.copy_pixels(&mut pixels)?;
        Ok(Some(pixels))
    }

    pub fn wait(&mut self, device: &Device) -> Result<Vec<u8>, Failure> {
        let mut pixels = vec![0; self.readback.layout.pixel_length()];
        self.wait_into(device, &mut pixels)?;
        Ok(pixels)
    }

    pub fn wait_into(&mut self, device: &Device, pixels: &mut [u8]) -> Result<(), Failure> {
        if pixels.len() != self.readback.layout.pixel_length() {
            return Err(Failure::validation(
                "Vulkan readback output size does not match the rectangle",
            ));
        }
        self.readback
            .buffer
            .same_device(&device.device)
            .map_err(Failure::from_error)?;
        device.wait_for(self.submission)?;
        // Another poll may have collected the callback without delivering it yet.
        if self.status.is_none() {
            self.status = Some(self.callback.recv().map_err(|_| Failure {
                kind: wgt::error::ErrorType::Internal,
                message: "Vulkan readback mapping callback was lost".into(),
            })?);
        }
        self.status.as_ref().unwrap().clone()?;
        self.copy_pixels(pixels)
    }
}

impl Drop for PendingReadback {
    fn drop(&mut self) {
        self.readback.buffer.unmap();
    }
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod tests;
