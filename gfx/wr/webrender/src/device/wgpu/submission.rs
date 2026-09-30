/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::binding_cache::BindingCache;
use super::resources::Owned;
use super::{hal, Device};
use std::any::Any;
use crate::internal_types::FastHashMap;
use std::cell::RefMut;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::time::Duration;

#[path = "submission_queue.rs"]
mod queue;
pub use self::queue::{InstanceBuffers, SubmissionQueue};

pub struct Submission {
    data: SubmissionData,
    state: SubmissionState,
    #[cfg(test)]
    failure: Option<FailurePoint>,
}

enum SubmissionState {
    Recording { id: Rc<()> },
    EncodingFailed { id: Rc<()> },
    Unconfirmed { id: Rc<()>, buffer: Box<dyn hal::DynCommandBuffer> },
    Submitted { buffer: Box<dyn hal::DynCommandBuffer> },
    Retired,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
enum FailurePoint {
    EndEncoding,
    AfterSubmit,
}

struct SubmissionData {
    owner: Rc<Device>,
    encoder: Box<dyn hal::DynCommandEncoder>,
    fence: Rc<Owned<dyn hal::DynFence>>,
    fence_value: u64,
    commits: Vec<Box<dyn FnOnce()>>,
    uniform_writes: Vec<Rc<super::Buffer>>,
    resources: FastHashMap<*const (), Rc<dyn Any>>,
    uploads: Vec<queue::RecycleUpload>,
    bindings: BindingCache,
}

/// Exclusive access to an open submission; releasing this borrow does not submit it.
pub struct Recording<'a> {
    submission: SubmissionBorrow<'a>,
}

enum SubmissionBorrow<'a> {
    Direct(&'a mut SubmissionData, &'a Rc<()>),
    Queued(RefMut<'a, SubmissionData>, RefMut<'a, Rc<()>>),
}

impl Deref for SubmissionBorrow<'_> {
    type Target = SubmissionData;

    fn deref(&self) -> &SubmissionData {
        match self {
            Self::Direct(submission, _) => submission,
            Self::Queued(submission, _) => submission,
        }
    }
}

impl DerefMut for SubmissionBorrow<'_> {
    fn deref_mut(&mut self) -> &mut SubmissionData {
        match self {
            Self::Direct(submission, _) => submission,
            Self::Queued(submission, _) => submission,
        }
    }
}

impl SubmissionBorrow<'_> {
    fn id(&self) -> &Rc<()> {
        match self {
            Self::Direct(_, id) => id,
            Self::Queued(_, id) => id,
        }
    }
}

impl Recording<'_> {
    pub fn encoder(&mut self) -> &mut dyn hal::DynCommandEncoder {
        self.submission.encoder.as_mut()
    }

    pub(super) fn recording_id(&self, owner: &Rc<Device>) -> Result<Rc<()>, String> {
        if !Rc::ptr_eq(owner, &self.submission.owner) {
            return Err("Vulkan recording device mismatch".into());
        }
        Ok(self.submission.id().clone())
    }

    pub(super) fn commit(&mut self, commit: impl FnOnce() + 'static) {
        self.submission.commits.push(Box::new(commit));
    }

    pub(super) fn flush_uniform_before_submit(&mut self, buffer: &Rc<super::Buffer>) {
        if buffer.needs_uniform_flush() {
            self.submission.uniform_writes.push(buffer.clone());
        }
    }

    pub(super) fn with_binding_cache<T>(
        &mut self,
        record: impl FnOnce(&mut Self, &mut BindingCache) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut bindings = std::mem::take(&mut self.submission.bindings);
        let result = record(self, &mut bindings);
        self.submission.bindings = bindings;
        result
    }

    fn keep_upload(&mut self, upload: queue::RecycleUpload) {
        self.submission.uploads.push(upload);
    }

    pub fn keep<T: 'static>(&mut self, value: &Rc<T>) {
        self.submission.resources.entry(Rc::as_ptr(value).cast())
            .or_insert_with(|| value.clone());
    }
}

impl Submission {
    pub fn new(owner: &Rc<Device>) -> Result<Self, String> {
        if owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let fence = unsafe { owner.open.device.create_fence() }
            .map_err(|error| format!("Creating submission fence: {error:?}"))?;
        Self::with_fence(
            owner,
            Rc::new(Owned::new(owner, fence, <dyn hal::DynDevice>::destroy_fence)),
            1,
        )
    }

    fn with_fence(
        owner: &Rc<Device>,
        fence: Rc<Owned<dyn hal::DynFence>>,
        fence_value: u64,
    ) -> Result<Self, String> {
        let mut encoder = unsafe {
            owner.open.device.create_command_encoder(&hal::CommandEncoderDescriptor {
                label: Some("WR Vulkan submission"),
                queue: owner.open.queue.as_ref(),
            })
        }.map_err(|error| format!("Creating submission encoder: {error:?}"))?;
        unsafe {
            if let Err(error) = encoder.begin_encoding(Some("WR Vulkan submission")) {
                encoder.reset_all(Vec::new());
                return Err(format!("Beginning submission: {error:?}"));
            }
        }
        Ok(Self {
            data: SubmissionData {
                owner: owner.clone(),
                encoder,
                fence,
                fence_value,
                commits: Vec::new(),
                uniform_writes: Vec::new(),
                resources: FastHashMap::default(),
                uploads: Vec::new(),
                bindings: BindingCache::default(),
            },
            state: SubmissionState::Recording { id: Rc::new(()) },
            #[cfg(test)]
            failure: None,
        })
    }

