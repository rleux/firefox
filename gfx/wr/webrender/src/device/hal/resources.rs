/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::cell::Cell;
use std::rc::Rc;
use api::{ImageFormat, units::DeviceIntRect};

pub(super) struct Owned<A: hal::Api, T> {
    owner: Rc<Device<A>>,
    raw: Option<T>,
    destroy: unsafe fn(&A::Device, T),
}

impl<A: hal::Api, T> Owned<A, T> {
    pub fn new(owner: &Rc<Device<A>>, raw: T, destroy: unsafe fn(&A::Device, T)) -> Self {
        Self {
            owner: owner.clone(),
            raw: Some(raw),
            destroy,
        }
    }
}
impl<A: hal::Api, T> Deref for Owned<A, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.raw.as_ref().unwrap()
    }
}
impl<A: hal::Api, T> Drop for Owned<A, T> {
    fn drop(&mut self) {
        unsafe { (self.destroy)(&self.owner.open.device, self.raw.take().unwrap()) }
    }
}

pub(super) struct Buffer<A: hal::Api> {
    pub raw: Owned<A, A::Buffer>,
    pub size: u64,
    state: Cell<wgt::BufferUses>,
}

impl<A: hal::Api> Buffer<A> {
    pub fn new(owner: &Rc<Device<A>>, bytes: &[u8], usage: wgt::BufferUses) -> Result<Self> {
        let size = (bytes.len() as u64).max(4);
        if size > owner.capabilities.limits.max_buffer_size {
            return Err("HAL buffer exceeds device limit".into());
        }
        let device = &owner.open.device;
        let raw = unsafe {
            device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR HAL buffer"),
                size,
                usage: usage | wgt::BufferUses::MAP_WRITE,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|e| format!("Creating buffer: {e:?}"))?;
        let raw = Owned::new(owner, raw, A::Device::destroy_buffer);
        unsafe {
            let mapping = device
                .map_buffer(&raw, 0..size)
                .map_err(|e| format!("Mapping upload: {e:?}"))?;
            std::ptr::write_bytes(mapping.ptr.as_ptr(), 0, size as usize);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.ptr.as_ptr(), bytes.len());
            if !mapping.is_coherent {
                device.flush_mapped_ranges(&raw, std::iter::once(0..size));
            }
            device.unmap_buffer(&raw);
        }
        Ok(Self {
            raw,
            size,
            state: Cell::new(wgt::BufferUses::MAP_WRITE),
        })
    }

    pub fn transition(&self, encoder: &mut A::CommandEncoder, to: wgt::BufferUses) {
        let from = self.state.replace(to);
        if from != to {
            unsafe {
                encoder.transition_buffers(std::iter::once(hal::BufferBarrier {
                    buffer: &*self.raw,
                    usage: hal::StateTransition { from, to },
                }))
            }
        }
    }
    pub fn binding(&self) -> hal::BufferBinding<'_, A::Buffer> {
        hal::BufferBinding::new_unchecked(&*self.raw, 0, std::num::NonZeroU64::new(self.size))
    }
}

pub(super) struct Texture<A: hal::Api> {
    pub view: Owned<A, A::TextureView>,
    pub target: Option<Owned<A, A::TextureView>>,
    pub raw: Owned<A, A::Texture>,
    pub size: wgt::Extent3d,
    pub format: wgt::TextureFormat,
    pub filter: crate::device::TextureFilter,
    pub state: Cell<wgt::TextureUses>,
}

pub(super) fn texture_format(format: ImageFormat) -> Result<wgt::TextureFormat> {
    Ok(match format {
        ImageFormat::RGBA8 => wgt::TextureFormat::Rgba8Unorm,
        ImageFormat::BGRA8 => wgt::TextureFormat::Bgra8Unorm,
        ImageFormat::R8 => wgt::TextureFormat::R8Unorm,
        ImageFormat::RGBAF32 => wgt::TextureFormat::Rgba32Float,
        ImageFormat::RGBAI32 => wgt::TextureFormat::Rgba32Sint,
        _ => return Err(format!("Unsupported HAL image format {format:?}")),
    })
}

pub(super) fn bytes_per_pixel(format: wgt::TextureFormat) -> usize {
    match format {
        wgt::TextureFormat::R8Unorm => 1,
        wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
        _ => 4,
    }
}

impl<A: hal::Api> Texture<A> {
    pub fn new(
        owner: &Rc<Device<A>>,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        filter: crate::device::TextureFilter,
        renderable: bool,
    ) -> Result<Rc<Self>> {
        owner.layout(width, height)?;
        if filter == crate::device::TextureFilter::Trilinear {
            return Err("HAL mipmaps are not implemented".into());
        }
        let depth = format == wgt::TextureFormat::Depth32Float;
        let target_usage = if depth {
            wgt::TextureUses::DEPTH_STENCIL_WRITE
        } else {
            wgt::TextureUses::COLOR_TARGET
        };
        let mut usage = wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
        if !depth {
            usage |= wgt::TextureUses::RESOURCE;
        }
        if renderable {
            usage |= target_usage;
        }
        let mut required =
            hal::TextureFormatCapabilities::COPY_SRC | hal::TextureFormatCapabilities::COPY_DST;
        if !depth {
            required |= hal::TextureFormatCapabilities::SAMPLED;
        }
        if renderable {
            required |= if depth {
                hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
            } else {
                hal::TextureFormatCapabilities::COLOR_ATTACHMENT
                    | hal::TextureFormatCapabilities::COLOR_ATTACHMENT_BLEND
            };
        }
        if filter == crate::device::TextureFilter::Linear {
            required |= hal::TextureFormatCapabilities::SAMPLED_LINEAR;
        }
        let caps = owner
            .formats
            .iter()
            .find(|(f, _)| *f == format)
            .ok_or("Unknown HAL texture format")?
            .1;
        if !caps.contains(required) {
            return Err(format!(
                "Unsupported HAL format usages {format:?}: {required:?}"
            ));
        }
        let size = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let device = &owner.open.device;
        let raw = unsafe { device.create_texture(&texture_descriptor(size, format, usage)) }
            .map_err(|e| format!("Creating {format:?} texture: {e:?}"))?;
        let raw = Owned::new(owner, raw, A::Device::destroy_texture);
        let view = |usage| -> Result<_> {
            let raw_view = unsafe {
                device.create_texture_view(
                    &raw,
                    &hal::TextureViewDescriptor {
                        label: Some("WR HAL view"),
                        format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange::default(),
                    },
                )
            }
            .map_err(|e| format!("Creating view: {e:?}"))?;
            Ok(Owned::new(owner, raw_view, A::Device::destroy_texture_view))
        };
        let sample_view = view(if depth {
            target_usage
        } else {
            wgt::TextureUses::RESOURCE
        })?;
        let target = if renderable {
            Some(view(target_usage)?)
        } else {
            None
        };
        Ok(Rc::new(Self {
            view: sample_view,
            target,
            raw,
            size,
            format,
            filter,
            state: Cell::new(wgt::TextureUses::UNINITIALIZED),
        }))
    }

