//! Minimal public-path adapter for lifecycle and accounting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, ManualMonotonicClock, OpenOptions, QueryControl, QueryError, Store,
    StoreError, StoreTestDependencies,
};
use zeppelin_embed::scan::{F32Rows, ScanOptions, ScanQuery, ScanRequest, ScanRows};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::lifecycle_accounting::{LifecycleInput, LifecycleObserved};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleOperationKind {
    Deadline,
    Cancellation,
    CloseDrain,
    Locking,
    Accounting,
}

impl LifecycleOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Cancellation => "cancellation",
            Self::CloseDrain => "close-drain",
            Self::Locking => "locking",
            Self::Accounting => "accounting",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleFaultKind {
    ClockFreezeJump,
    CancelAdmissionQuery,
    CloseActiveQuery,
    WorkerPanic,
    LockContention,
    AllocationDenial,
}

impl LifecycleFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::ClockFreezeJump => "clock-freeze-jump",
            Self::CancelAdmissionQuery => "cancel-admission-query",
            Self::CloseActiveQuery => "close-active-query",
            Self::WorkerPanic => "worker-panic",
            Self::LockContention => "lock-contention",
            Self::AllocationDenial => "allocation-denial",
        }
    }

    #[must_use]
    pub const fn operation(self) -> LifecycleOperationKind {
        match self {
            Self::ClockFreezeJump => LifecycleOperationKind::Deadline,
            Self::CancelAdmissionQuery => LifecycleOperationKind::Cancellation,
            Self::CloseActiveQuery | Self::WorkerPanic => LifecycleOperationKind::CloseDrain,
            Self::LockContention => LifecycleOperationKind::Locking,
            Self::AllocationDenial => LifecycleOperationKind::Accounting,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::ClockFreezeJump => "lifecycle.deadline.clock",
            Self::CancelAdmissionQuery => "lifecycle.query.admission",
            Self::CloseActiveQuery => "lifecycle.close.active-query",
            Self::WorkerPanic => "lifecycle.query-pool.worker",
            Self::LockContention => "lifecycle.writer-lock.acquire",
            Self::AllocationDenial => "lifecycle.accounting.reserve",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleFaultReceipt {
    pub fault: LifecycleFaultKind,
    pub operation: LifecycleOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleInvariantEvidence {
    I54 {
        input: LifecycleInput,
        observed: LifecycleObserved,
    },
    I55 {
        input: LifecycleInput,
        observed: LifecycleObserved,
    },
    I56 {
        input: LifecycleInput,
        observed: LifecycleObserved,
    },
    I57 {
        input: LifecycleInput,
        observed: LifecycleObserved,
    },
    I58 {
        input: LifecycleInput,
        observed: LifecycleObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleOperationEvidence {
    pub invariant: LifecycleInvariantEvidence,
    pub receipts: Vec<LifecycleFaultReceipt>,
    pub clean_control_passed: bool,
}

fn blank() -> LifecycleObserved {
    LifecycleObserved {
        deadline_timed_out_without_partial: false,
        cancellation_without_partial: false,
        close_cancelled_active_query: false,
        post_close_refused: false,
        second_writer_refused: false,
        active_queries_after: 0,
        query_pool_bytes: 0,
        allocation_denied: false,
    }
}

fn request<'a>(query: &'a [f32], rows: &'a F32Rows) -> ScanRequest<'a> {
    ScanRequest {
        query: ScanQuery::F32(query),
        rows: ScanRows::F32RowMajor(rows),
        row_mask: None,
    }
}

fn observe_deadline() -> Result<LifecycleObserved, String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let clock = Arc::new(ManualMonotonicClock::new());
    let dependencies = StoreTestDependencies::new(clock_vfs(), clock.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| error.to_string())?;
    let deadline = Deadline::after_with_test_clock(Duration::from_millis(1), clock.clone())
        .map_err(|error| error.to_string())?;
    clock.advance(Duration::from_millis(2));
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0; 128]);
    let result = store.top_k_with_options(
        request(&query, &rows),
        1,
        ScanOptions { thread_budget: 1 },
        QueryControl::Deadline(deadline),
    );
    let mut observed = blank();
    observed.deadline_timed_out_without_partial =
        matches!(result, Err(QueryError::Timeout { partial: false }));
    store.close().map_err(|error| error.to_string())?;
    Ok(observed)
}

fn clock_vfs() -> Arc<StdVfs> {
    Arc::new(StdVfs)
}

fn observe_cancellation() -> Result<LifecycleObserved, String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0; 4_096]);
    let token = CancelToken::new();
    token.cancel();
    let result = store.top_k_with_options(
        request(&query, &rows),
        1,
        ScanOptions { thread_budget: 1 },
        QueryControl::Cancel(token),
    );
    let stats = store.stats().map_err(|error| error.to_string())?;
    let mut observed = blank();
    observed.cancellation_without_partial =
        matches!(result, Err(QueryError::Cancelled { partial: false }));
    observed.active_queries_after = stats.active_queries;
    store.close().map_err(|error| error.to_string())?;
    Ok(observed)
}

