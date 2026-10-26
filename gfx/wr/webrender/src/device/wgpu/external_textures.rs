/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Device, SubmissionQueue, Texture, TextureFilter};
use api::ExternalTextureHandle;
use std::cell::{Cell, RefCell};
use crate::internal_types::FastHashMap;
use std::convert::TryFrom;
use std::rc::{Rc, Weak};
use std::sync::Arc;

#[cfg(target_os = "linux")]
#[path = "external_access.rs"]
mod access;
#[cfg(target_os = "linux")]
pub use self::access::{ExternalReleaseStatus, PendingExternalRelease};

pub struct ExternalTextureRegistry {
    owner: Rc<Device>,
    entries: RefCell<FastHashMap<u32, Rc<Texture>>>,
    last_id: Cell<u32>,
    submissions: RefCell<Weak<SubmissionQueue>>,
}

impl ExternalTextureRegistry {
    pub(super) fn new(owner: &Rc<Device>) -> Rc<Self> {
        Rc::new(Self {
            owner: owner.clone(),
            entries: RefCell::new(FastHashMap::default()),
            last_id: Cell::new(0),
            submissions: RefCell::new(Weak::new()),
        })
    }

    pub fn device(&self) -> &Rc<Device> {
        &self.owner
    }

    pub(super) fn attach_queue(&self, queue: &Rc<SubmissionQueue>) {
        debug_assert!(Rc::ptr_eq(&self.owner, queue.owner()));
        debug_assert!(self.submissions.borrow().upgrade().is_none());
        *self.submissions.borrow_mut() = Rc::downgrade(queue);
    }

    pub(super) fn disconnect(&self) {
        *self.submissions.borrow_mut() = Weak::new();
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

    /// Register immutable storage finalized and logically expired on this exact graphics queue.
    ///
    /// # Safety
    /// The image must have successfully finalized on its exact graphics endpoint,
    /// and its logical texture must be expired. Do not issue new producer writes,
    /// layout changes or reuse while any renderer alias or retained binding may
    /// schedule draws, and through retirement of all accepted consumers.
    /// Unregister removes only the name; a lease or handle is no readiness or reuse proof.
    pub unsafe fn register_shared_image(
        &self,
        lease: Arc<dyn hal::shared_image::ImageLease>,
        descriptor: &hal::TextureDescriptor<'_>,
        view_format: wgt::TextureFormat,
        filter: TextureFilter,
        force_opaque: bool,
    ) -> Result<ExternalTextureHandle, String> {
        let texture = unsafe {
            Texture::from_shared_image(
                &self.owner,
                lease,
                descriptor,
                view_format,
                filter,
                force_opaque,
            )
        }?;
        self.register(&texture)
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
