/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::time::{Duration, Instant};
use wgh::shared::{NativeRetirement, SharedSubmissionReceipt};
use wgh::shared_image::{ImageCloseStatus, ReturnedImage};

struct Fence {
    owner: Arc<wgh::DynOpenDevice>,
    raw: Option<Box<dyn wgh::DynFence>>,
}
impl Drop for Fence {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { self.owner.device.destroy_fence(raw) };
        }
    }
}

struct Marker {
    owner: Arc<ProcessOwner>,
    epoch: Arc<ImageEpoch>,
    _fence: Arc<Fence>,
    receipt: Option<Box<dyn SharedSubmissionReceipt>>,
}
impl Marker {
    fn completed(&mut self) {
        unsafe { self.epoch.marker_retired() };
        drop(self.receipt.take());
    }

    fn recover(&mut self, error: &wgh::DeviceError) {
        native_error(&self.owner, error);
        self.epoch.abandon();
        let retirement = match self.owner.context().drain() {
            Ok(()) => NativeRetirement::Confirmed,
            Err(error) => {
                native_error(&self.owner, &error);
                NativeRetirement::Unconfirmed
            }
        };
        if retirement == NativeRetirement::Confirmed {
            drop(self.receipt.take());
        }
        self.owner.recover_native_failure(retirement);
    }
}
impl Drop for Marker {
    fn drop(&mut self) {
        // An accepted receipt must survive until explicit retirement or process termination.
        if self.receipt.is_some() {
            self.owner.begin_shutdown();
            unsafe { (self.owner.fatal_process())() }
        }
    }
}

fn native_error(owner: &ProcessOwner, error: &wgh::DeviceError) {
    if matches!(error, wgh::DeviceError::Lost) {
        owner.begin_shutdown();
        unsafe { (owner.fatal_process())() }
    }
}

enum State {
    Published,
    Closing(Marker),
    Ready,
}
struct Record {
    owner: u64,
    info: NativeConfiguration,
    native: wgh::shared::SharedDeviceIdentity,
    device: Weak<Device>,
    queue: Weak<Queue>,
    frame: Weak<SealedCanvasFrame>,
    epoch: Arc<ImageEpoch>,
    state: State,
}
impl Record {
    fn matches(&self, owner: u64, info: &NativeConfiguration) -> bool {
        self.owner == owner
            && self.info.generation == info.generation
            && self.info.device_id == info.device_id
            && self.info.queue_id == info.queue_id
            && self.info.descriptor == info.descriptor
    }
}

pub(super) struct Returns {
    records: RefCell<Vec<Record>>,
    fence: RefCell<Option<Arc<Fence>>>,
    next_value: Cell<u64>,
    closed: Cell<bool>,
    owner: Arc<ProcessOwner>,
}
impl Returns {
    pub(super) fn new(owner: Arc<ProcessOwner>) -> Self {
        Self {
            records: RefCell::new(Vec::new()),
            fence: RefCell::new(None),
            next_value: Cell::new(1),
            closed: Cell::new(false),
            owner,
        }
    }

    pub(super) fn track(
        &self,
        owner: u64,
        info: &NativeConfiguration,
        device: &Arc<Device>,
        queue: Weak<Queue>,
        frame: &Arc<SealedCanvasFrame>,
    ) {
        if self.closed.get() {
            return;
        }
        let mut records = self.records.borrow_mut();
        if records
            .iter()
            .any(|record| Arc::ptr_eq(&record.epoch, frame.epoch()))
        {
            return;
        }
        records.push(Record {
            owner,
            info: info.clone(),
            native: self.owner.identity(),
            device: Arc::downgrade(device),
            queue,
            frame: Arc::downgrade(frame),
            epoch: frame.epoch().clone(),
            state: State::Published,
        });
    }

    fn fence(&self) -> Option<Arc<Fence>> {
        if let Some(fence) = self.fence.borrow().as_ref() {
            return Some(fence.clone());
        }
        let graphics = self.owner.graphics();
        let raw = match unsafe { graphics.device.create_fence() } {
            Ok(raw) => raw,
            Err(error) => {
                native_error(&self.owner, &error);
                return None;
            }
        };
        let fence = Arc::new(Fence {
            owner: graphics,
            raw: Some(raw),
        });
        *self.fence.borrow_mut() = Some(fence.clone());
        Some(fence)
    }

