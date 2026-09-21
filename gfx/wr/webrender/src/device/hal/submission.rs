/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::Owned;
use std::any::Any;
use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::rc::Rc;

pub(super) trait SubmissionSync<A: hal::Api>: 'static {
    fn stage(&self, queue: &A::Queue);
    fn unstage(&self, queue: &A::Queue);
}

struct StagedSync<'a, A: hal::Api> {
    sync: &'a [Rc<dyn SubmissionSync<A>>],
    queue: &'a A::Queue,
}
impl<A: hal::Api> Drop for StagedSync<'_, A> {
    fn drop(&mut self) { for sync in self.sync { sync.unstage(self.queue); } }
}

pub(super) type CompletionCheck = Box<dyn Fn(bool) -> Result<bool>>;
pub(super) type CompletionProbe<A> = fn(&<A as hal::Api>::CommandEncoder, &Device<A>) -> Result<CompletionCheck>;

pub(super) struct Submission<A: hal::Api> {
    owner: Rc<Device<A>>,
    encoder: Option<A::CommandEncoder>,
    buffer: Option<A::CommandBuffer>,
    fence: Rc<Owned<A, A::Fence>>,
    recording: bool,
    attempted: bool,
    complete: bool,
    counted_pending: bool,
    completion_check: Option<CompletionCheck>,
    serial: u64,
    resources: Vec<Box<dyn Any>>,
    sync: Vec<Rc<dyn SubmissionSync<A>>>,
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
            counted_pending: false,
            completion_check: None,
            serial,
            resources: Vec::new(),
            sync: Vec::new(),
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
        submission.completion_check = owner.completion_probe.map(|probe| probe(submission.encoder.as_ref().unwrap(), owner)).transpose()?;
        Ok(submission)
    }

    fn recycle(&mut self) {
        assert!(self.complete);
        if self.counted_pending {
            if let Some(metrics) = &self.owner.metrics {
                metrics.add(diagnostics::RenderCounter::CompletedSubmissions, 1);
                metrics.release(diagnostics::RenderGauge::PendingSubmissions);
            }
            self.counted_pending = false;
        }
        let buffer = self.buffer.take();
        unsafe {
            self.encoder().reset_all(buffer.into_iter());
        }
        for complete in self.completions.drain(..) { complete(); }
        self.completion_check = None;
        self.resources.clear();
        self.sync.clear();
    }

    fn restart(&mut self, serial: u64) -> Result<()> {
        assert!(self.complete && self.buffer.is_none());
        self.serial = serial;
        self.complete = false;
        self.attempted = false;
        unsafe { self.encoder().begin_encoding(Some("WR submission")) }
            .map_err(|error| format!("Reusing command encoder: {error:?}"))?;
        self.recording = true;
        self.completion_check = self.owner.completion_probe.map(|probe| probe(self.encoder.as_ref().unwrap(), &self.owner)).transpose()?;
        Ok(())
    }

    pub fn encoder(&mut self) -> &mut A::CommandEncoder {
        self.encoder.as_mut().unwrap()
    }

    pub fn keep<T: 'static>(&mut self, value: T) {
        self.resources.push(Box::new(value));
    }

    pub fn synchronize(&mut self, sync: Rc<dyn SubmissionSync<A>>) { self.sync.push(sync); }

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
            let result = {
                let _guard = self.owner.lock_queue()?;
                let _staged = StagedSync { sync: &self.sync, queue: &self.owner.open.queue };
                for sync in &self.sync { sync.stage(&self.owner.open.queue); }
                let result = self.owner.open.queue.submit(
                    &[self.buffer.as_ref().unwrap()], surfaces, (&self.fence, self.serial));
                result
            };
            result.map_err(|e| format!("Submitting WR commands: {e:?}"))?;
        }
        if let Some(metrics) = &self.owner.metrics {
            metrics.add(diagnostics::RenderCounter::QueueSubmissions, 1);
            if !surfaces.is_empty() { metrics.add(diagnostics::RenderCounter::SurfaceSubmissions, 1); }
            metrics.retain(diagnostics::RenderGauge::PendingSubmissions);
            self.counted_pending = true;
        }
        for commit in self.commits.drain(..) {
            commit();
        }
        Ok(())
    }

    fn fence_value(&self) -> Result<u64> {
        unsafe {
            self.owner
                .open
                .device
                .get_fence_value(&self.fence)
                .map_err(|e| format!("Polling WR submission: {e:?}"))
        }
    }

    fn poll(&mut self, completed: u64) -> Result<bool> {
        self.complete = completed >= self.serial;
        if self.complete {
            if let Some(check) = &self.completion_check {
                self.complete = check(false).map_err(|error| { self.owner.lost.set(true); error })?;
            }
        }
        Ok(self.complete)
    }

    fn wait(&mut self, timeout: Option<std::time::Duration>) -> Result<()> {
        self.complete = unsafe {
            self.owner
                .open
                .device
                .wait(&self.fence, self.serial, timeout)
                .map_err(|e| format!("Waiting for WR submission: {e:?}"))?
        };
        if self.complete {
            if let Some(check) = &self.completion_check {
                self.complete = check(true).map_err(|error| { self.owner.lost.set(true); error })?;
            }
        }
        if self.complete {
            Ok(())
        } else {
            self.owner.lost.set(true);
            Err("WR submission did not complete".into())
        }
    }
}

