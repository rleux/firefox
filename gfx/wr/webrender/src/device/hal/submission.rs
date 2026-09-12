/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::Owned;
use std::any::Any;
use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::rc::Rc;

pub(super) struct Submission<A: hal::Api> {
    owner: Rc<Device<A>>,
    encoder: Option<A::CommandEncoder>,
    buffer: Option<A::CommandBuffer>,
    fence: Rc<Owned<A, A::Fence>>,
    recording: bool,
    attempted: bool,
    complete: bool,
    serial: u64,
    resources: Vec<Box<dyn Any>>,
    commits: Vec<Box<dyn FnOnce()>>,
    completions: Vec<Box<dyn FnOnce()>>,
}

impl<A: hal::Api> Submission<A> {
    fn new(owner: &Rc<Device<A>>, serial: u64, fence: Rc<Owned<A, A::Fence>>) -> Result<Self> {
        let mut submission = Self {
            owner: owner.clone(),
            encoder: None,
            buffer: None,
            fence,
            recording: false,
            attempted: false,
            complete: false,
            serial,
            resources: Vec::new(),
            commits: Vec::new(),
            completions: Vec::new(),
        };
        unsafe {
            submission.encoder = Some(
                owner
                    .open
                    .device
                    .create_command_encoder(&hal::CommandEncoderDescriptor {
                        label: Some("WR submission"),
                        queue: &owner.open.queue,
                    })
                    .map_err(|e| format!("Creating submission encoder: {e:?}"))?,
            );
            submission
                .encoder()
                .begin_encoding(Some("WR submission"))
                .map_err(|e| format!("Beginning submission: {e:?}"))?;
        }
        submission.recording = true;
        Ok(submission)
    }

    fn recycle(&mut self) {
        assert!(self.complete);
        let buffer = self.buffer.take();
        unsafe {
            self.encoder().reset_all(buffer.into_iter());
        }
        for complete in self.completions.drain(..) { complete(); }
        self.resources.clear();
    }

    fn restart(&mut self, serial: u64) -> Result<()> {
        assert!(self.complete && self.buffer.is_none());
        self.serial = serial;
        self.complete = false;
        self.attempted = false;
        unsafe { self.encoder().begin_encoding(Some("WR submission")) }
            .map_err(|error| format!("Reusing command encoder: {error:?}"))?;
        self.recording = true;
        Ok(())
    }

    pub fn encoder(&mut self) -> &mut A::CommandEncoder {
        self.encoder.as_mut().unwrap()
    }

    pub fn keep<T: 'static>(&mut self, value: T) {
        self.resources.push(Box::new(value));
    }

    pub fn commit(&mut self, commit: impl FnOnce() + 'static) {
        self.commits.push(Box::new(commit));
    }

    pub fn on_complete(&mut self, complete: impl FnOnce() + 'static) {
        self.completions.push(Box::new(complete));
    }

    fn submit(&mut self, surfaces: &[&A::SurfaceTexture]) -> Result<()> {
        unsafe {
            self.buffer = Some(
                self.encoder()
                    .end_encoding()
                    .map_err(|e| format!("Finishing submission: {e:?}"))?,
            );
            self.recording = false;
            self.attempted = true;
            self.owner
                .open
                .queue
                .submit(
                    &[self.buffer.as_ref().unwrap()],
                    surfaces,
                    (&self.fence, self.serial),
                )
                .map_err(|e| format!("Submitting WR commands: {e:?}"))?;
        }
        for commit in self.commits.drain(..) {
            commit();
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<bool> {
        self.complete = unsafe {
            self.owner
                .open
                .device
                .get_fence_value(&self.fence)
                .map_err(|e| format!("Polling WR submission: {e:?}"))?
                >= self.serial
        };
        Ok(self.complete)
    }

    fn wait(&mut self) -> Result<()> {
        self.complete = unsafe {
            self.owner
                .open
                .device
                .wait(&self.fence, self.serial, None)
                .map_err(|e| format!("Waiting for WR submission: {e:?}"))?
        };
        if self.complete {
            Ok(())
        } else {
            Err("WR submission did not complete".into())
        }
    }
}

impl<A: hal::Api> Drop for Submission<A> {
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
    }
}

