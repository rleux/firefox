/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn publication() -> NativeAccess {
    unsafe { NativeAccess::initialized_publication() }
}
fn wrapped() -> NativeAccess {
    let mut access = publication();
    assert_eq!(access.prepare(1), Ok(Prepare::RecordFirstAcquire));
    access.submitted(1, 4).unwrap();
    access.observe_initial_completion(4);
    access.wrapped().unwrap();
    access
}

#[test]
fn first_acquire_needs_successful_submission_and_completion_before_wrap() {
    let mut access = publication();
    assert_eq!(access.prepare(1), Ok(Prepare::RecordFirstAcquire));
    assert_eq!(access.prepare(1), Ok(Prepare::Pending));
    assert!(access.wrapped().is_err());
    assert!(!access.may_sample(1));
    access.submitted(1, 9).unwrap();
    assert_eq!(access.prepare(2), Ok(Prepare::Pending));
    access.observe_initial_completion(8);
    assert!(access.wrapped().is_err());
    access.observe_initial_completion(9);
    assert_eq!(access.prepare(2), Ok(Prepare::WrapInitialized));
    access.wrapped().unwrap();
    assert_eq!(access.prepare(2), Ok(Prepare::Ready));
    assert!(access.may_sample(2));
}

#[test]
fn cancelled_acquire_retries_without_claiming_native_ownership() {
    let mut access = publication();
    access.prepare(1).unwrap();
    access.cancel_recording(1).unwrap();
    assert_eq!(access.prepare(2), Ok(Prepare::RecordFirstAcquire));
}

#[test]
fn cancelled_release_after_submitted_acquire_stays_renderer_owned() {
    let mut access = wrapped();
    assert_eq!(access.prepare(2), Ok(Prepare::Ready));
    access.release(2).unwrap();
    assert!(!access.may_sample(2));
    access.cancel_recording(2).unwrap();
    assert_eq!(access.prepare(3), Ok(Prepare::Ready));
    access.release(3).unwrap();
    access.submitted(3, 5).unwrap();
    assert_eq!(access.prepare(4), Ok(Prepare::RecordAcquire));
    assert!(access.may_sample(4));
}

#[test]
fn repeated_wrapped_acquisition_does_not_reinitialize_core_texture() {
    let mut access = wrapped();
    for batch in 2..6 {
        access.prepare(batch).unwrap();
        access.release(batch).unwrap();
        access.submitted(batch, batch + 4).unwrap();
        assert_eq!(access.prepare(batch + 1), Ok(Prepare::RecordAcquire));
        assert!(access.wrapped().is_err());
    }
}

#[test]
fn aliases_share_one_recording_and_support_reacquisition_in_queue_order() {
    let mut access = wrapped();
    access.prepare(2).unwrap();
    assert!(access.prepare(3).is_err());
    assert!(!access.may_sample(3));
    access.release(2).unwrap();
    assert_eq!(access.prepare(2), Ok(Prepare::RecordAcquire));
    assert_eq!(access.prepare(2), Ok(Prepare::Ready));
    access.release(2).unwrap();
    access.submitted(2, 5).unwrap();
}

#[test]
fn first_acquire_cleanup_does_not_wrap_an_externally_returned_image() {
    let mut access = publication();
    access.prepare(1).unwrap();
    access.submitted(1, 4).unwrap();
    access.release(2).unwrap();
    access.submitted(2, 5).unwrap();
    access.observe_initial_completion(5);
    assert!(access.wrapped().is_err());
    assert_eq!(access.prepare(3), Ok(Prepare::RecordFirstAcquire));
}

#[test]
fn invalid_release_does_not_claim_the_recording() {
    let mut access = publication();
    assert!(access.release(1).is_err());
    assert_eq!(access.prepare(2), Ok(Prepare::RecordFirstAcquire));
}

#[test]
fn failed_initial_acquire_cannot_wrap_after_a_work_done_callback() {
    let mut access = publication();
    access.prepare(1).unwrap();
    access.submitted(1, 4).unwrap();
    access.abandon();
    access.observe_initial_completion(u64::MAX);
    assert!(access.wrapped().is_err());
    assert!(access.prepare(2).is_err());
}

#[test]
fn recording_return_never_uses_a_preexisting_timeline_value() {
    let mut receipt = NativeReturn::recording(ReturnKind::Foreign, 9).unwrap();
    assert_eq!(receipt.status(true, Ok(999)), ReturnStatus::Pending);
    receipt.abandon();
    assert_eq!(receipt.status(true, Ok(999)), ReturnStatus::Abandoned);
}

#[test]
fn renderer_drop_does_not_abandon_a_successfully_submitted_return() {
    let mut receipt = NativeReturn::recording(ReturnKind::Foreign, 9).unwrap();
    receipt.submitted().unwrap();
    assert_eq!(receipt.status(true, Ok(8)), ReturnStatus::Pending);
    // The owned retirement poller still supplies native observations after Renderer drop.
    assert_eq!(receipt.status(true, Ok(9)), ReturnStatus::Complete);
}

#[test]
fn submitted_timeline_point_allows_a_vulkan_wait_but_not_gl_reuse() {
    let mut vulkan = NativeReturn::recording(ReturnKind::Vulkan, 9).unwrap();
    let mut foreign = NativeReturn::recording(ReturnKind::Foreign, 9).unwrap();
    vulkan.submitted().unwrap();
    foreign.submitted().unwrap();
    assert_eq!(vulkan.status(true, Ok(8)), ReturnStatus::Submitted(9));
    assert_eq!(foreign.status(true, Ok(8)), ReturnStatus::Pending);
    assert_eq!(vulkan.status(false, Ok(9)), ReturnStatus::Abandoned);
    assert_eq!(foreign.status(false, Ok(9)), ReturnStatus::Abandoned);
}

#[test]
fn uncertain_native_completion_abandons_instead_of_releasing() {
    let mut receipt = NativeReturn::recording(ReturnKind::Foreign, 9).unwrap();
    receipt.submitted().unwrap();
    assert_eq!(receipt.status(true, Err(())), ReturnStatus::Abandoned);
    assert_eq!(receipt.status(true, Ok(9)), ReturnStatus::Abandoned);
}
