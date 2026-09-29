/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, Device};
use std::any::Any;
use std::cell::RefMut;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::time::Duration;
use wgpu_hal::{CommandEncoder as _, Device as _, Queue as _};

#[path = "submission_queue.rs"]
mod queue;
pub use self::queue::SubmissionQueue;

pub struct Submission {
    owner: Rc<Device>,
    encoder: Option<hal::vulkan::CommandEncoder>,
    buffer: Option<hal::vulkan::CommandBuffer>,
    fence: Rc<Owned<hal::vulkan::Fence>>,
    fence_value: u64,
    recording: bool,
    attempted: bool,
    submitted: bool,
    complete: bool,
    recording_id: Option<Rc<()>>,
    commits: Vec<Box<dyn FnOnce()>>,
    resources: Vec<Box<dyn Any>>,
}

/// Exclusive access to an open submission; releasing this borrow does not submit it.
pub struct Recording<'a> {
    submission: SubmissionBorrow<'a>,
}

enum SubmissionBorrow<'a> {
    Direct(&'a mut Submission),
    Queued(RefMut<'a, Submission>),
}

impl Deref for SubmissionBorrow<'_> {
    type Target = Submission;

    fn deref(&self) -> &Submission {
        match self {
            Self::Direct(submission) => submission,
            Self::Queued(submission) => submission,
        }
    }
}

impl DerefMut for SubmissionBorrow<'_> {
    fn deref_mut(&mut self) -> &mut Submission {
        match self {
            Self::Direct(submission) => submission,
            Self::Queued(submission) => submission,
        }
    }
}

impl Recording<'_> {
    pub fn encoder(&mut self) -> &mut hal::vulkan::CommandEncoder {
        self.submission.encoder.as_mut().unwrap()
    }

    pub(super) fn recording_id(&self, owner: &Rc<Device>) -> Result<Rc<()>, String> {
        if !Rc::ptr_eq(owner, &self.submission.owner) {
            return Err("Vulkan recording device mismatch".into());
        }
        Ok(self.submission.recording_id.as_ref().unwrap().clone())
    }

    pub(super) fn commit(&mut self, commit: impl FnOnce() + 'static) {
        self.submission.commits.push(Box::new(commit));
    }

    pub fn keep<T: 'static>(&mut self, value: T) {
        self.submission.resources.push(Box::new(value));
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
            Rc::new(Owned::new(owner, fence, hal::vulkan::Device::destroy_fence)),
            1,
        )
    }

    fn with_fence(
        owner: &Rc<Device>,
        fence: Rc<Owned<hal::vulkan::Fence>>,
        fence_value: u64,
    ) -> Result<Self, String> {
        let mut submission = Self {
            owner: owner.clone(),
            encoder: None,
            buffer: None,
            fence,
            fence_value,
            recording: false,
            attempted: false,
            submitted: false,
            complete: false,
            recording_id: Some(Rc::new(())),
            commits: Vec::new(),
            resources: Vec::new(),
        };
        unsafe {
            submission.encoder = Some(
                owner
                    .open
                    .device
                    .create_command_encoder(&hal::CommandEncoderDescriptor {
                        label: Some("WR Vulkan submission"),
                        queue: &owner.open.queue,
                    })
                    .map_err(|error| format!("Creating submission encoder: {error:?}"))?,
            );
            submission
                .encoder
                .as_mut()
                .unwrap()
                .begin_encoding(Some("WR Vulkan submission"))
                .map_err(|error| format!("Beginning submission: {error:?}"))?;
        }
        submission.recording = true;
        Ok(submission)
    }

    fn restart(&mut self, fence_value: u64) -> Result<(), String> {
        assert!(self.complete && self.buffer.is_none());
        unsafe {
            self.encoder
                .as_mut()
                .unwrap()
                .begin_encoding(Some("WR Vulkan submission"))
        }
        .map_err(|error| {
            self.owner.lost.set(true);
            format!("Reusing submission encoder: {error:?}")
        })?;
        self.fence_value = fence_value;
        self.recording = true;
        self.attempted = false;
        self.submitted = false;
        self.complete = false;
        self.recording_id = Some(Rc::new(()));
        Ok(())
    }

    pub fn recording(&mut self) -> Result<Recording<'_>, String> {
        if !self.recording || self.attempted || self.owner.is_lost() {
            return Err("Submission is no longer recording".into());
        }
        Ok(Recording { submission: SubmissionBorrow::Direct(self) })
    }

    pub fn submit(&mut self) -> Result<(), String> {
        if self.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        if !self.recording || self.attempted {
            return Err("Submission has already been attempted".into());
        }
        self.attempted = true;
        unsafe {
            self.buffer = Some(
                self.encoder
                    .as_mut()
                    .unwrap()
                    .end_encoding()
                    .map_err(|error| {
                        self.owner.lost.set(true);
                        format!("Finishing submission: {error:?}")
                    })?,
            );
            self.recording = false;
            self.owner
                .open
                .queue
                .submit(
                    &[self.buffer.as_ref().unwrap()],
                    &[],
                    (&self.fence, self.fence_value),
                )
                .map_err(|error| {
                    self.owner.lost.set(true);
                    format!("Submitting commands: {error:?}")
                })?;
        }
        self.submitted = true;
        for commit in self.commits.drain(..) {
            commit();
        }
        self.recording_id.take();
        Ok(())
    }

    fn retire(&mut self) {
        let buffer = self.buffer.take();
        unsafe {
            self.encoder.as_mut().unwrap().reset_all(buffer.into_iter());
        }
        self.resources.clear();
        self.complete = true;
    }

    pub fn poll(&mut self) -> Result<bool, String> {
        if !self.submitted {
            return Err("Cannot poll an unsubmitted recording".into());
        }
        if !self.complete {
            let completed = unsafe { self.owner.open.device.get_fence_value(&self.fence) }
                .map_err(|error| format!("Polling submission: {error:?}"))?;
            if completed >= self.fence_value {
                self.retire();
            }
        }
        Ok(self.complete)
    }

    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<bool, String> {
        if !self.submitted {
            return Err("Cannot wait for an unsubmitted recording".into());
        }
        if !self.complete {
            let completed = unsafe {
                self.owner
                    .open
                    .device
                    .wait(&self.fence, self.fence_value, timeout)
            }
            .map_err(|error| format!("Waiting for submission: {error:?}"))?;
            if completed {
                self.retire();
            }
        }
        Ok(self.complete)
    }
}

impl Drop for Submission {
    fn drop(&mut self) {
        unsafe {
            if self.attempted && !self.complete {
                let _ = self.owner.open.queue.wait_for_idle();
            }
            if let Some(mut encoder) = self.encoder.take() {
                if self.recording {
                    encoder.discard_encoding();
                }
                encoder.reset_all(self.buffer.take().into_iter());
            }
        }
        self.recording_id.take();
    }
}