    pub fn transition(&self, encoder: &mut A::CommandEncoder, to: wgt::TextureUses) {
        let from = self.state.replace(to);
        if from != to {
            unsafe { encoder.transition_textures(std::iter::once(barrier(&*self.raw, from, to))) }
        }
    }

    pub fn upload(
        &self,
        owner: &Rc<Device<A>>,
        rect: DeviceIntRect,
        data: &[u8],
        stride: Option<i32>,
        offset: i32,
        source_format: Option<ImageFormat>,
    ) -> Result<()> {
        if rect.min.x < 0
            || rect.min.y < 0
            || rect.width() <= 0
            || rect.height() <= 0
            || rect.max.x as u32 > self.size.width
            || rect.max.y as u32 > self.size.height
            || offset < 0
        {
            return Err("Invalid HAL upload rectangle/offset".into());
        }
        if !Rc::ptr_eq(&self.raw.owner, owner) {
            return Err("HAL upload device mismatch".into());
        }
        let bpp = bytes_per_pixel(self.format);
        let row_bytes = rect.width() as usize * bpp;
        let source_stride = usize::try_from(stride.unwrap_or(row_bytes as i32))
            .map_err(|_| "Invalid upload stride")?;
        let end = (offset as usize)
            .checked_add(
                source_stride
                    .checked_mul(rect.height() as usize - 1)
                    .ok_or("Upload size overflow")?,
            )
            .and_then(|n| n.checked_add(row_bytes))
            .ok_or("Upload size overflow")?;
        if source_stride < row_bytes || end > data.len() {
            return Err("Upload source is too short".into());
        }
        let source_format = source_format
            .map(texture_format)
            .transpose()?
            .unwrap_or(self.format);
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
            return Err("Unsupported HAL upload conversion".into());
        }
        let alignment = owner.capabilities.alignments.buffer_copy_pitch.get() as usize;
        let pitch = row_bytes.div_ceil(alignment) * alignment;
        let packed_size = pitch
            .checked_mul(rect.height() as usize)
            .ok_or("HAL upload size overflow")?;
        if packed_size as u64 > owner.capabilities.limits.max_buffer_size
            || packed_size > isize::MAX as usize
        {
            return Err("HAL upload exceeds buffer limits".into());
        }
        let mut packed = vec![0; packed_size];
        for y in 0..rect.height() as usize {
            let src = offset as usize + y * source_stride;
            let dst = &mut packed[y * pitch..y * pitch + row_bytes];
            dst.copy_from_slice(&data[src..src + row_bytes]);
            if swizzle {
                for pixel in dst.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }
        }
        let staging = Buffer::new(owner, &packed, wgt::BufferUses::COPY_SRC)?;
        let mut commands = Commands::<A>::new(&owner.open)?;
        staging.transition(commands.encoder(), wgt::BufferUses::COPY_SRC);
        self.transition(commands.encoder(), wgt::TextureUses::COPY_DST);
        unsafe {
            commands.encoder().copy_buffer_to_texture(
                &staging.raw,
                &self.raw,
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch as u32),
                        rows_per_image: Some(rect.height() as u32),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: 0,
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
        self.transition(commands.encoder(), wgt::TextureUses::RESOURCE);
        commands.submit_and_wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires Vulkan"]
    fn rejects_cross_device_and_invalid_uploads() {
        let first = Rc::new(create_vulkan_device(&Options::default()).unwrap());
        let second = Rc::new(create_vulkan_device(&Options::default()).unwrap());
        let texture = Texture::new(
            &first,
            2,
            2,
            wgt::TextureFormat::Rgba8Unorm,
            crate::device::TextureFilter::Nearest,
            false,
        )
        .unwrap();
        let rect = DeviceIntRect::from_size(api::units::DeviceIntSize::new(2, 2));
        assert!(texture
            .upload(&second, rect, &[0; 16], None, 0, None)
            .unwrap_err()
            .contains("device mismatch"));
        assert!(texture
            .upload(&first, rect, &[0; 15], None, 0, None)
            .is_err());
        assert!(texture
            .upload(&first, rect, &[0; 16], Some(4), 0, None)
            .is_err());
        assert!(texture
            .upload(&first, rect, &[0; 16], None, -1, None)
            .is_err());
        assert_eq!(texture.state.get(), wgt::TextureUses::UNINITIALIZED);
    }
}
