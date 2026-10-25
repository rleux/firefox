/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, Device};
use std::{os::fd::BorrowedFd, rc::Rc, sync::Arc};

/// A single-use GPU wait for a fence exported by EGL or another compatible API.
pub struct SyncFileWait {
    pub(super) owner: Rc<Device>,
    pub(super) semaphore: Arc<dyn hal::external_sync::Semaphore>,
}

impl SyncFileWait {
    /// # Safety
    /// The borrowed FD must be a genuine SYNC_FD semaphore export or compatible fence.
    pub unsafe fn import(owner: &Rc<Device>, fd: BorrowedFd<'_>) -> Result<Self, String> {
        if owner.is_lost() || !owner.supports_sync_file_import() {
            return Err("Vulkan sync-file import is unavailable".into());
        }
        let semaphore = unsafe { hal::external_sync::import_sync_file(&*owner.open.device, fd) }
            .map_err(|error| error.to_string())?;
        Ok(Self {
            owner: owner.clone(),
            semaphore,
        })
    }
}

impl Device {
    pub(super) fn supports_sync_file_import(&self) -> bool {
        *self
            .native_cache
            .sync_file
            .get_or_init(|| hal::external_sync::supports_sync_file_import(&*self.open.device))
    }
}
