//! Store-owned persistent exact-scan worker pool.

use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use crate::scan::topk::BoundedTopK;
use crate::scan::{
    PartitionScan, ScanError, ScanGeometry, ScanOptions, ScanOutcome, ScanRequest, ScanStats,
    scan_geometry, scan_partition,
};

use super::cancel::{QueryCancellation, QueryControl, QueryError};
use super::stats::{Accounted, Accounting, AllocationComponent};
use super::{SnapshotLease, StoreError};

enum WorkerMessage {
    Run(WorkItem),
    Stop,
    #[cfg(any(test, feature = "test-support"))]
    PanicForTest,
}

#[derive(Clone, Copy)]
struct WorkItem {
    query: usize,
    slot: usize,
    start: usize,
    end: usize,
}

impl WorkItem {
    fn run(self) {
        let execution = unsafe {
            // SAFETY: `QueryPool::execute` boxes this `QueryExecution` and waits
            // for every submitted slot before dropping it. `query` is that
            // stable box address, and workers only borrow its immutable request.
            &*(self.query as *const QueryExecution<'static>)
        };
        execution.run_partition(self.slot, self.start..self.end);
    }

    fn report_panic(self) {
        let execution = unsafe {
            // SAFETY: the same boxed-execution lifetime proof as `run` applies;
            // catch_unwind reports the panic before this work slot completes.
            &*(self.query as *const QueryExecution<'static>)
        };
        execution.complete(self.slot, Err(ScanError::WorkerPanicked));
    }
}

struct Worker {
    sender: mpsc::Sender<WorkerMessage>,
    handle: Option<JoinHandle<()>>,
}

/// Lazily started persistent threads owned by one store handle.
pub(crate) struct QueryPool {
    workers: Mutex<Option<Accounted<Vec<Worker>>>>,
    worker_count: usize,
}

impl QueryPool {
    pub(crate) fn start(
        worker_count: usize,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        static NEXT_POOL: AtomicU64 = AtomicU64::new(1);

        let pool_id = NEXT_POOL.fetch_add(1, Ordering::Relaxed);
        let mut workers =
            Accounted::try_with_capacity(accounting, worker_count, AllocationComponent::QueryPool)?;
        for worker_index in 0..worker_count {
            let (sender, receiver) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::sync_channel(0);
            let handle = match std::thread::Builder::new()
                .name(format!("ze-query-{pool_id}-{worker_index}"))
                .spawn(move || {
                    if ready_tx.send(()).is_err() {
                        return;
                    }
                    while let Ok(message) = receiver.recv() {
                        match message {
                            WorkerMessage::Run(work) => {
                                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                    work.run();
                                }));
                                if result.is_err() {
                                    work.report_panic();
                                }
                            }
                            WorkerMessage::Stop => return,
                            #[cfg(any(test, feature = "test-support"))]
                            WorkerMessage::PanicForTest => {
                                std::panic::resume_unwind(Box::new(
                                    "injected query worker panic".to_owned(),
                                ));
                            }
                        }
                    }
                }) {
                Ok(handle) => handle,
                Err(source) => {
                    stop_workers_best_effort(workers.as_mut_slice());
                    return Err(StoreError::QueryPoolStart { source });
                }
            };
            if ready_rx.recv().is_err() {
                workers.push(Worker {
                    sender,
                    handle: Some(handle),
                })?;
                stop_workers_best_effort(workers.as_mut_slice());
                return Err(StoreError::QueryPoolHandshake);
            }
            workers.push(Worker {
                sender,
                handle: Some(handle),
            })?;
        }
        Ok(Self {
            workers: Mutex::new(Some(workers)),
            worker_count,
        })
    }

    pub(crate) fn execute(
        &self,
        request: ScanRequest<'_>,
        k: usize,
        options: ScanOptions,
        control: QueryControl,
        lease: SnapshotLease,
    ) -> Result<ScanOutcome, QueryError> {
        let geometry = scan_geometry(request).map_err(QueryError::Scan)?;
        let requested = if options.thread_budget == 0 {
            self.worker_count
        } else {
            options.thread_budget.min(self.worker_count)
        };
        let workers = requested.min(geometry.work_units.max(1));
        let ranges = partition_row_count(geometry.row_count, workers).map_err(QueryError::Scan)?;
        let execution = Box::new(QueryExecution::new(request, k, control, lease, workers));
        let query = (&raw const *execution) as usize;

        let worker_guard = self.workers.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "query pool",
            })
        })?;
        let available = worker_guard.as_ref().ok_or({
            QueryError::Store(StoreError::Synchronization {
                component: "stopped query pool",
            })
        })?;
        for (slot, (worker, range)) in available.iter().zip(ranges).enumerate() {
            let work = WorkItem {
                query,
                slot,
                start: range.start,
                end: range.end,
            };
            if worker.sender.send(WorkerMessage::Run(work)).is_err() {
                execution.complete(slot, Err(ScanError::WorkerPanicked));
            }
        }
        drop(worker_guard);

        let partitions = execution.wait()?;
        merge_partitions(partitions, geometry, workers, k).map_err(QueryError::Scan)
    }

    pub(crate) fn stop_and_join(&self) -> Result<(), StoreError> {
        let mut slot = self
            .workers
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "query pool",
            })?;
        let Some(mut workers) = slot.take() else {
            return Ok(());
        };
        drop(slot);
        for worker in workers.iter() {
            let _ = worker.sender.send(WorkerMessage::Stop);
        }
        let mut panicked = false;
        for worker in workers.as_mut_slice() {
            if let Some(handle) = worker.handle.take()
                && handle.join().is_err()
            {
                panicked = true;
            }
        }
        if panicked {
            Err(StoreError::QueryPoolThreadPanicked)
        } else {
            Ok(())
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn panic_one_and_join(&self) -> Result<(), StoreError> {
        let workers = self
            .workers
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "query pool",
            })?;
        let worker = workers.as_ref().and_then(|workers| workers.first()).ok_or(
            StoreError::Synchronization {
                component: "stopped query pool",
            },
        )?;
        worker
            .sender
            .send(WorkerMessage::PanicForTest)
            .map_err(|_| StoreError::QueryPoolThreadPanicked)?;
        drop(workers);
        self.stop_and_join()
    }

    fn stop_best_effort(&mut self) {
        let workers = match self.workers.get_mut() {
            Ok(slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(mut workers) = workers {
            stop_workers_best_effort(workers.as_mut_slice());
        }
    }
}

impl Drop for QueryPool {
    fn drop(&mut self) {
        self.stop_best_effort();
    }
}

type LexicalJob = Box<dyn FnOnce() + Send + 'static>;

enum LexicalWorkerMessage {
    Run(LexicalWorkItem),
    Stop,
}

struct LexicalWorkItem {
    run: LexicalJob,
    report_panic: LexicalJob,
}

struct LexicalWorkerThread {
    sender: mpsc::Sender<LexicalWorkerMessage>,
    handle: Option<JoinHandle<()>>,
}

enum LexicalReply<R> {
    Completed(R),
    Panicked,
}

struct PendingLexical<R> {
    receiver: Option<mpsc::Receiver<LexicalReply<R>>>,
}

impl<R> PendingLexical<R> {
    pub(crate) fn wait(mut self) -> Result<R, crate::fusion::FusionError> {
        let receiver =
            self.receiver
                .take()
                .ok_or_else(|| crate::fusion::FusionError::LegThreadStart {
                    leg: crate::fusion::FusionLeg::Lexical,
                    detail: "pooled lexical worker result was already consumed".to_owned(),
                })?;
        match receiver.recv() {
            Ok(LexicalReply::Completed(result)) => Ok(result),
            Ok(LexicalReply::Panicked) => Err(crate::fusion::FusionError::LegPanic {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: "lexical hybrid leg panicked",
            }),
            Err(_) => Err(crate::fusion::FusionError::LegThreadStart {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: "pooled lexical worker disconnected before reporting a result".to_owned(),
            }),
        }
    }
}

impl<R> Drop for PendingLexical<R> {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.recv();
        }
    }
}

