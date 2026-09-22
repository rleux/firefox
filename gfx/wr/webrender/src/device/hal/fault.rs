/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailurePoint { Acquire, Import, VideoPlaneView, Record, Submit, Map, Configure }

impl<A: hal::Api> Device<A> {
    pub(super) fn check_fault(&self, point: FailurePoint) -> Result<()> {
        if self.fault.get() == Some(point) {
            self.fault.set(None);
            if !matches!(point, FailurePoint::Import | FailurePoint::VideoPlaneView) { self.lost.set(true); }
            return Err(format!("Injected HAL {point:?} failure"));
        }
        Ok(())
    }
}
