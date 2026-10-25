/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Device};
use std::{cell::Cell, rc::Rc, sync::Arc};

pub use hal::external_sync::TimelineHandle;

/// One producer signals increasing values; imported timelines can only be waited on.
pub struct SharedTimeline {
    owner: Rc<Device>,
    semaphore: Arc<dyn hal::external_sync::Semaphore>,
    last_signal: Option<Cell<u64>>,
    max_difference: u64,
}

pub(super) fn properties(owner: &Device) -> ([u8; 16], [u8; 16], u64) {
    *owner.native_cache.properties.get_or_init(|| {
        let properties = hal::external_sync::properties(&*owner.open.device)
            .expect("Renderer native semaphore properties");
        (
            properties.device_uuid,
            properties.driver_uuid,
            properties.max_timeline_value_difference,
        )
    })
}

impl SharedTimeline {
    fn available(owner: &Device) -> bool {
        !owner.is_lost()
            && (owner.shared.is_some()
                || owner.supports_dma_buf())
    }

    pub fn new(owner: &Rc<Device>) -> Result<Rc<Self>, String> {
        if !Self::available(owner) {
            return Err("External Vulkan timelines are unavailable".into());
        }
        let semaphore = hal::external_sync::create_timeline(&*owner.open.device, true)
            .map_err(|error| error.to_string())?;
        Ok(Rc::new(Self {
            owner: owner.clone(),
            semaphore,
            last_signal: Some(Cell::new(0)),
            max_difference: properties(owner).2,
        }))
    }

    pub fn export(&self) -> Result<TimelineHandle, String> {
        if self.last_signal.is_none() || self.owner.is_lost() {
            return Err("Only a live timeline producer can export its handle".into());
        }
        hal::external_sync::export_timeline(&*self.semaphore).map_err(|error| error.to_string())
    }

    pub fn import(owner: &Rc<Device>, handle: &TimelineHandle) -> Result<Rc<Self>, String> {
        if !Self::available(owner) {
            return Err("External Vulkan timelines are unavailable".into());
        }
        let semaphore = hal::external_sync::import_timeline(&*owner.open.device, handle)
            .map_err(|error| error.to_string())?;
        Ok(Rc::new(Self {
            owner: owner.clone(),
            semaphore,
            last_signal: None,
            max_difference: properties(owner).2,
        }))
    }

    pub(super) fn current_value(&self) -> Result<u64, String> {
        if self.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        hal::external_sync::current_value(&*self.semaphore).map_err(|error| {
            if self.owner.shared.is_some() { self.owner.mark_lost(); }
            error.to_string()
        })
    }

    fn validate_value(&self, value: u64) -> Result<(), String> {
        if self.max_difference == u64::MAX {
            return Ok(());
        }
        if value.saturating_sub(self.current_value()?) > self.max_difference {
            return Err("Timeline value exceeds the device's outstanding range".into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct SubmissionSync {
    sync_files: Vec<super::SyncFileWait>,
    waits: Vec<(Rc<SharedTimeline>, u64)>,
    signals: Vec<(Rc<SharedTimeline>, u64)>,
}

fn add(entries: &mut Vec<(Rc<SharedTimeline>, u64)>, timeline: &Rc<SharedTimeline>, value: u64) {
    if let Some((_, previous)) = entries
        .iter_mut()
        .find(|(entry, _)| Rc::ptr_eq(entry, timeline))
    {
        *previous = (*previous).max(value);
    } else {
        entries.push((timeline.clone(), value));
    }
}

impl SubmissionSync {
    pub fn wait_sync_file(
        &mut self,
        owner: &Rc<Device>,
        wait: super::SyncFileWait,
    ) -> Result<(), String> {
        if !Rc::ptr_eq(owner, &wait.owner) {
            return Err("Sync-file wait belongs to another Vulkan device".into());
        }
        self.sync_files.push(wait);
        Ok(())
    }

    pub fn wait(
        &mut self,
        owner: &Rc<Device>,
        timeline: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        if !Rc::ptr_eq(owner, &timeline.owner) {
            return Err("Timeline must be imported into the recording's device".into());
        }
        if value != 0 {
            add(&mut self.waits, timeline, value);
        }
        Ok(())
    }

    pub fn signal(
        &mut self,
        owner: &Rc<Device>,
        timeline: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let last = timeline
            .last_signal
            .as_ref()
            .ok_or("An imported timeline cannot signal")?;
        if !Rc::ptr_eq(owner, &timeline.owner) || value <= last.get() {
            return Err("Timeline signal requires its producer and an increasing value".into());
        }
        add(&mut self.signals, timeline, value);
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        for (timeline, value) in &self.waits {
            if timeline
                .last_signal
                .as_ref()
                .is_some_and(|last| *value > last.get())
            {
                return Err("Cannot wait for a future signal on the same Vulkan queue".into());
            }
            timeline.validate_value(*value)?;
        }
        for (timeline, value) in &self.signals {
            if *value <= timeline.last_signal.as_ref().unwrap().get() {
                return Err("Timeline signals were submitted out of order".into());
            }
            timeline.validate_value(*value)?;
        }
        Ok(())
    }

    pub fn dependencies(&self) -> hal::external_sync::SubmissionSync {
        let mut waits = self
            .sync_files
            .iter()
            .map(|wait| hal::external_sync::SemaphoreValue {
                semaphore: wait.semaphore.clone(),
                value: None,
            })
            .collect::<Vec<_>>();
        waits.extend(self.waits.iter().map(|(timeline, value)| {
            hal::external_sync::SemaphoreValue {
                semaphore: timeline.semaphore.clone(),
                value: Some(*value),
            }
        }));
        let signals = self
            .signals
            .iter()
            .map(|(timeline, value)| hal::external_sync::SemaphoreValue {
                semaphore: timeline.semaphore.clone(),
                value: Some(*value),
            })
            .collect();
        hal::external_sync::SubmissionSync { waits, signals }
    }

    pub fn submitted(&self) {
        for (timeline, value) in &self.signals {
            timeline.last_signal.as_ref().unwrap().set(*value);
        }
    }

    pub fn clear(&mut self) {
        self.sync_files.clear();
        self.waits.clear();
        self.signals.clear();
    }
}

#[cfg(test)]
#[path = "timeline_tests.rs"]
mod tests;