struct QueueState<A: hal::Api> {
    active: Option<Submission<A>>,
    pending: VecDeque<Submission<A>>,
    next_serial: u64,
    completed: u64,
    submitted: u64,
    peak_pending: usize,
    waits: usize,
    ready: Vec<Submission<A>>,
}

pub(super) struct SubmissionQueue<A: hal::Api> {
    owner: Rc<Device<A>>,
    state: RefCell<QueueState<A>>,
    limit: usize,
    synchronous: bool,
    uploads: super::pool::BufferPool<A>,
    fence: RefCell<Option<Rc<Owned<A, A::Fence>>>>,
    #[cfg(test)]
    defer_poll: std::cell::Cell<bool>,
}

impl<A: hal::Api> SubmissionQueue<A> {
    pub fn new(owner: &Rc<Device<A>>, limit: usize, synchronous: bool) -> Self {
        assert!(limit > 0);
        Self {
            owner: owner.clone(),
            limit,
            synchronous,
            uploads: super::pool::BufferPool::new(owner),
            fence: RefCell::new(None),
            #[cfg(test)]
            defer_poll: std::cell::Cell::new(false),
            state: RefCell::new(QueueState {
                active: None,
                pending: VecDeque::new(),
                next_serial: 1,
                completed: 0,
                submitted: 0,
                peak_pending: 0,
                waits: 0,
                ready: Vec::new(),
            }),
        }
    }

    pub fn fence(&self) -> Result<Rc<Owned<A, A::Fence>>> {
        let mut fence = self.fence.borrow_mut();
        if fence.is_none() {
            let raw = unsafe { self.owner.open.device.create_fence() }
                .map_err(|error| format!("Creating submission fence: {error:?}"))?;
            *fence = Some(Rc::new(Owned::new(&self.owner, raw, A::Device::destroy_fence)));
        }
        Ok(fence.as_ref().unwrap().clone())
    }

    fn retire(state: &mut QueueState<A>, wait: bool) -> Result<()> {
        while let Some(front) = state.pending.front_mut() {
            if wait {
                front.wait()?;
                state.waits += 1;
            } else if !front.poll()? {
                break;
            }
            state.completed = front.serial;
            let mut completed = state.pending.pop_front().unwrap();
            completed.recycle();
            state.ready.push(completed);
            if wait {
                break;
            }
        }
        Ok(())
    }

    pub fn upload(
        &self,
        bytes: &[u8],
        usage: wgt::BufferUses,
    ) -> Result<Rc<super::resources::Buffer<A>>> {
        Self::retire(&mut self.state.borrow_mut(), false)?;
        self.uploads.upload(bytes, usage)
    }

    pub fn memory(&self, stats: &mut MemoryStats) {
        let state = self.state.borrow();
        stats.in_flight = state.pending.len();
        stats.retained_references = state
            .pending
            .iter()
            .map(|submission| submission.resources.len())
            .sum::<usize>()
            + state
                .active
                .as_ref()
                .map_or(0, |submission| submission.resources.len());
        stats.cached_buffer_bytes = self.uploads.bytes();
    }

    pub fn trim(&self, uploads: bool) -> Result<()> {
        self.poll()?;
        self.state.borrow_mut().ready.clear();
        if uploads { self.uploads.clear(); }
        Ok(())
    }