/// Lazily started persistent worker for one store's hybrid lexical leg.
pub(crate) struct LexicalWorker {
    worker: Mutex<Option<LexicalWorkerThread>>,
}

impl LexicalWorker {
    pub(crate) fn start() -> Result<Self, crate::fusion::FusionError> {
        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let handle = std::thread::Builder::new()
            .name("zeppelin-fts".to_owned())
            .spawn(move || {
                if ready_tx.send(()).is_err() {
                    return;
                }
                while let Ok(message) = receiver.recv() {
                    match message {
                        LexicalWorkerMessage::Run(work) => {
                            let result = std::panic::catch_unwind(AssertUnwindSafe(work.run));
                            if result.is_err() {
                                (work.report_panic)();
                            }
                        }
                        LexicalWorkerMessage::Stop => return,
                    }
                }
            })
            .map_err(|error| crate::fusion::FusionError::LegThreadStart {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: error.to_string(),
            })?;
        if ready_rx.recv().is_err() {
            let _ = sender.send(LexicalWorkerMessage::Stop);
            let _ = handle.join();
            return Err(crate::fusion::FusionError::LegThreadStart {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: "pooled lexical worker startup handshake failed".to_owned(),
            });
        }
        Ok(Self {
            worker: Mutex::new(Some(LexicalWorkerThread {
                sender,
                handle: Some(handle),
            })),
        })
    }

