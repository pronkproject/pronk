use std::num::NonZeroUsize;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Barrier, Weak,
};

use drm_display_executor::scheduler::source_use::{ClosedUse, GateError, SourceUse};

fn gate<F>(capacity: usize) -> SourceUse<F> {
    SourceUse::new(NonZeroUsize::new(capacity).unwrap()).unwrap()
}

#[test]
fn closed_admission_waits_for_permits_not_native_completion() {
    let owner = gate(2);
    assert!(matches!(owner.finish(), Err(GateError::AdmissionOpen)));
    let first = owner.begin().unwrap();
    let second = owner.begin().unwrap();
    owner.close();
    assert!(matches!(owner.begin(), Err(GateError::AdmissionClosed)));
    assert!(matches!(
        owner.finish(),
        Err(GateError::UnresolvedSubmissions)
    ));
    first.cancel_unsubmitted();
    let signaled = Arc::new(AtomicBool::new(false));
    second.submitted(Arc::clone(&signaled));
    let ClosedUse::Released(records) = owner.finish().unwrap() else {
        panic!("normal use failed")
    };
    assert_eq!(records.len(), 1);
    assert!(!records[0].load(Ordering::Relaxed));
    assert!(Arc::ptr_eq(&records[0], &signaled));
    assert!(matches!(owner.finish(), Err(GateError::AlreadyFinished)));
}

#[test]
fn record_budget_is_per_use_and_cancellation_returns_unused_capacity() {
    let first = gate(1);
    let second = gate(1);
    let permit = first.begin().unwrap();
    assert!(matches!(first.begin(), Err(GateError::Capacity)));
    second.begin().unwrap().submitted(22);
    permit.cancel_unsubmitted();
    first.begin().unwrap().submitted(11);
    assert!(matches!(first.begin(), Err(GateError::Capacity)));
    first.close();
    second.close();
    let ClosedUse::Released(a) = first.finish().unwrap() else {
        panic!("first failed")
    };
    let ClosedUse::Released(b) = second.finish().unwrap() else {
        panic!("second failed")
    };
    assert_eq!(a, [11]);
    assert_eq!(b, [22]);
}

#[test]
fn abandonment_is_terminal_even_when_other_permits_report_records() {
    let owner = gate(3);
    let abandoned = owner.begin().unwrap();
    let accepted = owner.begin().unwrap();
    let canceled = owner.begin().unwrap();
    drop(abandoned);
    assert!(matches!(owner.begin(), Err(GateError::AdmissionClosed)));
    assert!(matches!(
        owner.finish(),
        Err(GateError::UnresolvedSubmissions)
    ));
    accepted.submitted(17);
    canceled.cancel_unsubmitted();
    owner.close();
    let ClosedUse::Failed(records) = owner.finish().unwrap() else {
        panic!("abandonment became normal release")
    };
    assert_eq!(records, [17]);
    assert!(matches!(owner.finish(), Err(GateError::AlreadyFinished)));
}

#[test]
fn close_and_admission_have_one_ordered_decision() {
    for _ in 0..64 {
        let owner = gate::<u32>(1);
        let start = Barrier::new(2);
        std::thread::scope(|scope| {
            let submit = scope.spawn(|| {
                start.wait();
                match owner.begin() {
                    Ok(permit) => {
                        permit.submitted(1);
                        true
                    }
                    Err(GateError::AdmissionClosed) => false,
                    Err(error) => panic!("unexpected admission result: {error}"),
                }
            });
            start.wait();
            owner.close();
            let accepted = submit.join().unwrap();
            let ClosedUse::Released(records) = owner.finish().unwrap() else {
                panic!("race failed")
            };
            assert_eq!(records.len(), usize::from(accepted));
            assert!(matches!(owner.begin(), Err(GateError::AdmissionClosed)));
        });
    }
}

struct Record {
    owner: Weak<SourceUse<Record>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Record {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.close();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn record_destruction_runs_outside_the_accounting_lock() {
    let owner = Arc::new(gate(1));
    let drops = Arc::new(AtomicUsize::new(0));
    owner.begin().unwrap().submitted(Record {
        owner: Arc::downgrade(&owner),
        drops: Arc::clone(&drops),
    });
    owner.close();
    drop(owner.finish().unwrap());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn outstanding_permit_retains_accounting_after_controller_drop() {
    let owner = Arc::new(gate(1));
    let drops = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&owner);
    let permit = owner.begin().unwrap();
    drop(owner);
    permit.submitted(Record {
        owner: weak,
        drops: Arc::clone(&drops),
    });
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_finish_takes_the_complete_record_set_once() {
    for _ in 0..64 {
        let owner = gate(2);
        owner.begin().unwrap().submitted(11);
        owner.begin().unwrap().submitted(22);
        owner.close();
        let start = Barrier::new(3);
        std::thread::scope(|scope| {
            let finish = || {
                start.wait();
                owner.finish()
            };
            let first = scope.spawn(finish);
            let second = scope.spawn(finish);
            start.wait();
            let results = [first.join().unwrap(), second.join().unwrap()];
            let mut winners = 0;
            let mut consumed = 0;
            for result in results {
                match result {
                    Ok(ClosedUse::Released(records)) => {
                        assert_eq!(records, [11, 22]);
                        winners += 1;
                    }
                    Err(GateError::AlreadyFinished) => consumed += 1,
                    _ => panic!("unexpected concurrent finish result"),
                }
            }
            assert_eq!((winners, consumed), (1, 1));
        });
    }
}

#[test]
fn last_resolution_and_finish_preserve_terminal_failure() {
    for abandon in [false, true] {
        for _ in 0..64 {
            let owner = gate(2);
            let last = owner.begin().unwrap();
            if abandon {
                drop(owner.begin().unwrap());
            }
            owner.close();
            let start = Barrier::new(2);
            std::thread::scope(|scope| {
                let resolve = scope.spawn(|| {
                    start.wait();
                    last.submitted(17);
                });
                start.wait();
                let result = owner.finish();
                resolve.join().unwrap();
                let result = match result {
                    Err(GateError::UnresolvedSubmissions) => owner.finish(),
                    result => result,
                };
                match result.unwrap() {
                    ClosedUse::Released(records) => {
                        assert!(!abandon);
                        assert_eq!(records, [17]);
                    }
                    ClosedUse::Failed(records) => {
                        assert!(abandon);
                        assert_eq!(records, [17]);
                    }
                }
                assert!(matches!(owner.finish(), Err(GateError::AlreadyFinished)));
                assert!(matches!(owner.begin(), Err(GateError::AdmissionClosed)));
            });
        }
    }
}
