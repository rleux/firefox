/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Buffer, Recording, SubmissionQueue};
use std::rc::Rc;

#[derive(Debug, PartialEq)]
struct InstanceRange {
    buffer: usize,
    offset: usize,
    size: usize,
}

fn instance_layout(
    sizes: impl Iterator<Item = usize>,
    limit: usize,
) -> Result<(Vec<InstanceRange>, Vec<usize>), String> {
    if limit < 4 {
        return Err("Vulkan instance buffer limit is too small".into());
    }
    let mut ranges = Vec::with_capacity(sizes.size_hint().1.unwrap_or(0));
    let mut buffers: Vec<usize> = Vec::new();
    for size in sizes {
        let length = size.max(4);
        let offset = buffers
            .last()
            .copied()
            .unwrap_or(0)
            .checked_add(3)
            .ok_or("Vulkan instance offset overflow")?
            & !3;
        let end = offset
            .checked_add(length)
            .ok_or("Vulkan instance size overflow")?;
        let offset = if buffers.is_empty() || end > limit {
            buffers.push(length);
            0
        } else {
            *buffers.last_mut().unwrap() = end;
            offset
        };
        ranges.push(InstanceRange {
            buffer: buffers.len() - 1,
            offset,
            size,
        });
    }
    Ok((ranges, buffers))
}

pub struct InstanceBuffers {
    buffers: Vec<Rc<Buffer>>,
    ranges: Vec<InstanceRange>,
}

impl InstanceBuffers {
    pub fn binding(
        &self,
        draw: usize,
    ) -> Result<hal::BufferBinding<'_, dyn hal::DynBuffer, wgt::BufferAddress>, String> {
        let range = self
            .ranges
            .get(draw)
            .ok_or("Invalid Vulkan instance draw index")?;
        self.buffers[range.buffer].vertex_binding(range.offset as u64, range.size.max(4) as u64)
    }
}

impl SubmissionQueue {
    /// Populate and transition instance storage before beginning the render pass.
    pub fn upload_instances_with(
        &self,
        recording: &mut Recording<'_>,
        sizes: &[usize],
        write: impl FnMut(usize, &mut [u8]) -> Result<(), String>,
    ) -> Result<InstanceBuffers, String> {
        self.upload_instances_iter(recording, sizes.iter().copied(), write)
    }

    pub(in crate::device::wgpu) fn upload_instances_iter(
        &self,
        recording: &mut Recording<'_>,
        sizes: impl Iterator<Item = usize>,
        mut write: impl FnMut(usize, &mut [u8]) -> Result<(), String>,
    ) -> Result<InstanceBuffers, String> {
        recording.recording_id(&self.pool.owner)?;
        let limit = self
            .pool
            .owner
            .capabilities
            .limits
            .max_buffer_size
            .min(1024 * 1024) as usize;
        let (ranges, sizes) = instance_layout(sizes, limit)?;
        for &size in &sizes {
            super::super::super::resources::allocation_size(
                size,
                self.pool.owner.capabilities.limits.max_buffer_size,
            )?;
        }
        let mut buffers = Vec::with_capacity(sizes.len());
        let mut first = 0;
        for (index, size) in sizes.into_iter().enumerate() {
            let mut last = first;
            while last < ranges.len() && ranges[last].buffer == index {
                last += 1;
            }
            let buffer = self.upload_in_recording(
                recording,
                size,
                wgt::BufferUses::VERTEX,
                |destination| {
                    let mut end = 0;
                    for (draw, range) in ranges[first..last].iter().enumerate() {
                        destination[end..range.offset].fill(0);
                        if range.size == 0 {
                            destination[range.offset..range.offset + 4].fill(0);
                        }
                        write(
                            first + draw,
                            &mut destination[range.offset..range.offset + range.size],
                        )?;
                        end = range.offset + range.size.max(4);
                    }
                    Ok(())
                },
            )?;
            buffer.transition(recording, wgt::BufferUses::VERTEX)?;
            buffers.push(buffer);
            first = last;
        }
        Ok(InstanceBuffers { buffers, ranges })
    }
}

#[cfg(test)]
#[path = "instance_buffer_tests.rs"]
mod tests;
