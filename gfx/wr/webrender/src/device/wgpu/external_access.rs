/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::ExternalTextureRegistry;
use crate::device::wgpu::{
    Device, DmaBufImage, ForeignRgbImage, PendingForeignRelease, SharedTimeline, SubmissionQueue,
    SyncFileWait,
};
use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalReleaseStatus {
    Pending,
    /// The producer may queue a GPU wait for this value; it need not be complete yet.
    Submitted(u64),
    /// Ownership return is not guaranteed. The producer must abandon this allocation.
    Abandoned,
}

struct ReleaseRecord {
    owner: Rc<Device>,
    value: u64,
    status: Cell<ExternalReleaseStatus>,
}

#[derive(Clone)]
pub struct PendingExternalRelease(Rc<ReleaseRecord>);

impl PendingExternalRelease {
    pub fn status(&self) -> ExternalReleaseStatus {
        if self.0.owner.is_lost() {
            ExternalReleaseStatus::Abandoned
        } else {
            self.0.status.get()
        }
    }
}

struct CommitRelease(Rc<ReleaseRecord>);

impl CommitRelease {
    fn submit(self) {
        self.0
            .status
            .set(ExternalReleaseStatus::Submitted(self.0.value));
    }
}

impl Drop for CommitRelease {
    fn drop(&mut self) {
        if self.0.status.get() == ExternalReleaseStatus::Pending {
            self.0.status.set(ExternalReleaseStatus::Abandoned);
        }
    }
}

impl ExternalTextureRegistry {
    /// # Safety
    /// The caller must satisfy ForeignRgbImage::acquire's publication contract.
    pub unsafe fn acquire_foreign_rgb(
        &self,
        image: &ForeignRgbImage,
        ready: SyncFileWait,
    ) -> Result<(), String> {
        let queue = self.queue()?;
        let mut commands = queue.recording()?;
        image.acquire(&mut commands, ready)
    }

    pub fn release_foreign_rgb(
        &self,
        image: &ForeignRgbImage,
        released: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<PendingForeignRelease, String> {
        let queue = self.queue()?;
        let mut commands = queue.recording()?;
        image.release(&mut commands, released, value)
    }

    fn queue(&self) -> Result<Rc<SubmissionQueue>, String> {
        self.submissions
            .borrow()
            .upgrade()
            .ok_or_else(|| "Vulkan Renderer is unavailable".into())
    }

    /// Record an acquire in the Renderer's queue, including from image lock callbacks.
    /// # Safety
    /// The producer must satisfy DmaBufImage::acquire's ownership/timeline contract.
    pub unsafe fn acquire_dma_buf(
        &self,
        image: &Rc<DmaBufImage>,
        ready: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let queue = self.queue()?;
        let mut commands = queue.recording()?;
        unsafe { image.acquire(&mut commands, ready, value) }
    }

    /// Record release without submitting. Publish its value only when the receipt
    /// reports Submitted, after the Renderer has submitted the shared recording.
    pub fn release_dma_buf(
        &self,
        image: &Rc<DmaBufImage>,
        released: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<PendingExternalRelease, String> {
        let queue = self.queue()?;
        let mut commands = queue.recording()?;
        image.release(&mut commands, released, value)?;
        let record = Rc::new(ReleaseRecord {
            owner: self.owner.clone(),
            value,
            status: Cell::new(ExternalReleaseStatus::Pending),
        });
        let commit = CommitRelease(record.clone());
        commands.commit(move || commit.submit());
        Ok(PendingExternalRelease(record))
    }
}
