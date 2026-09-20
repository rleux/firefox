/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::units::{DeviceIntRect, DeviceIntSize};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Repair {
    Full,
    Partial(DeviceIntRect),
    Unchanged,
}

struct Change {
    serial: u64,
    size: [u32; 2],
    damage: DeviceIntRect,
}

#[derive(Default)]
pub(super) struct OutputHistory {
    changes: VecDeque<Change>,
}

impl OutputHistory {
    pub fn record(&mut self, serial: u64, size: [u32; 2], damage: DeviceIntRect) {
        debug_assert!(self.changes.back().map_or(true, |last| serial > last.serial));
        let full = DeviceIntRect::from_size(DeviceIntSize::new(size[0] as i32, size[1] as i32));
        let damage = if damage.is_empty() || !full.contains_box(&damage) { full } else { damage };
        if self.changes.len() == 32 { self.changes.pop_front(); }
        self.changes.push_back(Change { serial, size, damage });
    }

    pub fn repair(&self, serial: u64, size: [u32; 2], previous: Option<u64>) -> Repair {
        let Some(latest) = self.changes.back() else { return Repair::Full; };
        if latest.serial != serial || latest.size != size { return Repair::Full; }
        let Some(previous) = previous else { return Repair::Full; };
        let Some(index) = self.changes.iter().position(|change| change.serial == previous) else { return Repair::Full; };
        if self.changes[index].size != size { return Repair::Full; }
        if previous == serial { return Repair::Unchanged; }
        let mut damage = DeviceIntRect::zero();
        for change in self.changes.iter().skip(index + 1) {
            if change.size != size { return Repair::Full; }
            damage = if damage.is_empty() { change.damage } else { damage.union(&change.damage) };
        }
        let full = DeviceIntRect::from_size(DeviceIntSize::new(size[0] as i32, size[1] as i32));
        if damage.is_empty() || damage == full { Repair::Full } else { Repair::Partial(damage) }
    }
}

#[cfg(test)]
#[path = "present_history_tests.rs"]
mod tests;
