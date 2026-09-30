/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::internal_types::FastHashMap;
use super::texture_store::TextureStore;
use super::{wgt, Buffer, BufferPool, SubmissionQueue};
use api::{ImageFormat, units::DeviceIntSize};
use crate::device::{TransferBuffer, UploadBufferMapping, UploadChunk};
use std::{convert::TryFrom, mem::MaybeUninit, ptr::NonNull, rc::Rc};

struct Entry {
    size: usize,
    buffer: Option<Rc<Buffer>>,
    mapping: UploadBufferMapping,
}

/// Persistent mappings may be written again only after their upload fence signals.
pub(super) struct UploadBuffers {
    pool: Rc<BufferPool>,
    last_id: u32,
    entries: FastHashMap<u32, Entry>,
}

fn align(size: usize, alignment: usize) -> Result<usize, String> {
    size.checked_add(alignment - 1)
        .map(|n| n / alignment * alignment)
        .ok_or_else(|| "Vulkan upload layout overflow".into())
}

fn upload_layout(
    size: DeviceIntSize,
    format: ImageFormat,
    copy_pitch: u64,
    copy_offset: u64,
    max_buffer_size: u64,
) -> Result<(usize, usize), String> {
    if size.width <= 0 || size.height <= 0 {
        return Err("Invalid Vulkan upload dimensions".into());
    }
    let bpp = format.bytes_per_pixel() as usize;
    let pitch_alignment = usize::try_from(copy_pitch)
        .map_err(|_| "Vulkan pitch alignment exceeds address space")?
        .max(bpp);
    let offset_alignment = usize::try_from(copy_offset)
        .map_err(|_| "Vulkan offset alignment exceeds address space")?
        // Chunks of different formats share the same upload buffer.
        .max(ImageFormat::RGBAF32.bytes_per_pixel() as usize);
    let row = (size.width as usize)
        .checked_mul(bpp)
        .ok_or("Vulkan upload row overflow")?;
    let stride = align(row, pitch_alignment)?;
    let length = stride
        .checked_mul(size.height as usize)
        .ok_or("Vulkan upload size overflow")?;
    let length = align(length, offset_alignment)?;
    if stride > i32::MAX as usize || length > isize::MAX as usize || length as u64 > max_buffer_size
    {
        return Err("Vulkan upload layout exceeds buffer limits".into());
    }
    Ok((length, stride))
}

impl UploadBuffers {
    pub fn new(pool: &Rc<BufferPool>) -> Self {
        Self {
            pool: pool.clone(),
            last_id: 0,
            entries: FastHashMap::default(),
        }
    }

    pub fn layout(
        &self,
        size: DeviceIntSize,
        format: ImageFormat,
    ) -> Result<(usize, usize), String> {
        let capabilities = &self.pool.owner.capabilities;
        upload_layout(
            size,
            format,
            capabilities.alignments.buffer_copy_pitch.get(),
            capabilities.alignments.buffer_copy_offset.get(),
            capabilities.limits.max_buffer_size,
        )
    }

    pub fn create(&mut self) -> Result<TransferBuffer, String> {
        let id = self
            .last_id
            .checked_add(1)
            .ok_or("Vulkan upload identifier space exhausted")?;
        self.entries.insert(
            id,
            Entry {
                size: 0,
                buffer: None,
                mapping: UploadBufferMapping::Unmapped,
            },
        );
        self.last_id = id;
        Ok(TransferBuffer {
            id,
            reserved_size: 0,
        })
    }

    fn entry(&mut self, handle: &TransferBuffer) -> Result<&mut Entry, String> {
        self.entries
            .get_mut(&handle.id)
            .filter(|entry| entry.size == handle.reserved_size)
            .ok_or_else(|| "Invalid Vulkan upload handle".into())
    }

    pub fn allocate(
        &mut self,
        handle: &mut TransferBuffer,
        size: usize,
        persistent: bool,
    ) -> Result<UploadBufferMapping, String> {
        self.entry(handle)?;
        let mut buffer = self
            .pool
            .upload_with(size, wgt::BufferUses::COPY_SRC, |_| Ok(()))?;
        let pointer = Rc::get_mut(&mut buffer).unwrap().mapped_write_ptr();
        let mapping = || {
            if persistent {
                UploadBufferMapping::Persistent(pointer)
            } else {
                UploadBufferMapping::Transient(pointer)
            }
        };
        let entry = self.entry(handle)?;
        let previous = entry.buffer.replace(buffer);
        entry.size = size;
        entry.mapping = mapping();
        handle.reserved_size = size;
        if let Some(previous) = previous {
            self.pool.recycle(previous);
        }
        Ok(mapping())
    }

