/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{Device, Texture};
use api::ExternalTextureHandle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::rc::Rc;

pub struct ExternalTextureRegistry {
    owner: Rc<Device>,
    entries: RefCell<HashMap<u32, Rc<Texture>>>,
    last_id: Cell<u32>,
}


impl ExternalTextureRegistry {
    pub(super) fn new(owner: &Rc<Device>) -> Rc<Self> {
        Rc::new(Self {
            owner: owner.clone(),
            entries: RefCell::new(HashMap::new()),
            last_id: Cell::new(0),
        })
    }

    pub fn device(&self) -> &Rc<Device> {
        &self.owner
    }

    pub fn register(&self, texture: &Rc<Texture>) -> Result<ExternalTextureHandle, String> {
        if !Rc::ptr_eq(&texture.raw.owner, &self.owner) {
            return Err("External texture belongs to another Vulkan device".into());
        }
        let id = self
            .last_id
            .get()
            .checked_add(1)
            .ok_or("Vulkan external texture identifiers exhausted")?;
        self.entries.borrow_mut().insert(id, texture.clone());
        self.last_id.set(id);
        Ok(ExternalTextureHandle(u64::from(id)))
    }

    /// Remove the name; existing bindings and submissions retain their texture.
    pub fn unregister(&self, handle: ExternalTextureHandle) -> Result<(), String> {
        let id =
            u32::try_from(handle.0).map_err(|_| "Invalid Vulkan external texture identifier")?;
        self.entries
            .borrow_mut()
            .remove(&id)
            .ok_or("Unknown Vulkan external texture")?;
        Ok(())
    }

    pub(super) fn get(&self, id: u32) -> Result<Rc<Texture>, String> {
        self.entries
            .borrow()
            .get(&id)
            .cloned()
            .ok_or_else(|| "Unknown Vulkan external texture".into())
    }
}

#[cfg(test)]
#[path = "external_texture_tests.rs"]
mod tests;
