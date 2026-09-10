//! Bounded submission accounting for one authorized source use.
//!
//! This gate coordinates a trusted worker's submission threads. It does not
//! revoke imports or intercept native driver calls. A permit must surround every
//! native acceptance path; only already-materialized completion records belong
//! in `submitted`. Neither a permit nor a ready gate is a GPU-completion fence.

use std::collections::TryReserveError;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateError {
    AdmissionClosed,
    Capacity,
    AdmissionOpen,
    UnresolvedSubmissions,
    AlreadyFinished,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AdmissionClosed => "source-use submission admission is closed",
            Self::Capacity => "source-use completion record budget is exhausted",
            Self::AdmissionOpen => "source-use submission admission is still open",
            Self::UnresolvedSubmissions => "source use has unresolved submission permits",
            Self::AlreadyFinished => "source-use result was already taken",
        })
    }
}

impl std::error::Error for GateError {}

/// A terminal accounting result, not a statement that GPU work has finished.
#[must_use = "report normal release or terminal failure to the source-use owner"]
pub enum ClosedUse<F> {
    /// Every issued permit was canceled before submission or supplied a record.
    Released(Vec<F>),
    /// At least one permit was abandoned. Records cover only reported work;
    /// they must not be presented as a complete normal source release.
    Failed(Vec<F>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    Closing,
    Failed,
    Finished,
}

struct State<F> {
    phase: Phase,
    unresolved: usize,
    records: Vec<F>,
    capacity: usize,
}

struct Shared<F>(Mutex<State<F>>);

impl<F> Shared<F> {
    fn lock(&self) -> MutexGuard<'_, State<F>> {
        match self.0.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                // A poisoned accounting decision cannot authorize normal release.
                if state.phase != Phase::Finished {
                    state.phase = Phase::Failed;
                }
                state
            }
        }
    }
}

/// One non-reusable source-use controller with a bounded native-record budget.
///
/// Capacity includes unresolved permits and retained completion records. It is
/// per use, not a global frame queue limit. Close each use after its submissions;
/// unchanged-scene output reuse belongs to independent private-image storage.
pub struct SourceUse<F> {
    shared: Arc<Shared<F>>,
}

impl<F> SourceUse<F> {
    pub fn new(capacity: NonZeroUsize) -> Result<Self, TryReserveError> {
        let mut records = Vec::new();
        records.try_reserve_exact(capacity.get())?;
        Ok(Self {
            shared: Arc::new(Shared(Mutex::new(State {
                phase: Phase::Open,
                unresolved: 0,
                records,
                capacity: capacity.get(),
            }))),
        })
    }

    /// Reserve accounting capacity before entering a native submission path.
    pub fn begin(&self) -> Result<Submission<F>, GateError> {
        let mut state = self.shared.lock();
        if state.phase != Phase::Open {
            return Err(GateError::AdmissionClosed);
        }
        if state.unresolved + state.records.len() == state.capacity {
            return Err(GateError::Capacity);
        }
        state.unresolved += 1;
        Ok(Submission {
            shared: Arc::clone(&self.shared),
            unresolved: true,
        })
    }

    /// Permanently close new admission; issued permits must still be resolved.
    pub fn close(&self) {
        let mut state = self.shared.lock();
        if state.phase == Phase::Open {
            state.phase = Phase::Closing;
        }
    }

    /// Take the result once every issued permit has resolved, without waiting
    /// for its native records to signal. Transport and source identity remain
    /// the controller owner's obligations.
    pub fn finish(&self) -> Result<ClosedUse<F>, GateError> {
        let mut state = self.shared.lock();
        if state.phase == Phase::Finished {
            return Err(GateError::AlreadyFinished);
        }
        if state.phase == Phase::Open {
            return Err(GateError::AdmissionOpen);
        }
        if state.unresolved != 0 {
            return Err(GateError::UnresolvedSubmissions);
        }
        let failed = state.phase == Phase::Failed;
        let records = std::mem::take(&mut state.records);
        state.phase = Phase::Finished;
        Ok(if failed {
            ClosedUse::Failed(records)
        } else {
            ClosedUse::Released(records)
        })
    }
}

impl<F> Drop for SourceUse<F> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        if state.phase != Phase::Finished {
            state.phase = Phase::Failed;
        }
    }
}

/// A single-use permit tied to its originating controller by shared ownership.
/// Dropping it unresolved records terminal failure, never successful cancellation.
///
/// ```compile_fail
/// use drm_display_executor::scheduler::source_use::SourceUse;
/// use std::num::NonZeroUsize;
/// let owner = SourceUse::<u32>::new(NonZeroUsize::new(1).unwrap()).unwrap();
/// let permit = owner.begin().unwrap();
/// permit.submitted(7);
/// permit.cancel_unsubmitted();
/// ```
#[must_use = "resolve the submission permit or deliberately abandon the source use"]
pub struct Submission<F> {
    shared: Arc<Shared<F>>,
    unresolved: bool,
}

impl<F> Submission<F> {
    /// Account accepted native work before reporting the permit resolved.
    /// `record` must cover all reads accepted under this permit. The worker
    /// must not use a future userspace response as the completion record.
    pub fn submitted(mut self, record: F) {
        let mut state = self.shared.lock();
        // Capacity was reserved before admission. No allocation or external
        // callback separates storing the record from resolving the permit.
        state.records.push(record);
        state.unresolved -= 1;
        self.unresolved = false;
    }

    /// Resolve a permit only when no native read was accepted under it.
    pub fn cancel_unsubmitted(mut self) {
        let mut state = self.shared.lock();
        state.unresolved -= 1;
        self.unresolved = false;
    }
}

impl<F> Drop for Submission<F> {
    fn drop(&mut self) {
        if self.unresolved {
            let mut state = self.shared.lock();
            state.unresolved -= 1;
            state.phase = Phase::Failed;
        }
    }
}
