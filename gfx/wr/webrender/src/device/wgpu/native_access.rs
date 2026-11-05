/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Prepare {
    RecordFirstAcquire,
    Pending,
    WrapInitialized,
    RecordAcquire,
    Ready,
}

#[derive(Clone, Copy)]
struct Recording {
    batch: u64,
    acquired: bool,
    first_acquire: bool,
}

pub(super) struct NativeAccess {
    // This is submitted queue order, not proof that GPU ownership has completed.
    acquired: bool,
    wrapped: bool,
    first_acquire: Option<u64>,
    recording: Option<Recording>,
    abandoned: bool,
}

impl NativeAccess {
    /// The producer must fully initialize the admitted allocation and hold all aliases
    /// immutable until the return signal completes; ready alone is not initialization.
    pub(super) unsafe fn initialized_publication() -> Self {
        Self {
            acquired: false,
            wrapped: false,
            first_acquire: None,
            recording: None,
            abandoned: false,
        }
    }

    fn check(&self, batch: u64) -> Result<(), &'static str> {
        if self.abandoned {
            return Err("Native image access is abandoned");
        }
        if batch == 0 {
            return Err("Invalid native recording identifier");
        }
        if self
            .recording
            .is_some_and(|recording| recording.batch != batch)
        {
            return Err("Native image belongs to another recording");
        }
        Ok(())
    }

    fn recording(&mut self, batch: u64) -> &mut Recording {
        self.recording.get_or_insert(Recording {
            batch,
            acquired: self.acquired,
            first_acquire: false,
        })
    }

    pub(super) fn prepare(&mut self, batch: u64) -> Result<Prepare, &'static str> {
        self.check(batch)?;
        if !self.wrapped {
            if self.first_acquire.is_some()
                || self
                    .recording
                    .is_some_and(|recording| recording.first_acquire)
            {
                return Ok(Prepare::Pending);
            }
            if self.acquired {
                if self.recording.is_some() {
                    return Ok(Prepare::Pending);
                }
                return Ok(Prepare::WrapInitialized);
            }
            let recording = self.recording(batch);
            recording.acquired = true;
            recording.first_acquire = true;
            return Ok(Prepare::RecordFirstAcquire);
        }
        let recording = self.recording(batch);
        if recording.acquired {
            Ok(Prepare::Ready)
        } else {
            recording.acquired = true;
            Ok(Prepare::RecordAcquire)
        }
    }

    /// Call only after a successful device poll supplies the completed submission.
    pub(super) fn observe_initial_completion(&mut self, completed: u64) {
        if self.first_acquire.is_some_and(|index| index <= completed) {
            self.first_acquire = None;
        }
    }

    /// Call after create_texture_from_hal(RESOURCE, cleared=true) succeeds while the
    /// completed raw acquire still owns the image in SHADER_READ_ONLY_OPTIMAL.
    pub(super) fn wrapped(&mut self) -> Result<(), &'static str> {
        if self.abandoned
            || !self.acquired
            || self.wrapped
            || self.first_acquire.is_some()
            || self.recording.is_some()
        {
            return Err("Native image is not ready for an initialized core wrap");
        }
        self.wrapped = true;
        Ok(())
    }

    pub(super) fn has_access(&self) -> bool {
        !self.abandoned
            && self
                .recording
                .map_or(self.acquired, |recording| recording.acquired)
    }

    pub(super) fn may_sample(&self, batch: u64) -> bool {
        !self.abandoned
            && self.wrapped
            && self.recording.map_or(self.acquired, |recording| {
                recording.batch == batch && recording.acquired
            })
    }

    pub(super) fn release(&mut self, batch: u64) -> Result<(), &'static str> {
        self.check(batch)?;
        let acquired = self
            .recording
            .map_or(self.acquired, |recording| recording.acquired);
        if !acquired {
            return Err("Native image is not acquired");
        }
        let recording = self.recording(batch);
        recording.acquired = false;
        Ok(())
    }

    /// The caller must have checked actual submit success, including device errors.
    pub(super) fn submitted(&mut self, batch: u64, index: u64) -> Result<(), &'static str> {
        self.check(batch)?;
        if index == 0 {
            return Err("Invalid native submission identifier");
        }
        let recording = self
            .recording
            .take()
            .ok_or("No native recording to submit")?;
        self.acquired = recording.acquired;
        if recording.first_acquire && recording.acquired {
            self.first_acquire = Some(index);
        } else if !recording.acquired {
            self.first_acquire = None;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn cancel_recording(&mut self, batch: u64) -> Result<(), &'static str> {
        self.check(batch)?;
        self.recording = None;
        Ok(())
    }

    pub(super) fn abandon(&mut self) {
        self.abandoned = true;
        self.recording = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReturnKind {
    Vulkan,
    Foreign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReturnStatus {
    Pending,
    Submitted(u64),
    Complete,
    Abandoned,
}

#[derive(Clone, Copy)]
enum ReturnStage {
    Recording,
    Submitted,
    Complete,
    Abandoned,
}

pub(super) struct NativeReturn {
    kind: ReturnKind,
    value: u64,
    stage: ReturnStage,
}

impl NativeReturn {
    pub(super) fn recording(kind: ReturnKind, value: u64) -> Result<Self, &'static str> {
        if value == 0 {
            return Err("Invalid native return timeline point");
        }
        Ok(Self {
            kind,
            value,
            stage: ReturnStage::Recording,
        })
    }

    pub(super) fn submitted(&mut self) -> Result<(), &'static str> {
        if !matches!(self.stage, ReturnStage::Recording) {
            return Err("Native return is not recording");
        }
        self.stage = ReturnStage::Submitted;
        Ok(())
    }

    pub(super) fn cancel_recording(&mut self) {
        if matches!(self.stage, ReturnStage::Recording) {
            self.stage = ReturnStage::Abandoned;
        }
    }

    pub(super) fn abandon(&mut self) {
        if !matches!(self.stage, ReturnStage::Complete) {
            self.stage = ReturnStage::Abandoned;
        }
    }

    /// The owner checks device failure first; only the consumer may signal this timeline.
    /// Work-done callbacks and renderer/frame counters must not supply this value.
    pub(super) fn status(
        &mut self,
        healthy: bool,
        completed_signal: Result<u64, ()>,
    ) -> ReturnStatus {
        if matches!(self.stage, ReturnStage::Complete) {
            return ReturnStatus::Complete;
        }
        if !healthy || completed_signal.is_err() {
            self.abandon();
        }
        match self.stage {
            ReturnStage::Recording => ReturnStatus::Pending,
            ReturnStage::Submitted if self.kind == ReturnKind::Vulkan => {
                ReturnStatus::Submitted(self.value)
            }
            ReturnStage::Submitted => {
                if completed_signal.unwrap() >= self.value {
                    self.stage = ReturnStage::Complete;
                    ReturnStatus::Complete
                } else {
                    ReturnStatus::Pending
                }
            }
            ReturnStage::Complete => ReturnStatus::Complete,
            ReturnStage::Abandoned => ReturnStatus::Abandoned,
        }
    }
}

#[cfg(test)]
#[path = "native_access_tests.rs"]
mod tests;
