/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Device, Recording};
use super::state::UsageState;
use std::ops::Deref;
use std::rc::Rc;

pub(super) struct Owned<T: ?Sized> {
    pub(super) owner: Rc<Device>,
    raw: Option<Box<T>>,
    destroy: unsafe fn(&dyn hal::DynDevice, Box<T>),
}

impl<T: ?Sized> Owned<T> {
    pub fn new(owner: &Rc<Device>, raw: Box<T>, destroy: unsafe fn(&dyn hal::DynDevice, Box<T>)) -> Self {
        Self {
            owner: owner.clone(),
            raw: Some(raw),
            destroy,
        }
    }
}

impl<T: ?Sized> Deref for Owned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.raw.as_deref().unwrap()
    }
}

impl<T: ?Sized> Drop for Owned<T> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { (self.destroy)(self.owner.open.device.as_ref(), raw) }
        }
    }
}

pub struct Buffer {
    pub(super) raw: Owned<dyn hal::DynBuffer>,
    size: u64,
    usage: wgt::BufferUses,
    mapping: hal::BufferMapping,
    used_size: wgt::BufferSize,
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
        let device = owner.open.device.as_ref();
        let (raw, size) = unsafe {
            device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR Vulkan buffer"),
                size,
                usage: usage | wgt::BufferUses::MAP_WRITE,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|error| format!("Creating buffer: {error:?}"))?;
        let raw = Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_buffer);
        let mapping = unsafe { device.map_buffer(&*raw, 0..size) }
            .map_err(|error| format!("Mapping upload: {error:?}"))?;
        let mut buffer = Self {
            raw,
            size,
            usage: usage | wgt::BufferUses::MAP_WRITE,
            mapping,
            used_size: wgt::BufferSize::new((length as u64).max(4)).unwrap(),
            state: UsageState::new(wgt::BufferUses::MAP_WRITE),
        };
        unsafe {
            std::ptr::write_bytes(
                buffer.mapping.ptr.as_ptr(),
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
        let mapping = &self.mapping;
        write(unsafe { std::slice::from_raw_parts_mut(mapping.ptr.as_ptr(), length) })?;
        if !mapping.is_coherent {
            unsafe { device.flush_mapped_ranges(&*self.raw, &[0..self.size]) }
        }
        self.used_size = wgt::BufferSize::new((length as u64).max(4)).unwrap();
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
        commands.keep(self);
        if from != to {
            unsafe {
                commands
                    .encoder()
                    .transition_buffers(&[hal::BufferBarrier {
                        buffer: &*self.raw,
                        usage: hal::StateTransition { from, to },
                    }]);
            }
        }
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn vertex_binding(
        &self,
        offset: u64,
        size: u64,
    ) -> Result<hal::BufferBinding<'_, dyn hal::DynBuffer, wgt::BufferAddress>, String> {
        if !self.usage.contains(wgt::BufferUses::VERTEX)
            || offset % 4 != 0
            || size == 0
            || offset
                .checked_add(size)
                .map_or(true, |end| end > self.used_size.get())
        {
            return Err("Invalid Vulkan vertex buffer range".into());
        }
        Ok(hal::BufferBinding::new_unchecked(
            &*self.raw,
            offset,
            size,
        ))
    }

    pub fn binding<S: From<wgt::BufferSize>>(&self) -> hal::BufferBinding<'_, dyn hal::DynBuffer, S> {
        hal::BufferBinding::new_unchecked(&*self.raw, 0, self.used_size.into())
    }

    pub fn binding_size(&self) -> u64 {
        self.used_size.get()
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { self.raw.owner.open.device.unmap_buffer(&*self.raw) }
    }
}
