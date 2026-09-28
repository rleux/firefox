/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{wgt, Buffer};
use super::super::Options;
use super::super::tests::{Command, ERRORS, map_upload, record_upload, validation_logging};
use std::cell::Cell;
use std::sync::atomic::Ordering;

struct RetainedUntilIdle(Rc<Device>);

impl Drop for RetainedUntilIdle {
    fn drop(&mut self) {
        assert!(self.0.trace.borrow().iter().any(|event| matches!(event, Command::SubmissionIdleWait)));
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn failed_encoding_and_uncertain_submit_retain_resources_until_idle() {
    validation_logging();
    for failure in [FailurePoint::EndEncoding, FailurePoint::AfterSubmit] {
        let owner = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
        let bytes = [37; 64];
        let source = Buffer::new(&owner, &bytes, wgt::BufferUses::COPY_SRC).unwrap();
        let weak_source = Rc::downgrade(&source);
        let (mut submission, target) = record_upload(&owner, &source);
        let committed = Rc::new(Cell::new(false));
        let marker = Rc::new(RetainedUntilIdle(owner.clone()));
        let weak_marker = Rc::downgrade(&marker);
        {
            let mut commands = submission.recording().unwrap();
            commands.keep(&marker);
            let committed = committed.clone();
            commands.commit(move || committed.set(true));
        }
        drop(source);
        drop(marker);
        owner.trace.borrow_mut().clear();
        submission.failure = Some(failure);
        assert!(submission.submit().is_err());
        assert!(weak_source.upgrade().is_some());
        assert!(weak_marker.upgrade().is_some());
        assert!(!committed.get());
        assert!(submission.recording().is_err());
        assert!(submission.submit().is_err());
        assert!(submission.poll().is_err());
        assert!(submission.wait(None).is_err());
        drop(submission);
        assert!(weak_source.upgrade().is_none());
        assert!(weak_marker.upgrade().is_none());
        assert!(!committed.get());
        if failure == FailurePoint::AfterSubmit {
            assert_eq!(map_upload(&owner, &**target, 64), bytes);
        }
        drop(target);
        assert_eq!(Rc::strong_count(&owner), 1);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