    pub(super) fn maintain(&self, configs: &[(u64, NativeConfiguration)]) {
        if self.closed.get() {
            return;
        }
        let records = std::mem::take(&mut *self.records.borrow_mut());
        let mut retained = Vec::with_capacity(records.len());
        for mut record in records {
            let live = self.owner.is_ready()
                && record.native == self.owner.identity()
                && configs
                    .iter()
                    .any(|(owner, info)| record.matches(*owner, info))
                && record
                    .device
                    .upgrade()
                    .is_some_and(|device| device.is_valid())
                && record.queue.upgrade().is_some();
            if let State::Closing(marker) = &mut record.state {
                match marker.receipt.as_ref().unwrap().poll() {
                    Ok(true) => {
                        marker.completed();
                        record.state = State::Ready;
                    }
                    Ok(false) => {
                        retained.push(record);
                        continue;
                    }
                    Err(error) => {
                        marker.recover(&error);
                        continue;
                    }
                }
            }
            if !live {
                continue;
            }
            if matches!(record.state, State::Published) && record.frame.upgrade().is_none() {
                let Some(fence) = self.fence() else {
                    retained.push(record);
                    continue;
                };
                let value = self.next_value.get();
                let Some(next) = value.checked_add(1) else {
                    continue;
                };
                let graphics = &fence.owner;
                match record
                    .epoch
                    .try_begin_close_if_unused(graphics.device.as_ref(), graphics.queue.as_ref())
                {
                    Ok(ImageCloseStatus::Busy) => {
                        retained.push(record);
                        continue;
                    }
                    Ok(ImageCloseStatus::AlreadyClosed) | Err(_) => continue,
                    Ok(ImageCloseStatus::Claimed) => {}
                }
                self.next_value.set(next);
                let result = unsafe {
                    self.owner.context().submit_graphics(
                        graphics.queue.as_ref(),
                        &[],
                        &[],
                        (fence.raw.as_ref().unwrap().as_ref(), value),
                        &wgh::external_sync::SubmissionSync::default(),
                    )
                };
                match result {
                    Ok(receipt) => {
                        unsafe { record.epoch.marker_accepted() };
                        record.state = State::Closing(Marker {
                            owner: self.owner.clone(),
                            epoch: record.epoch.clone(),
                            _fence: fence,
                            receipt: Some(receipt),
                        });
                    }
                    Err(mut error) => {
                        native_error(&self.owner, &error.error);
                        record.epoch.abandon();
                        if let Some(receipt) = error.receipt.take() {
                            let mut marker = Marker {
                                owner: self.owner.clone(),
                                epoch: record.epoch.clone(),
                                _fence: fence,
                                receipt: Some(receipt),
                            };
                            marker.recover(&error.error);
                        }
                        continue;
                    }
                }
            }
            retained.push(record);
        }
        self.records.borrow_mut().extend(retained);
    }

    pub(super) fn take_ready(
        &self,
        owner: u64,
        info: &NativeConfiguration,
        device: &Arc<Device>,
        queue: &Arc<Queue>,
    ) -> Option<ReturnedImage> {
        if self.closed.get() {
            return None;
        }
        let records = std::mem::take(&mut *self.records.borrow_mut());
        let mut retained = Vec::with_capacity(records.len());
        let mut returned = None;
        for record in records {
            if returned.is_none()
                && matches!(record.state, State::Ready)
                && record.matches(owner, info)
                && record.native == self.owner.identity()
                && Weak::ptr_eq(&record.device, &Arc::downgrade(device))
                && Weak::ptr_eq(&record.queue, &Arc::downgrade(queue))
            {
                if let Ok(Some(image)) = queue.try_take_returned_shared_image(&record.epoch) {
                    returned = Some(image);
                }
            } else {
                retained.push(record);
            }
        }
        self.records.borrow_mut().extend(retained);
        returned
    }

    pub(super) fn evict_ready(&self) {
        let records = std::mem::take(&mut *self.records.borrow_mut());
        let retained = records
            .into_iter()
            .filter(|record| !matches!(record.state, State::Ready))
            .collect::<Vec<_>>();
        self.records.borrow_mut().extend(retained);
    }

    pub(super) fn discard_owner(&self, owner: u64) {
        let records = std::mem::take(&mut *self.records.borrow_mut());
        let retained = records
            .into_iter()
            .filter(|record| record.owner != owner || matches!(record.state, State::Closing(_)))
            .collect::<Vec<_>>();
        self.records.borrow_mut().extend(retained);
    }

    pub(super) fn shutdown(&self) {
        self.closed.set(true);
        let records = std::mem::take(&mut *self.records.borrow_mut());
        let deadline = Instant::now() + Duration::from_secs(60);
        for mut record in records {
            if let State::Closing(marker) = &mut record.state {
                match marker
                    .receipt
                    .as_ref()
                    .unwrap()
                    .wait(Some(deadline.saturating_duration_since(Instant::now())))
                {
                    Ok(true) => marker.completed(),
                    Ok(false) => marker.recover(&wgh::DeviceError::Unexpected),
                    Err(error) => marker.recover(&error),
                }
            }
        }
        let fence = self.fence.borrow_mut().take();
        drop(fence);
    }
}
