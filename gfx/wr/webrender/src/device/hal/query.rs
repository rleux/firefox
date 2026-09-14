/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::{Buffer, Owned};
use super::submission::SubmissionQueue;
use std::{collections::VecDeque, convert::TryInto, rc::Rc};

pub(super) struct Slot<A: hal::Api> {
    query: Owned<A, A::QuerySet>,
    buffer: Rc<Buffer<A>>,
}

pub(super) struct QueryPool<A: hal::Api> {
    owner: Rc<Device<A>>,
    bits: u32,
    period: f64,
    enabled: bool,
    slots: Vec<Rc<Slot<A>>>,
    pending: VecDeque<(u64, Rc<Slot<A>>)>,
    results: VecDeque<(u64, f64)>,
}

impl<A: hal::Api> QueryPool<A> {
    pub fn new(owner: &Rc<Device<A>>) -> Self {
        Self { owner: owner.clone(), bits: 0, period: 0.0, enabled: false,
            slots: Vec::new(), pending: VecDeque::new(), results: VecDeque::new() }
    }

    pub fn configure(&mut self, bits: u32) {
        self.bits = bits;
        self.period = unsafe { self.owner.open.queue.get_timestamp_period() } as f64;
    }

    pub fn enable(&mut self, enabled: bool) -> bool {
        let supported = self.supported();
        self.enabled = enabled && supported;
        supported
    }

    pub fn supported(&self) -> bool {
        (1..=64).contains(&self.bits) && self.period.is_finite() && self.period > 0.0
            && self.owner.features.contains(wgt::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    }

    fn layout() -> ReadbackLayout { ReadbackLayout { row_bytes: 16, pitch: 16, size: 16 } }

    pub fn begin(&mut self, queue: &SubmissionQueue<A>) -> Result<Option<Rc<Slot<A>>>> {
        if !self.enabled { return Ok(None); }
        let slot = if let Some(slot) = self.slots.iter().find(|slot| Rc::strong_count(slot) == 1) {
            slot.clone()
        } else if self.slots.len() < 8 {
            let query = unsafe { self.owner.open.device.create_query_set(&wgt::QuerySetDescriptor {
                label: Some("WR frame timestamps"), ty: wgt::QueryType::Timestamp, count: 2,
            }) }.map_err(|error| format!("Creating timestamp queries: {error:?}"))?;
            let query = Owned::new(&self.owner, query, A::Device::destroy_query_set);
            let buffer = Buffer::readback(&self.owner, &Self::layout())?;
            let slot = Rc::new(Slot { query, buffer });
            self.slots.push(slot.clone());
            slot
        } else {
            return Ok(None);
        };
        let mut commands = queue.recording()?;
        commands.keep(slot.clone());
        unsafe {
            commands.encoder().reset_queries(&slot.query, 0..2);
            commands.encoder().write_timestamp(&slot.query, 0);
        }
        Ok(Some(slot))
    }

    pub fn finish(&mut self, queue: &SubmissionQueue<A>, slot: Option<Rc<Slot<A>>>) -> Result<u64> {
        if let Some(slot) = &slot {
            let mut commands = queue.recording()?;
            slot.buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
            unsafe {
                commands.encoder().write_timestamp(&slot.query, 1);
                commands.encoder().copy_query_results(&slot.query, 0..2, &slot.buffer.raw, 0, std::num::NonZeroU64::new(8).unwrap());
            }
            slot.buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        }
        let serial = queue.submit_serial()?;
        if let Some(slot) = slot { self.pending.push_back((serial, slot)); }
        Ok(serial)
    }

    pub fn poll(&mut self, completed: u64) -> Result<()> {
        while self.pending.front().map_or(false, |entry| entry.0 <= completed) {
            let (serial, slot) = self.pending.front().unwrap();
            let bytes = self.owner.map_readback(&slot.buffer.raw, &Self::layout())?;
            let start = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
            let end = u64::from_ne_bytes(bytes[8..16].try_into().unwrap());
            let ticks = elapsed_ticks(start, end, self.bits);
            if self.results.len() == 64 { self.results.pop_front(); }
            self.results.push_back((*serial, ticks as f64 * self.period));
            self.pending.pop_front();
        }
        Ok(())
    }

    pub fn take(&mut self) -> Vec<(u64, f64)> { self.results.drain(..).collect() }
    pub fn trim(&mut self) { self.slots.retain(|slot| Rc::strong_count(slot) > 1); }
    pub fn counts(&self) -> (usize, usize) { (self.slots.len(), self.pending.len()) }
}

fn elapsed_ticks(start: u64, end: u64, bits: u32) -> u64 {
    end.wrapping_sub(start) & (u64::MAX >> (64 - bits))
}

#[cfg(all(test, wr_hal_vulkan))]
mod tests {
    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn bounded_queries_wait_for_completion() {
        use super::*;
        let device = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let bits = <hal::api::Vulkan as super::backend::BackendApi>::timestamp_valid_bits(&device);
        let owner = Rc::new(device);
        let queue = SubmissionQueue::new(&owner, 3, false);
        let mut queries = QueryPool::new(&owner);
        assert!(!queries.enable(true));
        assert!(queries.begin(&queue).unwrap().is_none());
        queries.configure(bits);
        assert!(queries.enable(true));
        let mut serials = Vec::new();
        for _ in 0..8 {
            let slot = queries.begin(&queue).unwrap();
            assert!(slot.is_some());
            serials.push(queries.finish(&queue, slot).unwrap());
        }
        assert!(queries.begin(&queue).unwrap().is_none());
        queries.poll(0).unwrap();
        assert!(queries.take().is_empty());
        assert_eq!(queries.counts(), (8, 8));
        queue.wait().unwrap();
        queries.poll(queue.poll().unwrap()).unwrap();
        let results = queries.take();
        assert_eq!(results.iter().map(|entry| entry.0).collect::<Vec<_>>(), serials);
        assert!(results.iter().all(|entry| entry.1.is_finite() && entry.1 >= 0.0));
        let slot = queries.begin(&queue).unwrap();
        assert!(slot.is_some());
        queries.finish(&queue, slot).unwrap();
        assert_eq!(queries.counts(), (8, 1));
        queries.enable(false);
        assert!(queries.begin(&queue).unwrap().is_none());
        queue.wait().unwrap();
        queries.poll(queue.poll().unwrap()).unwrap();
        let memory = owner.memory.get().buffer_bytes;
        queries.trim();
        assert_eq!(queries.counts(), (0, 0));
        assert!(owner.memory.get().buffer_bytes < memory);
    }

    #[test]
    fn timestamp_wrap() {
        assert_eq!(super::elapsed_ticks((1 << 36) - 3, 2, 36), 5);
        assert_eq!(super::elapsed_ticks(u64::MAX - 2, 2, 64), 5);
        assert_eq!(super::elapsed_ticks(7, 7, 64), 0);
    }
}