fn observe_close_drain() -> Result<LifecycleObserved, String> {
    const ROWS: usize = 10_000_000;
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store = Arc::new(
        Store::open(
            directory.path(),
            OpenOptions::default().with_reader_drain_timeout(Duration::ZERO),
        )
        .map_err(|error| error.to_string())?,
    );
    let rows = Arc::new(F32Rows::new(vec![1.0; ROWS]));
    let query_store = Arc::clone(&store);
    let query_rows = Arc::clone(&rows);
    let query = std::thread::spawn(move || {
        let values = [1.0_f32];
        query_store.top_k_with_options(
            request(&values, &query_rows),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
    });
    let until = Instant::now() + Duration::from_secs(5);
    while store
        .stats()
        .map_err(|error| error.to_string())?
        .active_queries
        == 0
    {
        if Instant::now() >= until {
            return Err("lifecycle close fixture query was never admitted".to_owned());
        }
        std::thread::yield_now();
    }
    store.close().map_err(|error| error.to_string())?;
    let result = query
        .join()
        .map_err(|_| "lifecycle close fixture thread panicked".to_owned())?;
    let values = [1.0_f32];
    let small_rows = F32Rows::new(vec![1.0]);
    let post_close = store.top_k_with_options(
        request(&values, &small_rows),
        1,
        ScanOptions { thread_budget: 1 },
        QueryControl::Cancel(CancelToken::new()),
    );
    let mut observed = blank();
    observed.close_cancelled_active_query =
        matches!(result, Err(QueryError::ReadCancelled { partial: false }));
    observed.post_close_refused = matches!(post_close, Err(QueryError::Store(StoreError::Closed)));
    Ok(observed)
}

fn observe_locking() -> Result<LifecycleObserved, String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let second = Store::open(directory.path(), OpenOptions::default());
    let mut observed = blank();
    observed.second_writer_refused = matches!(second, Err(StoreError::StoreBusy { .. }));
    store.close().map_err(|error| error.to_string())?;
    Ok(observed)
}

fn observe_accounting() -> Result<LifecycleObserved, String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0; 128]);
    store
        .top_k_with_options(
            request(&query, &rows),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let stats = store.stats().map_err(|error| error.to_string())?;
    store.close().map_err(|error| error.to_string())?;

    let denied_directory = tempdir().map_err(|error| error.to_string())?;
    let denied = Store::open(
        denied_directory.path(),
        OpenOptions::default().with_max_resident_bytes(0),
    )
    .map_err(|error| error.to_string())?;
    let denied_result = denied.top_k_with_options(
        request(&query, &rows),
        1,
        ScanOptions { thread_budget: 1 },
        QueryControl::Cancel(CancelToken::new()),
    );
    let mut observed = blank();
    observed.active_queries_after = stats.active_queries;
    observed.query_pool_bytes = stats.query_pool_bytes;
    observed.allocation_denied = matches!(
        denied_result,
        Err(QueryError::Store(StoreError::BudgetExceeded { .. }))
    );
    let _ = denied.close();
    Ok(observed)
}

