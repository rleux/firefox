/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Unacquired,
    Acquiring,
    AcquireSubmitted,
    Acquired,
    Releasing,
    Released,
}

pub(crate) struct ForeignRgbLifetime {
    phase: Phase,
}

impl ForeignRgbLifetime {
    pub(crate) fn new() -> Self {
        Self {
            phase: Phase::Unacquired,
        }
    }
    fn advance(&mut self, expected: Phase, next: Phase) -> Result<(), &'static str> {
        if self.phase != expected {
            return Err("Invalid foreign RGB ownership sequence");
        }
        self.phase = next;
        Ok(())
    }
    pub(crate) fn begin_acquire(&mut self) -> Result<(), &'static str> {
        self.advance(Phase::Unacquired, Phase::Acquiring)
    }
    pub(crate) fn acquired(&mut self) -> Result<(), &'static str> {
        self.advance(Phase::Acquiring, Phase::Acquired)
    }
    pub(crate) fn acquire_submitted(&mut self) -> Result<(), &'static str> {
        self.advance(Phase::Acquiring, Phase::AcquireSubmitted)
    }
    pub(crate) fn needs_release(&self) -> bool {
        matches!(self.phase, Phase::Acquired | Phase::AcquireSubmitted)
    }
    pub(crate) fn begin_release(&mut self) -> Result<(), &'static str> {
        if self.phase == Phase::AcquireSubmitted {
            return self.advance(Phase::AcquireSubmitted, Phase::Releasing);
        }
        self.advance(Phase::Acquired, Phase::Releasing)
    }
    pub(crate) fn released(&mut self) -> Result<(), &'static str> {
        self.advance(Phase::Releasing, Phase::Released)
    }
    pub(crate) fn producer_reusable(&self) -> bool {
        matches!(self.phase, Phase::Unacquired | Phase::Released)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_acquire_requires_completed_return_before_reuse() {
        let mut state = ForeignRgbLifetime::new();
        state.begin_acquire().unwrap();
        state.acquire_submitted().unwrap();
        assert!(state.needs_release());
        assert!(!state.producer_reusable());
        assert!(state.acquired().is_err());
        state.begin_release().unwrap();
        assert!(!state.producer_reusable());
        state.released().unwrap();
        assert!(state.producer_reusable());
    }
    #[test]
    fn rejection_before_acquire_does_not_lock_producer() {
        let state = ForeignRgbLifetime::new();
        assert!(state.producer_reusable());
        assert!(!state.needs_release());
    }
    #[test]
    fn failed_acquire_cannot_be_reported_as_unused() {
        let mut state = ForeignRgbLifetime::new();
        state.begin_acquire().unwrap();
        assert!(!state.producer_reusable());
        assert!(!state.needs_release());
        assert!(state.begin_release().is_err());
    }
    #[test]
    fn acquired_but_unused_image_still_needs_ownership_return() {
        let mut state = ForeignRgbLifetime::new();
        state.begin_acquire().unwrap();
        state.acquired().unwrap();
        assert!(state.needs_release());
        assert!(!state.producer_reusable());
    }
    #[test]
    fn failed_release_prevents_producer_reuse() {
        let mut state = ForeignRgbLifetime::new();
        state.begin_acquire().unwrap();
        state.acquired().unwrap();
        state.begin_release().unwrap();
        assert!(!state.producer_reusable());
        assert!(!state.needs_release());
    }
    #[test]
    fn reuse_requires_completed_ownership_return() {
        let mut state = ForeignRgbLifetime::new();
        state.begin_acquire().unwrap();
        state.acquired().unwrap();
        state.begin_release().unwrap();
        state.released().unwrap();
        assert!(state.producer_reusable());
        assert!(!state.needs_release());
        assert!(state.begin_acquire().is_err());
        assert!(state.released().is_err());
    }
    #[test]
    fn premature_completion_cannot_unlock_producer() {
        let mut state = ForeignRgbLifetime::new();
        assert!(state.acquired().is_err());
        assert!(state.released().is_err());
        state.begin_acquire().unwrap();
        assert!(state.begin_acquire().is_err());
        assert!(state.released().is_err());
        assert!(!state.producer_reusable());
    }
}