    pub fn map(&mut self, handle: &TransferBuffer) -> Result<NonNull<MaybeUninit<u8>>, String> {
        let entry = self.entry(handle)?;
        if !matches!(entry.mapping, UploadBufferMapping::Unmapped) {
            return Err("Vulkan upload buffer is already mapped".into());
        }
        let buffer = entry
            .buffer
            .as_mut()
            .and_then(Rc::get_mut)
            .ok_or("Vulkan upload buffer is unallocated or still in use")?;
        let pointer = buffer.mapped_write_ptr();
        entry.mapping = UploadBufferMapping::Transient(pointer);
        Ok(pointer)
    }

    pub fn flush(
        &mut self,
        handle: &TransferBuffer,
        mapping: &UploadBufferMapping,
        size_used: usize,
        chunks: &[UploadChunk<'_>],
        textures: &TextureStore,
        queue: &SubmissionQueue,
    ) -> Result<(), String> {
        queue.recording()?.recording_id(&self.pool.owner)?;
        let entry = self.entry(handle)?;
        let matches = match (&entry.mapping, mapping) {
            (UploadBufferMapping::Persistent(a), UploadBufferMapping::Persistent(b))
            | (UploadBufferMapping::Transient(a), UploadBufferMapping::Transient(b)) => a == b,
            _ => false,
        };
        if !matches || size_used > entry.size {
            return Err("Invalid Vulkan upload mapping or flush size".into());
        }
        let buffer = entry
            .buffer
            .as_mut()
            .and_then(Rc::get_mut)
            .ok_or("Vulkan upload buffer is still in use")?;
        buffer.flush_writes(size_used)?;
        if matches!(entry.mapping, UploadBufferMapping::Transient(_)) {
            entry.mapping = UploadBufferMapping::Unmapped;
        }
        let source = entry.buffer.as_ref().unwrap();
        for chunk in chunks {
            let image = textures.image(chunk.texture)?;
            let format = chunk.format_override.unwrap_or(chunk.texture.format);
            if chunk.rect.min.x < 0
                || chunk.rect.min.y < 0
                || chunk.rect.max.x <= chunk.rect.min.x
                || chunk.rect.max.y <= chunk.rect.min.y
            {
                return Err("Invalid Vulkan upload chunk rectangle".into());
            }
            let row = (chunk.rect.width() as usize)
                .checked_mul(format.bytes_per_pixel() as usize)
                .ok_or("Vulkan upload row overflow")?;
            let stride = match chunk.stride {
                Some(stride) => {
                    usize::try_from(stride).map_err(|_| "Invalid Vulkan upload stride")?
                }
                None => row,
            };
            let end = stride
                .checked_mul(chunk.rect.height() as usize - 1)
                .and_then(|size| size.checked_add(chunk.offset))
                .and_then(|size| size.checked_add(row))
                .ok_or("Vulkan upload chunk size overflow")?;
            if stride < row || end > size_used {
                return Err("Vulkan upload chunk exceeds written bytes".into());
            }
            if format == chunk.texture.format {
                image.copy_from_buffer(
                    &mut queue.recording()?,
                    source,
                    chunk.rect,
                    chunk.offset as u64,
                    u32::try_from(stride).map_err(|_| "Vulkan upload stride exceeds u32")?,
                )?;
            } else {
                image.upload(
                    queue,
                    chunk.rect,
                    &source.mapped_read_only()?[chunk.offset..size_used],
                    chunk.stride,
                    0,
                    Some(format),
                )?;
            }
        }
        Ok(())
    }

    pub fn orphan(&mut self, handle: &mut TransferBuffer) -> Result<(), String> {
        let entry = self.entry(handle)?;
        let buffer = entry.buffer.take();
        entry.size = 0;
        entry.mapping = UploadBufferMapping::Unmapped;
        handle.reserved_size = 0;
        if let Some(buffer) = buffer {
            self.pool.recycle(buffer);
        }
        Ok(())
    }

    pub fn delete(&mut self, handle: &mut TransferBuffer) -> Result<(), String> {
        if handle.id == 0 {
            return Ok(());
        }
        self.orphan(handle)?;
        self.entries.remove(&handle.id);
        handle.id = 0;
        Ok(())
    }
}

impl Drop for UploadBuffers {
    fn drop(&mut self) {
        for entry in self.entries.values_mut() {
            if let Some(buffer) = entry.buffer.take() {
                self.pool.recycle(buffer);
            }
        }
    }
}

#[cfg(test)]
#[path = "upload_buffer_tests.rs"]
mod tests;