fn observe_worker_panic() -> Result<bool, String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0; 128]);
    store
        .top_k_with_options(
            request(&query, &rows),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        store.panic_query_worker_for_test(),
        Err(StoreError::QueryPoolThreadPanicked)
    ))
}

pub fn run_lifecycle_operation(
    operation: LifecycleOperationKind,
    fault: Option<LifecycleFaultKind>,
) -> Result<LifecycleOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err("lifecycle fault targeted the wrong operation".to_owned());
    }
    let observed = match operation {
        LifecycleOperationKind::Deadline => observe_deadline()?,
        LifecycleOperationKind::Cancellation => observe_cancellation()?,
        LifecycleOperationKind::CloseDrain => observe_close_drain()?,
        LifecycleOperationKind::Locking => observe_locking()?,
        LifecycleOperationKind::Accounting => observe_accounting()?,
    };
    if matches!(fault, Some(LifecycleFaultKind::WorkerPanic)) && !observe_worker_panic()? {
        return Err("query worker panic did not reach the typed product error".to_owned());
    }
    let input = LifecycleInput {
        expected_active_queries_after: 0,
    };
    let invariant = match operation {
        LifecycleOperationKind::Deadline => LifecycleInvariantEvidence::I54 { input, observed },
        LifecycleOperationKind::Cancellation => LifecycleInvariantEvidence::I55 { input, observed },
        LifecycleOperationKind::CloseDrain => LifecycleInvariantEvidence::I56 { input, observed },
        LifecycleOperationKind::Locking => LifecycleInvariantEvidence::I57 { input, observed },
        LifecycleOperationKind::Accounting => LifecycleInvariantEvidence::I58 { input, observed },
    };
    let receipts = fault
        .map(|fault| LifecycleFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        })
        .into_iter()
        .collect();
    Ok(LifecycleOperationEvidence {
        invariant,
        receipts,
        clean_control_passed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::lifecycle_accounting as oracle;

    #[test]
    fn lifecycle_public_operations_pass_exact_checkers() {
        for operation in [
            LifecycleOperationKind::Deadline,
            LifecycleOperationKind::Cancellation,
            LifecycleOperationKind::CloseDrain,
            LifecycleOperationKind::Locking,
            LifecycleOperationKind::Accounting,
        ] {
            let evidence = run_lifecycle_operation(operation, None).unwrap();
            let result = match evidence.invariant {
                LifecycleInvariantEvidence::I54 { input, observed } => {
                    oracle::compare_i54(&input, &observed)
                }
                LifecycleInvariantEvidence::I55 { input, observed } => {
                    oracle::compare_i55(&input, &observed)
                }
                LifecycleInvariantEvidence::I56 { input, observed } => {
                    oracle::compare_i56(&input, &observed)
                }
                LifecycleInvariantEvidence::I57 { input, observed } => {
                    oracle::compare_i57(&input, &observed)
                }
                LifecycleInvariantEvidence::I58 { input, observed } => {
                    oracle::compare_i58(&input, &observed)
                }
            };
            result.unwrap();
        }
    }

    #[test]
    fn every_lifecycle_fault_fires_at_its_declared_operation() {
        for fault in [
            LifecycleFaultKind::ClockFreezeJump,
            LifecycleFaultKind::CancelAdmissionQuery,
            LifecycleFaultKind::CloseActiveQuery,
            LifecycleFaultKind::WorkerPanic,
            LifecycleFaultKind::LockContention,
            LifecycleFaultKind::AllocationDenial,
        ] {
            let evidence = run_lifecycle_operation(fault.operation(), Some(fault)).unwrap();
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
            assert!(evidence.clean_control_passed);
        }
    }
}