impl<A: hal::Api> Drop for Submission<A> {
    fn drop(&mut self) {
        unsafe {
            if self.attempted && !self.complete {
                let _guard = self.owner.queue_gate.lock().unwrap_or_else(|error| error.into_inner());
                let _ = self.owner.open.queue.wait_for_idle();
            }
            if self.counted_pending {
                if let Some(metrics) = &self.owner.metrics {
                    metrics.release(diagnostics::RenderGauge::PendingSubmissions);
                }
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
    wait_timeout: Option<std::time::Duration>,
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
            wait_timeout: None,
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

    pub fn with_wait_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.wait_timeout = Some(timeout);
        self
    }

    pub fn submitted(&self) -> u64 { self.state.borrow().submitted }

    fn retire(state: &mut QueueState<A>, wait: bool, timeout: Option<std::time::Duration>) -> Result<()> {
        let completed = if wait { None } else {
            state.pending.front().map(Submission::fence_value).transpose()?
        };
        while let Some(front) = state.pending.front_mut() {
            if wait {
                front.wait(timeout)?;
                state.waits += 1;
            } else if !front.poll(completed.unwrap())? {
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
        Self::retire(&mut self.state.borrow_mut(), false, self.wait_timeout)?;
        self.uploads.upload(bytes, usage)
    }

    pub fn upload_in_recording(
        &self,
        recording: &Submission<A>,
        length: usize,
        usage: wgt::BufferUses,
        write: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<Rc<super::resources::Buffer<A>>> {
        if !Rc::ptr_eq(&recording.owner, &self.owner) || !recording.recording {
            return Err("HAL upload requires a recording on the same device".into());
        }
        self.uploads.upload_with(length, usage, write)
    }

    #[cfg(test)]
    pub fn upload_recording(
        &self,
        bytes: &[u8],
        usage: wgt::BufferUses,
    ) -> Result<(RefMut<'_, Submission<A>>, Rc<super::resources::Buffer<A>>)> {
        self.upload_recording_with(bytes.len(), usage, |destination| {
            destination.copy_from_slice(bytes);
            Ok(())
        })
    }

    pub fn upload_recording_with(
        &self,
        length: usize,
        usage: wgt::BufferUses,
        write: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<(RefMut<'_, Submission<A>>, Rc<super::resources::Buffer<A>>)> {
        let recording = self.recording()?;
        let buffer = self.uploads.upload_with(length, usage, write)?;
        Ok((recording, buffer))
    }

    pub fn has_pending_work(&self) -> bool { !self.state.borrow().pending.is_empty() }

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
        if self.owner.lost.get() { return Err("HAL device requires recreation".into()); }
        #[cfg(any(test, feature = "hal-testing"))]
        self.owner.check_fault(FailurePoint::Record)?;
        let mut state = self.state.borrow_mut();
        if state.active.is_none() {
            #[cfg(test)]
            let poll = !self.defer_poll.get();
            #[cfg(not(test))]
            let poll = true;
            if poll {
                Self::retire(&mut state, false, self.wait_timeout)?;
            }
            if state.pending.len() == self.limit {
                Self::retire(&mut state, true, self.wait_timeout)?;
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
            if self.owner.lost.get() { return Err("HAL device requires recreation".into()); }
            #[cfg(any(test, feature = "hal-testing"))]
            self.owner.check_fault(FailurePoint::Submit)?;
            active.submit(surfaces).map_err(|error| { self.owner.lost.set(true); error })?;
            state.submitted = active.serial;
            state.pending.push_back(active);
            state.peak_pending = state.peak_pending.max(state.pending.len());
        }
        if self.synchronous {
            while !state.pending.is_empty() {
                Self::retire(&mut state, true, self.wait_timeout)?;
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
        Self::retire(&mut state, false, self.wait_timeout)?;
        Ok(state.completed)
    }

    pub fn wait_for(&self, serial: u64) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if serial > state.submitted {
            return Err("Cannot wait for an unsubmitted HAL serial".into());
        }
        while state.completed < serial {
            Self::retire(&mut state, true, self.wait_timeout)?;
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
            Self::retire(&mut state, true, self.wait_timeout)?;
        }
        Ok(())
    }
}

impl<A: hal::Api> Drop for SubmissionQueue<A> {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        state.active.take();
        while !state.pending.is_empty() {
            if Self::retire(state, true, self.wait_timeout).is_err() {
                break;
            }
        }
    }
}

#[cfg(all(test, wr_hal_vulkan))]
mod tests {
    use super::*;
    use super::super::resources::{Buffer, Texture};

    #[test]
    #[ignore = "Requires Vulkan"]
    fn native_completion_probe_gates_retirement_and_errors() {
        use diagnostics::{RenderCounter, RenderGauge, RenderMetrics};
        for fail in [false, true] {
            let mut device = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
            let metrics = RenderMetrics::for_test(device.cache_id, 0);
            device.metrics = Some(metrics.clone());
            device.completion_probe = Some(if fail {
                |_, _| Ok(Box::new(|_| Err("native command buffer failed".into())))
            } else {
                |_, _| Ok(Box::new(|wait| Ok(wait)))
            });
            let owner = Rc::new(device);
            let queue = SubmissionQueue::new(&owner, 3, false);
            let complete = Rc::new(std::cell::Cell::new(false));
            let notice = complete.clone();
            queue.recording().unwrap().on_complete(move || notice.set(true));
            queue.submit().unwrap();
            assert_eq!(metrics.snapshot().count(RenderCounter::QueueSubmissions), 1);
            assert_eq!(metrics.snapshot().gauge(RenderGauge::PendingSubmissions), 1);
            unsafe { owner.open.queue.wait_for_idle() }.unwrap();
            let result = queue.poll();
            assert!(!complete.get());
            if fail {
                assert!(result.unwrap_err().contains("native command buffer failed"));
                assert!(owner.lost.get());
                assert!(queue.recording().is_err());
            } else {
                assert_eq!(result.unwrap(), 0);
                queue.wait().unwrap();
                assert!(complete.get());
                assert!(!owner.lost.get());
            }
            drop(queue);
            assert_eq!(metrics.snapshot().gauge(RenderGauge::PendingSubmissions), 0);
            assert_eq!(metrics.snapshot().count(RenderCounter::CompletedSubmissions), u64::from(!fail));
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn failed_submit_does_not_publish_completion() {
        use diagnostics::{RenderCounter, RenderGauge, RenderMetrics};
        let mut device = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let metrics = RenderMetrics::for_test(device.cache_id, 0);
        device.metrics = Some(metrics.clone());
        let owner = Rc::new(device);
        let queue = SubmissionQueue::new(&owner, 3, false);
        let completed = Rc::new(std::cell::Cell::new(false));
        let notice = completed.clone();
        queue.recording().unwrap().on_complete(move || notice.set(true));
        owner.fault.set(Some(FailurePoint::Submit));
        assert!(queue.submit().is_err());
        assert_eq!(queue.state.borrow().submitted, 0);
        assert_eq!(queue.poll().unwrap(), 0);
        assert!(queue.wait_for(1).is_err());
        assert!(!completed.get());
        assert!(queue.recording().is_err());
        assert_eq!(metrics.snapshot().count(RenderCounter::QueueSubmissions), 0);
        assert_eq!(metrics.snapshot().gauge(RenderGauge::PendingSubmissions), 0);
    }

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
        let (recording, upload) = queue.upload_recording(&[0; 16], wgt::BufferUses::VERTEX).unwrap();
        drop(recording);
        drop(upload);
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

    #[test]
    #[ignore = "Requires Vulkan"]
    fn retirement_preserves_native_completion_prefix() {
        let owner = Rc::new(create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap());
        let queue = SubmissionQueue::new(&owner, 3, false);
        queue.defer_poll.set(true);
        let releases = Rc::new(RefCell::new(Vec::new()));
        for serial in 1..=3 {
            let notices = releases.clone();
            queue.recording().unwrap().on_complete(move || notices.borrow_mut().push(serial));
            queue.submit().unwrap();
        }
        let ready = Rc::new(std::cell::Cell::new(false));
        let check = ready.clone();
        queue.state.borrow_mut().pending[1].completion_check = Some(Box::new(move |_| Ok(check.get())));
        unsafe { owner.open.queue.wait_for_idle() }.unwrap();
        assert_eq!(queue.poll().unwrap(), 1);
        assert_eq!(*releases.borrow(), [1]);
        assert_eq!(queue.state.borrow().pending.len(), 2);
        ready.set(true);
        assert_eq!(queue.poll().unwrap(), 3);
        assert_eq!(*releases.borrow(), [1, 2, 3]);
        assert!(queue.state.borrow().pending.is_empty());
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn recording_uploads_keep_distinct_live_buffers() {
        let owner = Rc::new(create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap());
        let queue = SubmissionQueue::new(&owner, 3, false);
        let mut commands = queue.recording().unwrap();
        let first = queue.upload_in_recording(&commands, 16, wgt::BufferUses::VERTEX, |bytes| {
            bytes.fill(17);
            Ok(())
        }).unwrap();
        let second = queue.upload_in_recording(&commands, 16, wgt::BufferUses::VERTEX, |bytes| {
            bytes.fill(23);
            Ok(())
        }).unwrap();
        assert_ne!(first.allocation_id, second.allocation_id);
        first.transition(&mut commands, wgt::BufferUses::VERTEX);
        second.transition(&mut commands, wgt::BufferUses::VERTEX);
        drop(commands);
        assert_eq!(queue.submitted(), 0);
        queue.wait().unwrap();
        assert_eq!(queue.submitted(), 1);
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn failed_upload_fill_discards_mapping_without_submission() {
        let owner = Rc::new(create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap());
        let queue = SubmissionQueue::new(&owner, 3, false);
        for cached in [false, true] {
            if cached {
                drop(queue.upload_recording_with(16, wgt::BufferUses::COPY_SRC, |bytes| {
                    bytes.fill(17);
                    Ok(())
                }).unwrap());
                assert_eq!(owner.memory.get().buffers, 1);
            }
            assert!(queue.upload_recording_with(16, wgt::BufferUses::COPY_SRC, |bytes| {
                bytes[..4].fill(23);
                Err("Injected upload fill failure".into())
            }).is_err());
            assert_eq!(queue.submitted(), 0);
            assert_eq!(owner.memory.get().buffers, 0);
            queue.discard_recording();
            assert!(queue.state.borrow().active.is_none());
        }
    }
}