    /// Runs borrowed lexical work while the caller executes its vector leg.
    /// The pending guard cannot escape this scope, even through `forget`;
    /// on caller unwind its destructor still joins the lexical job before
    /// any caller-owned borrow can expire.
    pub(crate) fn run_scoped<'scope, F, R, G, S>(
        &self,
        work: F,
        concurrent: G,
    ) -> Result<(S, Result<R, crate::fusion::FusionError>), crate::fusion::FusionError>
    where
        F: FnOnce() -> R + Send + 'scope,
        R: Send + 'scope,
        G: FnOnce() -> S,
    {
        let worker =
            self.worker
                .lock()
                .map_err(|_| crate::fusion::FusionError::LegThreadStart {
                    leg: crate::fusion::FusionLeg::Lexical,
                    detail: "pooled lexical worker synchronization was poisoned".to_owned(),
                })?;
        let sender = worker
            .as_ref()
            .map(|worker| worker.sender.clone())
            .ok_or_else(|| crate::fusion::FusionError::LegThreadStart {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: "pooled lexical worker was stopped".to_owned(),
            })?;
        drop(worker);

        let (response_tx, response_rx) = mpsc::sync_channel(0);
        let completed_tx = response_tx.clone();
        let run: Box<dyn FnOnce() + Send + 'scope> = Box::new(move || {
            let result = work();
            let _ = completed_tx.send(LexicalReply::Completed(result));
        });
        let report_panic: Box<dyn FnOnce() + Send + 'scope> = Box::new(move || {
            let _ = response_tx.send(LexicalReply::Panicked);
        });
        let run = unsafe {
            // SAFETY: the local pending guard below cannot escape. It waits
            // on return or unwind while every borrow captured by `work` is
            // still live. FnOnce consumes and drops the captures before the
            // completion reply is sent.
            std::mem::transmute::<Box<dyn FnOnce() + Send + 'scope>, LexicalJob>(run)
        };
        let report_panic = unsafe {
            // SAFETY: this callback has the same scoped lifetime and completion
            // handshake as `run`; it owns no borrow beyond that scope.
            std::mem::transmute::<Box<dyn FnOnce() + Send + 'scope>, LexicalJob>(report_panic)
        };
        sender
            .send(LexicalWorkerMessage::Run(LexicalWorkItem {
                run,
                report_panic,
            }))
            .map_err(|_| crate::fusion::FusionError::LegThreadStart {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: "pooled lexical worker disconnected during submission".to_owned(),
            })?;
        let pending = PendingLexical {
            receiver: Some(response_rx),
        };
        let result = concurrent();
        Ok((result, pending.wait()))
    }

    pub(crate) fn stop_and_join(&self) -> Result<(), StoreError> {
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "pooled lexical worker",
            })?;
        let Some(mut worker) = slot.take() else {
            return Ok(());
        };
        drop(slot);
        let _ = worker.sender.send(LexicalWorkerMessage::Stop);
        if let Some(handle) = worker.handle.take() {
            handle
                .join()
                .map_err(|_| StoreError::QueryPoolThreadPanicked)?;
        }
        Ok(())
    }

    pub(crate) fn stop_best_effort(&self) {
        let mut slot = match self.worker.try_lock() {
            Ok(slot) => slot,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        if let Some(mut worker) = slot.take() {
            let _ = worker.sender.send(LexicalWorkerMessage::Stop);
            drop(worker.handle.take());
        }
    }
}

impl Drop for LexicalWorker {
    fn drop(&mut self) {
        let slot = match self.worker.get_mut() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(mut worker) = slot.take() {
            let _ = worker.sender.send(LexicalWorkerMessage::Stop);
            drop(worker.handle.take());
        }
    }
}

