/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

pub(super) struct UsageState<T: Copy> {
    committed: Cell<T>,
    pending: Cell<T>,
    // Pending usage is visible only while its recording is alive.
    recording: RefCell<Weak<()>>,
}

impl<T: Copy> UsageState<T> {
    pub fn new(value: T) -> Self {
        Self {
            committed: Cell::new(value),
            pending: Cell::new(value),
            recording: RefCell::new(Weak::new()),
        }
    }

    pub fn current(&self) -> T {
        if self.recording.borrow().upgrade().is_some() {
            self.pending.get()
        } else {
            self.committed.get()
        }
    }

    pub fn check_recording(&self, recording: &Rc<()>) -> Result<(), String> {
        if self
            .recording
            .borrow()
            .upgrade()
            .map_or(false, |owner| !Rc::ptr_eq(&owner, recording))
        {
            return Err("Resource is already in another recording".into());
        }
        Ok(())
    }

    pub fn prepare(&self, recording: &Rc<()>, to: T) -> Result<(T, bool), String> {
        let active = self.recording.borrow().upgrade();
        let (from, first) = match active {
            Some(owner) => {
                if !Rc::ptr_eq(&owner, recording) {
                    return Err("Resource is already in another recording".into());
                }
                (self.pending.get(), false)
            }
            None => {
                *self.recording.borrow_mut() = Rc::downgrade(recording);
                (self.committed.get(), true)
            }
        };
        self.pending.set(to);
        Ok((from, first))
    }

    pub fn commit(&self) {
        self.committed.set(self.pending.get());
    }

    pub fn reset(&mut self, value: T) {
        self.committed.set(value);
        self.pending.set(value);
        *self.recording.get_mut() = Weak::new();
    }
}
