/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::unified::ProcessOwner;
use std::sync::Arc;
use wgc::{
    device::Device,
    resource::{SealedCanvasTexture, SharedCanvasTexture, Texture, TextureDescriptor},
    SubmissionIndex,
};
use wgh::shared_image::{AllocationBudget, ImageEpoch};

pub struct NativeCanvasAllocator {
    owner: Arc<ProcessOwner>,
    budget: Arc<AllocationBudget>,
}

impl NativeCanvasAllocator {
    pub fn new(owner: Arc<ProcessOwner>) -> Self {
        Self {
            owner,
            budget: Arc::new(AllocationBudget::new(128)),
        }
    }

    pub fn create_texture(
        &self,
        device: &Arc<Device>,
        descriptor: &TextureDescriptor<'_>,
    ) -> Result<(Arc<Texture>, Option<SharedCanvasTexture>), String> {
        if !self.owner.is_ready() {
            return Err("Native canvas owner is unavailable for allocation".into());
        }
        Ok(unsafe {
            device.create_shared_canvas_texture(
                descriptor,
                self.owner.graphics(),
                self.budget.clone(),
            )
        })
    }

    pub fn live_allocations(&self) -> usize {
        self.budget.live_allocations()
    }
}

pub struct SealedCanvasFrame {
    owner: Arc<ProcessOwner>,
    epoch: Arc<ImageEpoch>,
    logical_descriptor: wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>>,
    submission_index: SubmissionIndex,
}

impl SealedCanvasFrame {
    pub fn new(owner: Arc<ProcessOwner>, sealed: SealedCanvasTexture) -> Result<Arc<Self>, String> {
        let submission_index = sealed.submission_index();
        let (finalized, logical_descriptor) = sealed.into_parts();
        let epoch = finalized.publish_epoch();
        let graphics = owner.graphics();
        epoch
            .validate_owner(graphics.device.as_ref(), graphics.queue.as_ref())
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            owner,
            epoch,
            logical_descriptor,
            submission_index,
        }))
    }

    pub fn owner(&self) -> &Arc<ProcessOwner> {
        &self.owner
    }
    pub fn epoch(&self) -> &Arc<ImageEpoch> {
        &self.epoch
    }
    pub fn logical_descriptor(&self) -> &wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>> {
        &self.logical_descriptor
    }
    pub fn native_descriptor(&self) -> &wgh::TextureDescriptor<'static> {
        self.epoch.descriptor()
    }
    pub fn submission_index(&self) -> SubmissionIndex {
        self.submission_index
    }
}

pub struct NativeCanvasFrame {
    frame: Arc<SealedCanvasFrame>,
}

impl NativeCanvasFrame {
    pub fn new(frame: Arc<SealedCanvasFrame>) -> Self {
        Self { frame }
    }
    pub fn frame(&self) -> &Arc<SealedCanvasFrame> {
        &self.frame
    }
}

/// # Safety
/// The pointer must be null or an owned box returned for this opaque frame type.
#[no_mangle]
pub unsafe extern "C" fn wgpu_native_canvas_frame_delete(frame: *mut NativeCanvasFrame) {
    if !frame.is_null() {
        drop(unsafe { Box::from_raw(frame) });
    }
}
