/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::{hal, Device};
use std::any::Any;
use crate::internal_types::FastHashMap;
use std::rc::Rc;
use std::time::Duration;

pub struct Submission {
    data: SubmissionData,
    state: SubmissionState,
}

enum SubmissionState {
    Recording,
    EncodingFailed,
    Unconfirmed { buffer: Box<dyn hal::DynCommandBuffer> },
    Submitted { buffer: Box<dyn hal::DynCommandBuffer> },
    Retired,
}

struct SubmissionData {
    owner: Rc<Device>,
    encoder: Box<dyn hal::DynCommandEncoder>,
    fence: Owned<dyn hal::DynFence>,
    resources: FastHashMap<*const (), Rc<dyn Any>>,
}

/// Exclusive access to an open submission; releasing this borrow does not submit it.
pub struct Recording<'a> {
    submission: &'a mut SubmissionData,
}

impl Recording<'_> {
    pub fn encoder(&mut self) -> &mut dyn hal::DynCommandEncoder {
        self.submission.encoder.as_mut()
    }

    pub fn keep<T: 'static>(&mut self, value: &Rc<T>) {
        self.submission.resources.entry(Rc::as_ptr(value).cast())
            .or_insert_with(|| value.clone());
    }
}

impl Submission {
    pub fn new(owner: &Rc<Device>) -> Result<Self, String> {
        let fence = unsafe { owner.open.device.create_fence() }
            .map_err(|error| format!("Creating submission fence: {error:?}"))?;
        let fence = Owned::new(owner, fence, <dyn hal::DynDevice>::destroy_fence);
        let mut encoder = unsafe {
            owner.open.device.create_command_encoder(&hal::CommandEncoderDescriptor {
                label: Some("WR Vulkan submission"),
                queue: owner.open.queue.as_ref(),
            })
        }.map_err(|error| format!("Creating submission encoder: {error:?}"))?;
        unsafe {
            if let Err(error) = encoder.begin_encoding(Some("WR Vulkan submission")) {
                encoder.reset_all(Vec::new());
                return Err(format!("Beginning submission: {error:?}"));
            }
        }
        Ok(Self {
            data: SubmissionData {
                owner: owner.clone(),
                encoder,
                fence,
                resources: FastHashMap::default(),
            },
            state: SubmissionState::Recording,
        })
    }

    pub fn recording(&mut self) -> Result<Recording<'_>, String> {
        match &self.state {
            SubmissionState::Recording => Ok(Recording {
                submission: &mut self.data,
            }),
            _ => Err("Submission is no longer recording".into()),
        }
    }

    pub fn submit(&mut self) -> Result<(), String> {
        let data = &mut self.data;
        if !matches!(self.state, SubmissionState::Recording) {
            return Err("Submission has already been attempted".into());
        }
        let SubmissionState::Recording = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!()
        };
        self.state = SubmissionState::EncodingFailed;
        let buffer = unsafe { data.encoder.end_encoding() }.map_err(|error| {
            format!("Finishing submission: {error:?}")
        })?;
        let SubmissionState::EncodingFailed = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!()
        };
        self.state = SubmissionState::Unconfirmed { buffer };
        let SubmissionState::Unconfirmed { buffer, .. } = &self.state else { unreachable!() };
        let result = unsafe { data.owner.open.queue.submit(
            &[&**buffer], &[], (&*data.fence, 1),
        ) };
        result.map_err(|error| {
            format!("Submitting commands: {error:?}")
        })?;
        let SubmissionState::Unconfirmed { buffer } = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!()
        };
        self.state = SubmissionState::Submitted { buffer };

        Ok(())
    }

    fn retire(&mut self) {
        let SubmissionState::Submitted { buffer } = std::mem::replace(&mut self.state, SubmissionState::Retired) else {
            unreachable!("Only submitted work can retire")
        };
        let data = &mut self.data;
        unsafe { data.encoder.reset_all(vec![buffer]); }
        data.resources.clear();
    }

    pub fn poll(&mut self) -> Result<bool, String> {
        match self.state {
            SubmissionState::Retired => return Ok(true),
            SubmissionState::Submitted { .. } => (),
            _ => return Err("Cannot poll an unsubmitted recording".into()),
        }
        let data = &self.data;
        let completed = unsafe { data.owner.open.device.get_fence_value(&*data.fence) }
            .map_err(|error| format!("Polling submission: {error:?}"))?;
        if completed >= 1 {
            self.retire();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<bool, String> {
        match self.state {
            SubmissionState::Retired => return Ok(true),
            SubmissionState::Submitted { .. } => (),
            _ => return Err("Cannot wait for an unsubmitted recording".into()),
        }
        let data = &self.data;
        let completed = unsafe { data.owner.open.device.wait(&*data.fence, 1, timeout) }
            .map_err(|error| format!("Waiting for submission: {error:?}"))?;
        if completed {
            self.retire();
        }
        Ok(completed)
    }
}

impl Drop for Submission {
    fn drop(&mut self) {
        let data = &mut self.data;
        unsafe {
            if matches!(self.state, SubmissionState::EncodingFailed
                | SubmissionState::Unconfirmed { .. } | SubmissionState::Submitted { .. })
            {
                let _ = data.owner.open.queue.wait_for_idle();
            }
            if matches!(self.state, SubmissionState::Recording | SubmissionState::EncodingFailed) {
                data.encoder.discard_encoding();
            }
            let buffers = match std::mem::replace(&mut self.state, SubmissionState::Retired) {
                SubmissionState::Recording | SubmissionState::EncodingFailed => Vec::new(),
                SubmissionState::Unconfirmed { buffer } => vec![buffer],
                SubmissionState::Submitted { buffer } => vec![buffer],
                SubmissionState::Retired => Vec::new(),
            };
            data.encoder.reset_all(buffers);
        }
        data.resources.clear();
    }
}
