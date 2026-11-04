/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::device::{TransferBuffer, UploadBufferMapping};
use crate::internal_types::FastHashMap;
use std::{mem::MaybeUninit, ptr::NonNull};

struct Storage {
    bytes: Box<[u8]>,
    mapped: bool,
    persistent: bool,
}
#[derive(Default)]
pub(super) struct UploadBuffers {
    last: u32,
    entries: FastHashMap<u32, Storage>,
}
impl UploadBuffers {
    pub fn create(&mut self) -> Result<TransferBuffer, String> {
        self.last = self
            .last
            .checked_add(1)
            .ok_or("Vulkan upload identifiers exhausted")?;
        self.entries.insert(
            self.last,
            Storage {
                bytes: Box::new([]),
                mapped: false,
                persistent: false,
            },
        );
        Ok(TransferBuffer {
            id: self.last,
            reserved_size: 0,
        })
    }
    pub fn allocate(
        &mut self,
        buffer: &mut TransferBuffer,
        size: usize,
        persistent: bool,
    ) -> Result<UploadBufferMapping, String> {
        let entry = self
            .entries
            .get_mut(&buffer.id)
            .ok_or("Unknown Vulkan upload buffer")?;
        if entry.mapped {
            return Err("Vulkan upload buffer is already mapped".into());
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(|e| e.to_string())?;
        bytes.resize(size, 0);
        entry.bytes = bytes.into_boxed_slice();
        entry.persistent = persistent;
        entry.mapped = true;
        buffer.reserved_size = size;
        let ptr = NonNull::new(entry.bytes.as_mut_ptr().cast()).unwrap();
        Ok(if persistent {
            UploadBufferMapping::Persistent(ptr)
        } else {
            UploadBufferMapping::Transient(ptr)
        })
    }
    pub fn map(&mut self, buffer: &TransferBuffer) -> Result<NonNull<MaybeUninit<u8>>, String> {
        let entry = self
            .entries
            .get_mut(&buffer.id)
            .ok_or("Unknown Vulkan upload buffer")?;
        if entry.mapped {
            return Err("Vulkan upload buffer is already mapped".into());
        }
        entry.mapped = true;
        Ok(NonNull::new(entry.bytes.as_mut_ptr().cast()).unwrap())
    }
    pub fn flush(
        &mut self,
        buffer: &TransferBuffer,
        mapping: &UploadBufferMapping,
        used: usize,
    ) -> Result<&[u8], String> {
        let entry = self
            .entries
            .get_mut(&buffer.id)
            .ok_or("Unknown Vulkan upload buffer")?;
        let ptr = match mapping {
            UploadBufferMapping::Transient(ptr) | UploadBufferMapping::Persistent(ptr) => *ptr,
            _ => return Err("Unmapped Vulkan upload buffer".into()),
        };
        if !entry.mapped
            || ptr.as_ptr().cast::<u8>() != entry.bytes.as_mut_ptr()
            || used > entry.bytes.len()
            || matches!(mapping, UploadBufferMapping::Persistent(_)) != entry.persistent
        {
            return Err("Invalid Vulkan upload mapping or length".into());
        }
        entry.mapped = entry.persistent;
        Ok(&entry.bytes[..used])
    }
    pub fn orphan(&mut self, buffer: &mut TransferBuffer) -> Result<(), String> {
        let entry = self
            .entries
            .get_mut(&buffer.id)
            .ok_or("Unknown Vulkan upload buffer")?;
        if entry.mapped && !entry.persistent {
            return Err("Cannot orphan a transient mapped upload".into());
        }
        entry.bytes = Box::new([]);
        entry.mapped = false;
        entry.persistent = false;
        buffer.reserved_size = 0;
        Ok(())
    }
    pub fn delete(&mut self, buffer: &mut TransferBuffer) -> Result<(), String> {
        if buffer.id == 0 {
            return Ok(());
        }
        self.entries
            .remove(&buffer.id)
            .ok_or("Unknown Vulkan upload buffer")?;
        buffer.id = 0;
        buffer.reserved_size = 0;
        Ok(())
    }
    pub fn write_readback(
        &mut self,
        buffer: &TransferBuffer,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let entry = self
            .entries
            .get_mut(&buffer.id)
            .ok_or("Unknown Vulkan readback buffer")?;
        if entry.mapped || bytes.len() != buffer.reserved_size {
            return Err("Invalid Vulkan readback buffer size or mapping".into());
        }
        entry.bytes = bytes.into_boxed_slice();
        Ok(())
    }
    pub fn read(&self, buffer: &TransferBuffer) -> Result<&[u8], String> {
        let entry = self
            .entries
            .get(&buffer.id)
            .ok_or("Unknown Vulkan readback buffer")?;
        if entry.mapped {
            return Err("Vulkan transfer buffer is already mapped for upload".into());
        }
        Ok(&entry.bytes)
    }
    pub fn bytes(&self) -> usize {
        self.entries.values().map(|s| s.bytes.len()).sum()
    }
}