    fn restart(&mut self, fence_value: u64) -> Result<(), String> {
        assert!(matches!(self.state, SubmissionState::Retired));
        unsafe {
            self.data.encoder.begin_encoding(Some("WR Vulkan submission"))
        }.map_err(|error| {
            self.data.owner.lost.set(true);
            format!("Reusing submission encoder: {error:?}")
        })?;
        self.data.fence_value = fence_value;
        self.state = SubmissionState::Recording { id: Rc::new(()) };
        Ok(())
    }

    pub fn recording(&mut self) -> Result<Recording<'_>, String> {
        if self.data.owner.is_lost() {
            return Err("Submission is no longer recording".into());
        }
        match &self.state {
            SubmissionState::Recording { id } => Ok(Recording {
                submission: SubmissionBorrow::Direct(&mut self.data, id),
            }),
            _ => Err("Submission is no longer recording".into()),
        }
    }

    pub fn submit(&mut self) -> Result<(), String> {
        let data = &mut self.data;
        if data.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        if !matches!(self.state, SubmissionState::Recording { .. }) {
            return Err("Submission has already been attempted".into());
        }
        for buffer in &data.uniform_writes {
            buffer.flush_uniform_writes();
        }
        let SubmissionState::Recording { id } = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!()
        };
        self.state = SubmissionState::EncodingFailed { id };
        #[cfg(test)]
        if self.failure == Some(FailurePoint::EndEncoding) {
            data.owner.lost.set(true);
            return Err("Injected end-encoding failure".into());
        }
        let buffer = unsafe { data.encoder.end_encoding() }.map_err(|error| {
            data.owner.lost.set(true);
            format!("Finishing submission: {error:?}")
        })?;
        let SubmissionState::EncodingFailed { id } = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!()
        };
        self.state = SubmissionState::Unconfirmed { id, buffer };
        let SubmissionState::Unconfirmed { buffer, .. } = &self.state else { unreachable!() };
        let result = unsafe { data.owner.open.queue.submit(
            &[&**buffer], &[], (&**data.fence, data.fence_value),
        ) };
        #[cfg(test)]
        let result = if self.failure == Some(FailurePoint::AfterSubmit) {
            result.and(Err(hal::DeviceError::Lost))
        } else {
            result
        };
        result.map_err(|error| {
            data.owner.lost.set(true);
            format!("Submitting commands: {error:?}")
        })?;
        let SubmissionState::Unconfirmed { id, buffer } = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!()
        };
        self.state = SubmissionState::Submitted { buffer };
        data.uniform_writes.clear();
        for commit in data.commits.drain(..) {
            commit();
        }
        data.bindings.clear();
        drop(id);
        Ok(())
    }

    fn retire(&mut self) {
        let SubmissionState::Submitted { buffer } = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!("Only submitted work can retire")
        };
        let data = &mut self.data;
        unsafe { data.encoder.reset_all(vec![buffer]); }
        data.resources.clear();
        data.uniform_writes.clear();
        data.uploads.clear();
    }

    pub fn poll(&mut self) -> Result<bool, String> {
        match self.state {
            SubmissionState::Retired => return Ok(true),
            SubmissionState::Submitted { .. } => (),
            _ => return Err("Cannot poll an unsubmitted recording".into()),
        }
        let data = &self.data;
        let completed = unsafe { data.owner.open.device.get_fence_value(&**data.fence) }
            .map_err(|error| format!("Polling submission: {error:?}"))?;
        if completed >= data.fence_value {
            self.retire();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<bool, String> {
        match self.state {
            SubmissionState::Retired => return Ok(true),
            SubmissionState::Submitted { .. } => (),
            _ => return Err("Cannot wait for an unsubmitted recording".into()),
        }
        let data = &self.data;
        let completed = unsafe { data.owner.open.device.wait(&**data.fence, data.fence_value, timeout) }
            .map_err(|error| format!("Waiting for submission: {error:?}"))?;
        if completed {
            self.retire();
        }
        Ok(completed)
    }
}

impl Drop for Submission {
    fn drop(&mut self) {
        let data = &mut self.data;
        unsafe {
            if matches!(self.state, SubmissionState::EncodingFailed { .. }
                | SubmissionState::Unconfirmed { .. } | SubmissionState::Submitted { .. })
            {
                let _ = data.owner.open.queue.wait_for_idle();
                #[cfg(test)]
                data.owner.trace.borrow_mut().push(super::tests::Command::SubmissionIdleWait);
            }
            if matches!(self.state, SubmissionState::Recording { .. } | SubmissionState::EncodingFailed { .. }) {
                data.encoder.discard_encoding();
            }
            let (id, buffers) = match std::mem::replace(&mut self.state, SubmissionState::Retired) {
                SubmissionState::Recording { id } | SubmissionState::EncodingFailed { id } => (Some(id), Vec::new()),
                SubmissionState::Unconfirmed { id, buffer } => (Some(id), vec![buffer]),
                SubmissionState::Submitted { buffer } => (None, vec![buffer]),
                SubmissionState::Retired => (None, Vec::new()),
            };
            data.encoder.reset_all(buffers);
            data.bindings.clear();
            drop(id);
        }
        data.commits.clear();
        data.resources.clear();
        data.uniform_writes.clear();
        data.uploads.clear();
    }
}

#[cfg(test)]
#[path = "submission_state_tests.rs"]
mod tests;
