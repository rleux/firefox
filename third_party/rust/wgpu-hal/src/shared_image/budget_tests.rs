use super::*;

#[test]
fn unused_reservations_and_confirmed_cleanup_refund_capacity() {
    let budget = Arc::new(AllocationBudget::new(2));
    let unused = budget.reserve().unwrap();
    assert_eq!(budget.live_allocations(), 1);
    drop(unused);
    assert_eq!(budget.live_allocations(), 0);
    let mut physical = budget.reserve().unwrap();
    physical.allocated();
    assert_eq!(budget.live_allocations(), 1);
    physical.cleaned();
    assert_eq!(budget.live_allocations(), 1);
    drop(physical);
    assert_eq!(budget.live_allocations(), 0);
    assert!(!budget.is_poisoned());
}

#[test]
fn live_physical_credits_exhaust_budget_without_refunding_on_wrapper_loss() {
    let budget = Arc::new(AllocationBudget::new(2));
    let mut first = budget.reserve().unwrap();
    let mut second = budget.reserve().unwrap();
    first.allocated();
    second.allocated();
    assert!(matches!(
        budget.reserve(),
        Err(ImageLeaseError::Device(crate::DeviceError::OutOfMemory))
    ));
    assert_eq!(budget.live_allocations(), 2);
    first.cleaned();
    drop(first);
    assert_eq!(budget.live_allocations(), 1);
    let unused = budget.reserve().unwrap();
    assert_eq!(budget.live_allocations(), 2);
    drop(unused);
    second.cleaned();
    drop(second);
    assert_eq!(budget.live_allocations(), 0);
}

#[test]
fn cleanup_uncertainty_retains_credit_and_poison_is_not_ordinary_oom() {
    let budget = Arc::new(AllocationBudget::new(2));
    let mut uncertain = budget.reserve().unwrap();
    uncertain.allocated();
    drop(uncertain);
    assert!(budget.is_poisoned());
    assert_eq!(budget.live_allocations(), 1);
    assert!(matches!(
        budget.reserve(),
        Err(ImageLeaseError::Device(crate::DeviceError::OutOfMemory))
    ));
    let healthy = Arc::new(AllocationBudget::new(1));
    let unused = healthy.reserve().unwrap();
    assert!(healthy.reserve().is_err());
    drop(unused);
    assert!(!healthy.is_poisoned());
    assert_eq!(healthy.live_allocations(), 0);
    assert!(healthy.reserve().is_ok());
}

#[test]
fn concurrent_reservations_never_exceed_the_actor_quota() {
    let budget = Arc::new(AllocationBudget::new(8));
    let entered = Arc::new(std::sync::Barrier::new(33));
    let release = Arc::new(std::sync::Barrier::new(33));
    let mut workers = alloc::vec::Vec::new();
    for _ in 0..32 {
        let budget = budget.clone();
        let entered = entered.clone();
        let release = release.clone();
        workers.push(std::thread::spawn(move || {
            let credit = budget.reserve().ok();
            entered.wait();
            release.wait();
            drop(credit);
        }));
    }
    entered.wait();
    let live = budget.live_allocations();
    release.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(live, 8);
    assert_eq!(budget.live_allocations(), 0);
    assert!(!budget.is_poisoned());
}
