/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::bindings::DrawBindings;
use super::draw::Draw;
use super::{wgt, Buffer, BufferPool};
use api::units::DeviceIntRect;
use crate::device::{Buffer as DeviceBuffer, BufferId, BufferKind, VertexArray, VertexDescriptor};
use crate::internal_types::FastHashMap;
use std::num::NonZeroUsize;
use std::rc::Rc;

struct Data {
    usage: wgt::BufferUses,
    length: usize,
    buffer: Option<Rc<Buffer>>,
}

impl Data {
    fn write(
        &mut self,
        pool: &BufferPool,
        length: usize,
        preserve: bool,
        write: impl FnOnce(&mut [u8]),
    ) -> Result<(), String> {
        if pool.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        if length == 0 {
            if let Some(old) = self.buffer.take() {
                pool.recycle(old);
            }
            self.length = 0;
            return Ok(());
        }
        if let Some(buffer) = self
            .buffer
            .as_mut()
            .filter(|b| b.size() >= length as u64)
            .and_then(Rc::get_mut)
        {
            buffer.write_with(length, |bytes| {
                write(bytes);
                Ok(())
            })?;
        } else {
            let previous = if preserve {
                Some(
                    self.buffer
                        .as_ref()
                        .ok_or("Vulkan buffer has no storage")?
                        .mapped_read_only()?,
                )
            } else {
                None
            };
            let buffer = pool.upload_with(length, self.usage, |bytes| {
                if let Some(previous) = previous {
                    bytes.copy_from_slice(&previous[..length]);
                }
                write(bytes);
                Ok(())
            })?;
            if let Some(old) = self.buffer.replace(buffer) {
                pool.recycle(old);
            }
        }
        self.length = length;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Array {
    vertices: u32,
    indices: Option<u32>,
    instances: Option<u32>,
    stride: usize,
    divisor: u32,
}

pub(super) struct VertexArrayStore {
    pool: Rc<BufferPool>,
    last_id: u32,
    arrays: FastHashMap<u32, Array>,
    buffers: FastHashMap<u32, Data>,
    bound: Option<u32>,
}

pub(super) struct InstanceSlice {
    buffer: Rc<Buffer>,
    offset: u64,
    count: u32,
    stride: u64,
}

impl InstanceSlice {
    pub fn draw(&self, bindings: DrawBindings, scissor: DeviceIntRect) -> Result<Draw, String> {
        if self.stride != bindings.pipeline.instance_stride {
            return Err("Vulkan instance stride does not match the draw pipeline".into());
        }
        Ok(Draw {
            bindings,
            instances: self.buffer.clone(),
            instance_offset: self.offset,
            instance_count: self.count,
            scissor,
        })
    }
}

impl VertexArrayStore {
    pub fn new(pool: &Rc<BufferPool>) -> Self {
        Self {
            pool: pool.clone(),
            last_id: 0,
            arrays: FastHashMap::default(),
            buffers: FastHashMap::default(),
            bound: None,
        }
    }

    fn entry(&self, array: &VertexArray) -> Result<Array, String> {
        self.arrays
            .get(&array.id)
            .filter(|a| {
                a.vertices == array.vertices.0
                    && a.indices == array.indices.map(|id| id.0)
                    && a.instances == array.instances.map(|id| id.0)
                    && a.stride == array.instance_stride
            })
            .copied()
            .ok_or_else(|| "Invalid Vulkan vertex array".into())
    }

    fn next_id(&mut self) -> Result<u32, String> {
        self.last_id = self
            .last_id
            .checked_add(1)
            .ok_or("Vulkan vertex-array identifier space exhausted")?;
        Ok(self.last_id)
    }

    pub fn create_buffer(&mut self, kind: BufferKind) -> Result<DeviceBuffer, String> {
        let id = self.next_id()?;
        let usage = match kind {
            BufferKind::Vertex => wgt::BufferUses::VERTEX,
            BufferKind::Index => wgt::BufferUses::INDEX,
        };
        self.buffers.insert(
            id,
            Data {
                usage,
                length: 0,
                buffer: None,
            },
        );
        Ok(DeviceBuffer { id, kind, size: 0 })
    }

    fn buffer_data(&self, buffer: &DeviceBuffer) -> Result<&Data, String> {
        let usage = match buffer.kind {
            BufferKind::Vertex => wgt::BufferUses::VERTEX,
            BufferKind::Index => wgt::BufferUses::INDEX,
        };
        self.buffers
            .get(&buffer.id)
            .filter(|data| data.usage == usage && data.length == buffer.size)
            .ok_or_else(|| "Invalid Vulkan buffer".into())
    }

    pub fn delete_buffer(&mut self, buffer: &mut DeviceBuffer) -> Result<(), String> {
        if buffer.id == 0 {
            return Ok(());
        }
        self.buffer_data(buffer)?;
        if let Some(storage) = self.buffers.remove(&buffer.id).unwrap().buffer {
            self.pool.recycle(storage);
        }
        buffer.id = 0;
        buffer.size = 0;
        Ok(())
    }

    pub fn create(
        &mut self,
        descriptor: &VertexDescriptor,
        vertices: &DeviceBuffer,
        instances: Option<&DeviceBuffer>,
        indices: Option<&DeviceBuffer>,
        divisor: u32,
    ) -> Result<VertexArray, String> {
        if vertices.kind != BufferKind::Vertex
            || instances.is_some() != !descriptor.instance_attributes.is_empty()
            || instances.map_or(false, |buffer| buffer.kind != BufferKind::Vertex)
            || indices.map_or(false, |buffer| buffer.kind != BufferKind::Index)
        {
            return Err("Invalid Vulkan vertex array buffers".into());
        }
        for buffer in Some(vertices).into_iter().chain(instances).chain(indices) {
            self.buffer_data(buffer)?;
        }
        let stride = descriptor
            .instance_attributes
            .iter()
            .try_fold(0usize, |total, a| {
                (a.count as usize)
                    .checked_mul(a.kind.size_in_bytes() as usize)
                    .and_then(|size| total.checked_add(size))
                    .ok_or("Vulkan instance stride overflow")
            })?;
        let id = self.next_id()?;
        self.arrays.insert(
            id,
            Array {
                vertices: vertices.id,
                instances: instances.map(|buffer| buffer.id),
                indices: indices.map(|buffer| buffer.id),
                stride,
                divisor,
            },
        );
        Ok(VertexArray {
            id,
            vertices: BufferId(vertices.id),
            instances: instances.map(|buffer| BufferId(buffer.id)),
            indices: indices.map(|buffer| BufferId(buffer.id)),
            instance_stride: stride,
        })
    }

    pub fn bind(&mut self, vao: &VertexArray) -> Result<(), String> {
        self.entry(vao)?;
        self.bound = Some(vao.id);
        Ok(())
    }

    pub fn unbind(&mut self) {
        self.bound = None;
    }

    fn data_mut(&mut self, id: u32) -> Result<&mut Data, String> {
        self.buffers
            .get_mut(&id)
            .ok_or_else(|| "Unknown Vulkan vertex buffer".into())
    }

    pub fn write_buffer(&mut self, buffer: &mut DeviceBuffer, bytes: &[u8]) -> Result<(), String> {
        self.buffer_data(buffer)?;
        let pool = self.pool.clone();
        self.data_mut(buffer.id)?
            .write(&pool, bytes.len(), false, |out| out.copy_from_slice(bytes))?;
        buffer.size = bytes.len();
        Ok(())
    }

    pub fn write_buffer_repeated(
        &mut self,
        buffer: &mut DeviceBuffer,
        bytes: &[u8],
        stride: usize,
        repeat: NonZeroUsize,
    ) -> Result<(), String> {
        self.buffer_data(buffer)?;
        if stride == 0 || bytes.len() % stride != 0 {
            return Err("Invalid Vulkan buffer element size".into());
        }
        let repeat = repeat.get();
        let length = bytes
            .len()
            .checked_mul(repeat)
            .ok_or("Vulkan repeated instance size overflow")?;
        let pool = self.pool.clone();
        self.data_mut(buffer.id)?
            .write(&pool, length, false, |out| {
                for (index, target) in out.chunks_exact_mut(stride).enumerate() {
                    let source = index / repeat * stride;
                    target.copy_from_slice(&bytes[source..source + stride]);
                }
            })?;
        buffer.size = length;
        Ok(())
    }

    pub fn reallocate(&mut self, buffer: &mut DeviceBuffer, length: usize) -> Result<(), String> {
        self.buffer_data(buffer)?;
        let pool = self.pool.clone();
        self.data_mut(buffer.id)?
            .write(&pool, length, false, |bytes| bytes.fill(0))?;
        buffer.size = length;
        Ok(())
    }

    pub fn update_range(
        &mut self,
        buffer: &DeviceBuffer,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), String> {
        self.buffer_data(buffer)?;
        let pool = self.pool.clone();
        let data = self.data_mut(buffer.id)?;
        let end = offset
            .checked_add(bytes.len())
            .filter(|end| *end <= data.length)
            .ok_or("Vulkan vertex update exceeds buffer bounds")?;
        if bytes.is_empty() {
            return Ok(());
        }
        data.write(&pool, data.length, true, |out| {
            out[offset..end].copy_from_slice(bytes)
        })
    }

    pub fn instances(&self, base: u32, count: u32) -> Result<Option<InstanceSlice>, String> {
        if count == 0 {
            return Ok(None);
        }
        let array = self
            .bound
            .and_then(|id| self.arrays.get(&id))
            .ok_or("No Vulkan vertex array is bound")?;
        if array.divisor != 1 || array.stride == 0 {
            return Err("Vulkan instanced draws require divisor one".into());
        }
        let offset = (base as usize)
            .checked_mul(array.stride)
            .ok_or("Vulkan base instance overflow")?;
        let size = (count as usize)
            .checked_mul(array.stride)
            .ok_or("Vulkan instance count overflow")?;
        let data = array
            .instances
            .and_then(|id| self.buffers.get(&id))
            .ok_or("Invalid Vulkan instance buffer")?;
        if offset
            .checked_add(size)
            .map_or(true, |end| end > data.length)
        {
            return Err("Vulkan draw exceeds instance data".into());
        }
        let buffer = data
            .buffer
            .as_ref()
            .ok_or("Vulkan instance buffer has no storage")?;
        buffer.vertex_binding(offset as u64, size as u64)?;
        Ok(Some(InstanceSlice {
            buffer: buffer.clone(),
            offset: offset as u64,
            count,
            stride: array.stride as u64,
        }))
    }

    pub fn delete(&mut self, vao: &mut VertexArray) -> Result<(), String> {
        if vao.id == 0 {
            return Ok(());
        }
        self.entry(vao)?;
        if self.bound == Some(vao.id) {
            self.unbind();
        }
        self.arrays.remove(&vao.id);
        vao.id = 0;
        Ok(())
    }
}

impl Drop for VertexArrayStore {
    fn drop(&mut self) {
        for data in self.buffers.values_mut() {
            if let Some(buffer) = data.buffer.take() {
                self.pool.recycle(buffer);
            }
        }
    }
}

#[cfg(test)]
#[path = "vertex_array_tests.rs"]
mod tests;
