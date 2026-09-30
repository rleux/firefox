/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Device, Recording};
use super::state::UsageState;
use std::ops::Deref;
use std::rc::Rc;
use wgpu_hal::{CommandEncoder as _, Device as _};

pub(super) struct Owned<T> {
    pub(super) owner: Rc<Device>,
    raw: Option<T>,
    destroy: unsafe fn(&hal::vulkan::Device, T),
}

impl<T> Owned<T> {
    pub fn new(owner: &Rc<Device>, raw: T, destroy: unsafe fn(&hal::vulkan::Device, T)) -> Self {
        Self {
            owner: owner.clone(),
            raw: Some(raw),
            destroy,
        }
    }
}

impl<T> Deref for Owned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.raw.as_ref().unwrap()
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { (self.destroy)(&self.owner.open.device, raw) }
        }
    }
}

pub struct Buffer {
    pub(super) raw: Owned<hal::vulkan::Buffer>,
    size: u64,
    pub(super) usage: wgt::BufferUses,
    mapping: Option<hal::BufferMapping>,
    used_size: u64,
    state: UsageState<wgt::BufferUses>,
}

pub(super) fn allocation_size(length: usize, limit: u64) -> Result<u64, String> {
    let size = (length as u64)
        .max(4)
        .checked_next_power_of_two()
        .ok_or("Vulkan buffer size overflow")?;
    if size > limit || size > isize::MAX as u64 {
        return Err("Vulkan buffer exceeds device or address-space limit".into());
    }
    Ok(size)
}

impl Buffer {
    pub fn new(
        owner: &Rc<Device>,
        bytes: &[u8],
        usage: wgt::BufferUses,
    ) -> Result<Rc<Self>, String> {
        Self::new_with(owner, bytes.len(), usage, |destination| {
            destination.copy_from_slice(bytes);
            Ok(())
        })
    }

    pub fn new_with(
        owner: &Rc<Device>,
        length: usize,
        usage: wgt::BufferUses,
        write: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<Rc<Self>, String> {
        let size = allocation_size(length, owner.capabilities.limits.max_buffer_size)?;
        let device = &owner.open.device;
        let raw = unsafe {
            device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR Vulkan buffer"),
                size,
                usage: usage | wgt::BufferUses::MAP_WRITE,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|error| format!("Creating buffer: {error:?}"))?;
        let raw = Owned::new(owner, raw, hal::vulkan::Device::destroy_buffer);
        let mapping = unsafe { device.map_buffer(&raw, 0..size) }
            .map_err(|error| format!("Mapping upload: {error:?}"))?;
        let mut buffer = Self {
            raw,
            size,
            usage: usage | wgt::BufferUses::MAP_WRITE,
            mapping: Some(mapping),
            used_size: (length as u64).max(4),
            state: UsageState::new(wgt::BufferUses::MAP_WRITE),
        };
        unsafe {
            std::ptr::write_bytes(
                buffer.mapping.as_ref().unwrap().ptr.as_ptr(),
                0,
                size as usize,
            );
        }
        buffer.write_with(length, write)?;
        Ok(Rc::new(buffer))
    }

    pub fn write_with(
        &mut self,
        length: usize,
        write: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        if length as u64 > self.size {
            return Err("Vulkan buffer is too small".into());
        }
        let device = &self.raw.owner.open.device;
        let mapping = self
            .mapping
            .as_ref()
            .ok_or("Vulkan buffer is not mapped for upload")?;
        write(unsafe { std::slice::from_raw_parts_mut(mapping.ptr.as_ptr(), length) })?;
        // Short bindings still expose four bytes.
        if length < 4 {
            unsafe { std::ptr::write_bytes(mapping.ptr.as_ptr().add(length), 0, 4 - length) };
        }
        if !mapping.is_coherent {
            unsafe { device.flush_mapped_ranges(&self.raw, std::iter::once(0..self.size)) }
        }
        self.used_size = (length as u64).max(4);
        self.state.reset(wgt::BufferUses::MAP_WRITE);
        Ok(())
    }

    pub fn current_usage(&self) -> wgt::BufferUses {
        self.state.current()
    }

    pub fn transition(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        to: wgt::BufferUses,
    ) -> Result<(), String> {
        if to.is_empty()
            || !self.usage.contains(to)
            || (to.bits().count_ones() > 1 && !wgt::BufferUses::INCLUSIVE.contains(to))
        {
            return Err("Invalid Vulkan buffer usage transition".into());
        }
        let recording = commands.recording_id(&self.raw.owner)?;
        let (from, first) = self.state.prepare(&recording, to)?;
        if first {
            let resource = self.clone();
            commands.commit(move || resource.state.commit());
        }
        commands.keep(self.clone());
        if from != to {
            unsafe {
                commands
                    .encoder()
                    .transition_buffers(std::iter::once(hal::BufferBarrier {
                        buffer: &*self.raw,
                        usage: hal::StateTransition { from, to },
                    }));
            }
        }
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub(super) fn mapped_read_only(&self) -> Result<&[u8], String> {
        // GPU access must be read-only while CPU code reads this mapping.
        let allowed = wgt::BufferUses::MAP_WRITE | wgt::BufferUses::STORAGE_READ_ONLY
            | wgt::BufferUses::VERTEX | wgt::BufferUses::INDEX;
        if !self.usage.contains(wgt::BufferUses::MAP_WRITE) || !allowed.contains(self.usage) {
            return Err("Direct mapped reads require a GPU-read-only upload buffer".into());
        }
        let mapping = self.mapping.as_ref().ok_or("Buffer is not mapped")?;
        Ok(unsafe { std::slice::from_raw_parts(mapping.ptr.as_ptr(), self.used_size as usize) })
    }

    pub fn vertex_binding(
        &self,
        offset: u64,
        size: u64,
    ) -> Result<hal::BufferBinding<'_, hal::vulkan::Buffer>, String> {
        if !self.usage.contains(wgt::BufferUses::VERTEX)
            || offset % 4 != 0
            || size == 0
            || offset
                .checked_add(size)
                .map_or(true, |end| end > self.used_size)
        {
            return Err("Invalid Vulkan vertex buffer range".into());
        }
        Ok(hal::BufferBinding::new_unchecked(
            &*self.raw,
            offset,
            std::num::NonZeroU64::new(size),
        ))
    }

    pub fn binding(&self) -> hal::BufferBinding<'_, hal::vulkan::Buffer> {
        hal::BufferBinding::new_unchecked(&*self.raw, 0, std::num::NonZeroU64::new(self.used_size))
    }

    pub fn binding_size(&self) -> u64 {
        self.used_size
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if self.mapping.take().is_some() {
            unsafe { self.raw.owner.open.device.unmap_buffer(&self.raw) }
        }
    }
}
