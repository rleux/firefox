/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{DmaBufImage, TextureState};
use crate::device::wgpu::hal::dma_buf::ExternalDomain;
use crate::device::wgpu::{wgt, Recording, SharedTimeline};
use std::rc::Rc;

impl DmaBufImage {
    /// # Safety
    /// The producer must release this initialized image in GENERAL layout to
    /// QUEUE_FAMILY_EXTERNAL before `ready` reaches `value`, and must not access
    /// it again until the consumer's release signal completes. Concurrent aliases
    /// must obey the same ownership protocol.
    pub unsafe fn acquire(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        ready: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.owner)?;
        self.states[0].check_recording(&recording)?;
        if self.states[0].current().initialized {
            return Err("DMA-BUF image is already acquired".into());
        }
        commands.wait_timeline(ready, value)?;
        self.record_access(commands, &recording, true, ExternalDomain::External)
    }

    /// Publish the release value to the producer only after successful submission.
    pub fn release(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        released: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.owner)?;
        self.states[0].check_recording(&recording)?;
        if !self.states[0].current().initialized {
            return Err("DMA-BUF image is not acquired".into());
        }
        commands.signal_timeline(released, value)?;
        self.record_access(commands, &recording, false, ExternalDomain::External)
    }

    pub(in crate::device::wgpu) fn record_access(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        recording: &Rc<()>,
        acquire: bool,
        external_family: ExternalDomain,
    ) -> Result<(), String> {
        self.inner
            .validate_access(commands.encoder(), external_family)?;
        let (_, first) = self.states[0].prepare(
            recording,
            TextureState {
                usage: if acquire {
                    wgt::TextureUses::RESOURCE
                } else {
                    wgt::TextureUses::UNINITIALIZED
                },
                initialized: acquire,
            },
        )?;
        if first {
            let image = self.clone();
            commands.commit(move || image.states[0].commit());
        }
        commands.keep(self);
        unsafe {
            self.inner
                .record_access(commands.encoder(), acquire, external_family)
        };
        Ok(())
    }
}
