/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Buffer, Device};
use std::{cell::RefCell, rc::Rc};

const BYTE_LIMIT: u64 = 64 * 1024 * 1024;
const COUNT_LIMIT: usize = 256;

pub struct BufferPool {
    pub(super) owner: Rc<Device>,
    returned: RefCell<Vec<Rc<Buffer>>>,
}

impl BufferPool {
    pub fn new(owner: &Rc<Device>) -> Self {
        Self {
            owner: owner.clone(),
            returned: RefCell::new(Vec::new()),
        }
    }

    pub fn upload(&self, bytes: &[u8], usage: wgt::BufferUses) -> Result<Rc<Buffer>, String> {
        self.upload_with(bytes.len(), usage, |destination| {
            destination.copy_from_slice(bytes);
            Ok(())
        })
    }

    pub fn upload_with(
        &self,
        length: usize,
        usage: wgt::BufferUses,
        write: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<Rc<Buffer>, String> {
        if self.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let reusable = {
            let mut returned = self.returned.borrow_mut();
            let index = returned.iter_mut().position(|buffer| {
                Rc::get_mut(buffer).map_or(false, |buffer| {
                    buffer.usage == (usage | wgt::BufferUses::MAP_WRITE)
                        && buffer.size() >= (length as u64).max(4)
                })
            });
            index.map(|index| returned.swap_remove(index))
        };
        if let Some(mut buffer) = reusable {
            Rc::get_mut(&mut buffer)
                .unwrap()
                .write_with(length, write)?;
            Ok(buffer)
        } else {
            Buffer::new_with(&self.owner, length, usage, write)
        }
    }

    /// Return a buffer; it becomes reusable only after every other owner releases it.
    pub fn recycle(&self, buffer: Rc<Buffer>) {
        if self.owner.is_lost()
            || !Rc::ptr_eq(&buffer.raw.owner, &self.owner)
            || buffer.size() > BYTE_LIMIT
        {
            return;
        }
        let mut returned = self.returned.borrow_mut();
        if returned.iter().any(|old| Rc::ptr_eq(old, &buffer)) {
            return;
        }
        let mut bytes: u64 = returned.iter().map(|old| old.size()).sum();
        let mut count = returned.len();
        returned.retain(|old| {
            if (bytes + buffer.size() > BYTE_LIMIT || count >= COUNT_LIMIT)
                && Rc::strong_count(old) == 1
            {
                bytes -= old.size();
                count -= 1;
                false
            } else {
                true
            }
        });
        if bytes + buffer.size() <= BYTE_LIMIT && count < COUNT_LIMIT {
            returned.push(buffer);
        }
    }

    pub fn clear(&self) {
        self.returned.borrow_mut().clear();
    }

    pub fn bytes(&self) -> u64 {
        self.returned
            .borrow()
            .iter()
            .map(|buffer| buffer.size())
            .sum()
    }
}