fn stop_workers_best_effort(workers: &mut [Worker]) {
    for worker in &*workers {
        let _ = worker.sender.send(WorkerMessage::Stop);
    }
    for worker in workers {
        if let Some(handle) = worker.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Completion {
    remaining: usize,
    results: Vec<Option<Result<PartitionScan, ScanError>>>,
}

struct QueryExecution<'a> {
    request: ScanRequest<'a>,
    k: usize,
    control: QueryControl,
    lease: SnapshotLease,
    completion: Mutex<Completion>,
    changed: Condvar,
}

impl<'a> QueryExecution<'a> {
    fn new(
        request: ScanRequest<'a>,
        k: usize,
        control: QueryControl,
        lease: SnapshotLease,
        workers: usize,
    ) -> Self {
        let results = std::iter::repeat_with(|| None).take(workers).collect();
        Self {
            request,
            k,
            control,
            lease,
            completion: Mutex::new(Completion {
                remaining: workers,
                results,
            }),
            changed: Condvar::new(),
        }
    }

    fn run_partition(&self, slot: usize, range: Range<usize>) {
        let cancellation = QueryCancellation::new(&self.control, &self.lease);
        let result = scan_partition(self.request, self.k, range, Some(&cancellation));
        self.complete(slot, result);
    }

    fn complete(&self, slot: usize, result: Result<PartitionScan, ScanError>) {
        let mut completion = match self.completion.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(target) = completion.results.get_mut(slot)
            && target.is_none()
        {
            *target = Some(result);
            completion.remaining = completion.remaining.saturating_sub(1);
        }
        self.changed.notify_all();
    }

    fn wait(&self) -> Result<Vec<PartitionScan>, QueryError> {
        let mut completion = self.completion.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "query completion",
            })
        })?;
        if let Some(deadline) = self.control.deadline()
            && self.control.error().is_none()
            && self.control.now().is_some_and(|now| now >= deadline)
        {
            self.control.mark_timed_out();
        }
        while completion.remaining > 0 {
            if let Some(deadline) = self.control.deadline()
                && self.control.error().is_none()
            {
                let now = self.control.now().unwrap_or(deadline);
                if now >= deadline {
                    self.control.mark_timed_out();
                    continue;
                }
                let remaining = deadline.saturating_duration_since(now);
                let waited = self
                    .changed
                    .wait_timeout(completion, remaining)
                    .map_err(|_| {
                        QueryError::Store(StoreError::Synchronization {
                            component: "query completion",
                        })
                    })?;
                completion = waited.0;
                continue;
            }
            completion = self.changed.wait(completion).map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "query completion",
                })
            })?;
        }

        if self.lease.is_cancelled() {
            return Err(QueryError::ReadCancelled { partial: false });
        }
        if let Some(error) = self.control.error() {
            return Err(error);
        }
        let results = std::mem::take(&mut completion.results);
        drop(completion);
        let mut partitions = Vec::with_capacity(results.len());
        for result in results {
            let result = result.ok_or({
                QueryError::Store(StoreError::Synchronization {
                    component: "query result slot",
                })
            })?;
            partitions.push(result.map_err(QueryError::Scan)?);
        }
        Ok(partitions)
    }
}

fn partition_row_count(row_count: usize, workers: usize) -> Result<Vec<Range<usize>>, ScanError> {
    let mut ranges = Vec::with_capacity(workers);
    for worker in 0..workers {
        let start = worker
            .checked_mul(row_count)
            .ok_or(ScanError::ArithmeticOverflow)?
            / workers;
        let end = worker
            .checked_add(1)
            .and_then(|value| value.checked_mul(row_count))
            .ok_or(ScanError::ArithmeticOverflow)?
            / workers;
        ranges.push(start..end);
    }
    Ok(ranges)
}

fn merge_partitions(
    partitions: Vec<PartitionScan>,
    geometry: ScanGeometry,
    workers: usize,
    k: usize,
) -> Result<ScanOutcome, ScanError> {
    let mut merged = BoundedTopK::new(k.min(geometry.row_count));
    let mut dims_touched = 0_u64;
    let mut bytes_read = 0_u64;
    let mut worker_thread_ids = Vec::with_capacity(workers);
    for partition in partitions {
        dims_touched = dims_touched
            .checked_add(partition.dims_touched)
            .ok_or(ScanError::ArithmeticOverflow)?;
        bytes_read = bytes_read
            .checked_add(partition.bytes_read)
            .ok_or(ScanError::ArithmeticOverflow)?;
        if !worker_thread_ids.contains(&partition.worker_thread_id) {
            worker_thread_ids.push(partition.worker_thread_id);
        }
        for candidate in partition.candidates {
            merged.push(candidate);
        }
    }
    Ok(ScanOutcome {
        candidates: merged.into_sorted_with_ties(),
        worst_score: None,
        stats: ScanStats {
            dims_touched,
            bytes_read,
            threads_used: workers,
            worker_thread_ids,
        },
    })
}
