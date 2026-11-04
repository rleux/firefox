/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::device::{Buffer as DeviceBuffer, BufferId, BufferKind, VertexArray, VertexDescriptor};
use crate::internal_types::FastHashMap;
use std::num::NonZeroUsize;

pub(super) struct Data {
    pub kind: BufferKind,
    pub bytes: Vec<u8>,
}
impl Data {
    fn write(
        &mut self,
        length: usize,
        preserve: bool,
        write: impl FnOnce(&mut [u8]),
    ) -> Result<(), String> {
        if length > isize::MAX as usize {
            return Err("Vulkan vertex buffer exceeds address space".into());
        }
        if !preserve {
            self.bytes.clear();
        }
        self.bytes
            .try_reserve(length.saturating_sub(self.bytes.len()))
            .map_err(|e| e.to_string())?;
        self.bytes.resize(length, 0);
        write(&mut self.bytes);
        Ok(())
    }
}

pub(super) struct Array {
    pub vertices: u32,
    pub indices: Option<u32>,
    pub instances: Option<u32>,
    pub stride: usize,
    pub divisor: u32,
    pub descriptor: VertexDescriptor,
}

#[derive(Default)]
pub(super) struct VertexArrayStore {
    last_id: u32,
    pub arrays: FastHashMap<u32, Array>,
    pub buffers: FastHashMap<u32, Data>,
    pub bound: Option<u32>,
}

impl VertexArrayStore {
    fn entry(&self, array: &VertexArray) -> Result<&Array, String> {
        self.arrays
            .get(&array.id)
            .filter(|a| {
                a.vertices == array.vertices.0
                    && a.indices == array.indices.map(|id| id.0)
                    && a.instances == array.instances.map(|id| id.0)
                    && a.stride == array.instance_stride
            })
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
        self.buffers.insert(
            id,
            Data {
                kind,
                bytes: Vec::new(),
            },
        );
        Ok(DeviceBuffer { id, kind, size: 0 })
    }

    pub fn buffer_data(&self, buffer: &DeviceBuffer) -> Result<&Data, String> {
        self.buffers
            .get(&buffer.id)
            .filter(|data| data.kind == buffer.kind && data.bytes.len() == buffer.size)
            .ok_or_else(|| "Invalid Vulkan buffer".into())
    }

    pub fn delete_buffer(&mut self, buffer: &mut DeviceBuffer) -> Result<(), String> {
        if buffer.id == 0 {
            return Ok(());
        }
        self.buffer_data(buffer)?;
        self.buffers.remove(&buffer.id);
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
                descriptor: VertexDescriptor {
                    vertex_attributes: descriptor.vertex_attributes,
                    instance_attributes: descriptor.instance_attributes,
                },
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
        self.data_mut(buffer.id)?
            .write(bytes.len(), false, |out| out.copy_from_slice(bytes))?;
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
        self.data_mut(buffer.id)?.write(length, false, |out| {
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
        self.data_mut(buffer.id)?
            .write(length, false, |bytes| bytes.fill(0))?;
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
        let data = self.data_mut(buffer.id)?;
        let end = offset
            .checked_add(bytes.len())
            .filter(|end| *end <= data.bytes.len())
            .ok_or("Vulkan vertex update exceeds buffer bounds")?;
        if bytes.is_empty() {
            return Ok(());
        }
        data.write(data.bytes.len(), true, |out| {
            out[offset..end].copy_from_slice(bytes)
        })
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
