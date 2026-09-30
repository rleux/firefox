/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, Owned, Recording, Submission, SubmissionBorrow, SubmissionState};
use super::super::{wgt, Buffer, BufferPool, Device};
use crate::device::{Fence, FenceStatus};
use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::convert::TryFrom;
use std::rc::Rc;

#[path = "instance_buffers.rs"]
mod instances;
pub use self::instances::InstanceBuffers;

struct QueueState {
    active: Option<Submission>,
    pending: VecDeque<Submission>,
    ready: Vec<Submission>,
    next_serial: u64,
    submitted: u64,
    completed: u64,
}

pub struct SubmissionQueue {
    pool: Rc<BufferPool>,
    fence: Rc<Owned<dyn hal::DynFence>>,
    state: RefCell<QueueState>,
    limit: usize,
    #[cfg(test)]
    defer_poll: std::cell::Cell<bool>,
}

pub(super) struct RecycleUpload {
    pub(super) pool: Rc<BufferPool>,
    pub(super) buffer: Rc<Buffer>,
}

impl Drop for RecycleUpload {
    fn drop(&mut self) {
        self.pool.recycle(self.buffer.clone());
    }
}

impl SubmissionQueue {
    pub(in crate::device::wgpu) fn owner(&self) -> &Rc<Device> {
        &self.pool.owner
    }

    pub fn new(pool: &Rc<BufferPool>, limit: usize) -> Result<Self, String> {
        if limit == 0 || pool.owner.is_lost() {
            return Err("Invalid Vulkan submission limit or lost device".into());
        }
        let fence = unsafe { pool.owner.open.device.create_fence() }
            .map_err(|error| format!("Creating queue fence: {error:?}"))?;
        Ok(Self {
            pool: pool.clone(),
            fence: Rc::new(Owned::new(
                &pool.owner,
                fence,
                <dyn hal::DynDevice>::destroy_fence,
            )),
            state: RefCell::new(QueueState {
                active: None,
                pending: VecDeque::new(),
                ready: Vec::new(),
                next_serial: 1,
                submitted: 0,
                completed: 0,
            }),
            limit,
            #[cfg(test)]
            defer_poll: std::cell::Cell::new(false),
        })
    }

    fn retire(state: &mut QueueState, wait: bool) -> Result<(), String> {
        while let Some(front) = state.pending.front_mut() {
            let result = if wait { front.wait(None) } else { front.poll() };
            let complete = result.map_err(|error| {
                front.data.owner.lost.set(true);
                error
            })?;
            if !complete {
                if wait {
                    front.data.owner.lost.set(true);
                    return Err("Vulkan submission did not complete".into());
                }
                break;
            }
            state.completed = front.data.fence_value;
            state.ready.push(state.pending.pop_front().unwrap());
            if wait {
                break;
            }
        }
        Ok(())
    }

    pub fn recording(&self) -> Result<Recording<'_>, String> {
        if self.pool.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let mut state = self.state.borrow_mut();
        if state.active.is_none() {
            #[cfg(test)]
            let poll = !self.defer_poll.get();
            #[cfg(not(test))]
            let poll = true;
            if poll {
                Self::retire(&mut state, false)?;
            }
            if state.pending.len() == self.limit {
                Self::retire(&mut state, true)?;
            }
            let serial = state.next_serial;
            state.next_serial = serial
                .checked_add(1)
                .ok_or("Vulkan submission serial overflow")?;
            state.active = Some(if let Some(mut ready) = state.ready.pop() {
                ready.restart(serial)?;
                ready
            } else {
                Submission::with_fence(&self.pool.owner, self.fence.clone(), serial, &self.pool)?
            });
        }
        let (data, id) = RefMut::map_split(state, |state| {
            let submission = state.active.as_mut().unwrap();
            let SubmissionState::Recording { id } = &mut submission.state else {
                unreachable!("Queue active submission is not recording")
            };
            (&mut submission.data, id)
        });
        Ok(Recording {
            submission: SubmissionBorrow::Queued(data, id),
        })
    }

    pub fn upload_in_recording(
        &self,
        recording: &mut Recording<'_>,
        length: usize,
        usage: wgt::BufferUses,
        write: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<Rc<Buffer>, String> {
        recording.recording_id(&self.pool.owner)?;
        let buffer = self.pool.upload_with(length, usage, write)?;
        recording.keep_upload(RecycleUpload {
            pool: self.pool.clone(),
            buffer: buffer.clone(),
        });
        Ok(buffer)
    }

    pub fn submit(&self) -> Result<u64, String> {
        if self.pool.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let mut state = self.state.borrow_mut();
        if let Some(mut active) = state.active.take() {
            active.submit().map_err(|error| {
                self.pool.owner.lost.set(true);
                error
            })?;
            state.submitted = active.data.fence_value;
            state.pending.push_back(active);
        }
        Ok(state.submitted)
    }

    pub fn poll(&self) -> Result<u64, String> {
        let mut state = self.state.borrow_mut();
        Self::retire(&mut state, false)?;
        Ok(state.completed)
    }

    pub fn create_fence(&self) -> Result<Fence, String> {
        let serial = self.submit()?;
        Ok(Fence(
            usize::try_from(serial).map_err(|_| "Vulkan fence serial exceeds handle range")?,
        ))
    }

    pub fn poll_fence(&self, fence: &Fence) -> FenceStatus {
        let serial = fence.0 as u64;
        if self.pool.owner.is_lost() || serial > self.state.borrow().submitted {
            return FenceStatus::Error;
        }
        match self.poll() {
            Ok(completed) if completed >= serial => FenceStatus::Signaled,
            Ok(_) => FenceStatus::Pending,
            Err(_) => FenceStatus::Error,
        }
    }

    pub fn wait_for(&self, serial: u64) -> Result<(), String> {
        let mut state = self.state.borrow_mut();
        if serial > state.submitted {
            return Err("Cannot wait for an unsubmitted Vulkan serial".into());
        }
        while state.completed < serial {
            Self::retire(&mut state, true)?;
        }
        Ok(())
    }

    pub fn wait(&self) -> Result<u64, String> {
        let serial = self.submit()?;
        self.wait_for(serial)?;
        Ok(serial)
    }

    pub fn has_pending_work(&self) -> bool {
        !self.state.borrow().pending.is_empty()
    }

    pub fn discard_recording(&self) {
        self.state.borrow_mut().active.take();
    }

    pub fn trim(&self) -> Result<(), String> {
        self.poll()?;
        self.state.borrow_mut().ready.clear();
        self.pool.clear();
        Ok(())
    }
}

impl Drop for SubmissionQueue {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        state.active.take();
        while !state.pending.is_empty() {
            if Self::retire(state, true).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
#[path = "submission_queue_tests.rs"]
mod tests;