    pub fn recording(&self) -> Result<RefMut<'_, Submission<A>>> {
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
                .ok_or("WR submission serial overflow")?;
            state.active = Some(if let Some(mut ready) = state.ready.pop() {
                ready.restart(serial)?;
                ready
            } else {
                Submission::new(&self.owner, serial, self.fence()?)?
            });
        }
        Ok(RefMut::map(state, |state| state.active.as_mut().unwrap()))
    }

    pub fn submit(&self) -> Result<()> { self.submit_surfaces(&[]) }

    pub fn submit_surfaces(&self, surfaces: &[&A::SurfaceTexture]) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if let Some(mut active) = state.active.take() {
            active.submit(surfaces)?;
            state.submitted = active.serial;
            state.pending.push_back(active);
            state.peak_pending = state.peak_pending.max(state.pending.len());
        }
        if self.synchronous {
            while !state.pending.is_empty() {
                Self::retire(&mut state, true)?;
            }
        }
        Ok(())
    }

    pub fn submit_serial(&self) -> Result<u64> {
        self.submit()?;
        Ok(self.state.borrow().submitted)
    }

    pub fn poll(&self) -> Result<u64> {
        let mut state = self.state.borrow_mut();
        Self::retire(&mut state, false)?;
        Ok(state.completed)
    }

    pub fn wait_for(&self, serial: u64) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if serial > state.submitted {
            return Err("Cannot wait for an unsubmitted HAL serial".into());
        }
        while state.completed < serial {
            Self::retire(&mut state, true)?;
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        self.state.borrow_mut().active.take();
        let _ = self.wait();
        let mut state = self.state.borrow_mut();
        state.active.take();
        state.pending.clear();
    }

    pub fn discard_recording(&self) {
        self.state.borrow_mut().active.take();
    }

    pub fn wait(&self) -> Result<()> {
        self.submit()?;
        let mut state = self.state.borrow_mut();
        while !state.pending.is_empty() {
            Self::retire(&mut state, true)?;
        }
        Ok(())
    }
}

impl<A: hal::Api> Drop for SubmissionQueue<A> {
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
mod tests {
    use super::*;
    use super::super::resources::{Buffer, Texture};

    #[test]
    #[ignore = "Requires Vulkan"]
    fn retirement_backpressure_and_abandoned_recording() {
        let owner = Rc::new(
            create_vulkan_device(&Options {
                validation: true,
                ..Options::default()
            })
            .unwrap(),
        );
        let queue = SubmissionQueue::new(&owner, 2, false);
        queue.defer_poll.set(true);
        let texture = Texture::new(
            &owner,
            4,
            4,
            wgt::TextureFormat::Rgba8Unorm,
            crate::device::TextureFilter::Nearest,
            false,
        )
        .unwrap();
        let weak = Rc::downgrade(&texture);
        texture
            .upload_recorded(
                &owner,
                &queue,
                api::units::DeviceIntRect::from_size(api::units::DeviceIntSize::new(4, 4)),
                &[128; 64],
                None,
                0,
                None,
            )
            .unwrap();
        assert_eq!(texture.committed_usage(), wgt::TextureUses::UNINITIALIZED);
        queue.submit().unwrap();
        assert_eq!(texture.committed_usage(), wgt::TextureUses::RESOURCE);
        drop(texture);
        assert!(weak.upgrade().is_some());
        let retained = Rc::new(0u8);
        let second = Rc::downgrade(&retained);
        queue.recording().unwrap().keep(retained);
        queue.submit().unwrap();
        assert_eq!(queue.state.borrow().pending.len(), 2);
        queue.recording().unwrap();
        assert!(weak.upgrade().is_none());
        assert!(second.upgrade().is_some());
        assert_eq!(queue.state.borrow().waits, 1);
        queue.submit().unwrap();
        queue.wait().unwrap();
        assert!(second.upgrade().is_none());
        assert_eq!(queue.state.borrow().completed, 3);
        assert_eq!(queue.state.borrow().peak_pending, 2);
        let buffer = Buffer::new(&owner, &[0; 16], wgt::BufferUses::VERTEX).unwrap();
        let abandoned = Rc::downgrade(&buffer);
        buffer.transition(&mut queue.recording().unwrap(), wgt::BufferUses::VERTEX);
        drop(buffer);
        assert!(abandoned.upgrade().is_some());
        queue.shutdown();
        assert!(abandoned.upgrade().is_none());
        assert_eq!(queue.state.borrow().completed, 3);
    }
}
