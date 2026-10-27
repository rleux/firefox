/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::{cell::Cell, sync::Arc};

struct ReturnMarker {
    epoch: Arc<super::super::ImageEpoch>,
    owner: Rc<Device>,
    accepted: Cell<bool>,
}

impl ReturnMarker {
    fn accepted(&self) {
        self.accepted.set(true);
        unsafe { self.epoch.marker_accepted() };
    }
}

impl Drop for ReturnMarker {
    fn drop(&mut self) {
        if self.accepted.get() && !self.owner.is_lost() {
            unsafe { self.epoch.marker_retired() };
        } else {
            self.epoch.abandon();
        }
    }
}

impl RenderDevice {
    /// Some is the first accepted marker index; None queries an already closing epoch.
    /// Neither value establishes reusable ownership; consume the canonical return token.
    pub fn close_shared_image(
        &mut self,
        epoch: &Arc<super::super::ImageEpoch>,
    ) -> Result<Option<u64>, String> {
        let owner = self.submissions.owner().clone();
        epoch
            .validate_owner(owner.open.device.as_ref(), owner.open.queue.as_ref())
            .map_err(|error| format!("Validating shared canvas close owner: {error}"))?;
        if epoch.is_closed() {
            return Ok(None);
        }
        self.submissions.ensure_available()?;
        self.flush_pass()?;
        if !epoch
            .begin_close(owner.open.device.as_ref(), owner.open.queue.as_ref())
            .map_err(|error| format!("Closing shared canvas epoch: {error}"))?
        {
            return Ok(None);
        }
        let result = (|| {
            let marker = Rc::new(ReturnMarker {
                epoch: epoch.clone(),
                owner,
                accepted: Cell::new(false),
            });
            {
                let mut commands = self.submissions.recording()?;
                commands.keep(&marker);
                let accepted = marker.clone();
                commands.commit(move || accepted.accepted());
            }
            let serial = self.submissions.submit()?;
            Ok(Some(serial))
        })();
        if result.is_err() {
            epoch.abandon();
        }
        result
    }
}
