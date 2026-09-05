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
enum WorkKind {
    Scan,
    Exact,
}

#[derive(Clone, Copy)]
struct WorkItem {
    kind: WorkKind,
    query: usize,
    slot: usize,
    start: usize,
    end: usize,
}

impl WorkItem {
    fn run(self) {
        if matches!(self.kind, WorkKind::Exact) {
            let execution = unsafe {
                // SAFETY: execute_exact keeps its accounted execution at a stable
                // address; ExactExecution::drop joins every admitted slot on all
                // return/unwind paths before any borrowed request can be released.
                &*(self.query as *const ExactExecution<'static>)
            };
            execution.run_partition(self.slot, self.start..self.end);
            return;
        }
        let execution = unsafe {
            // SAFETY: `QueryPool::execute` boxes this `QueryExecution` and waits
            // for every submitted slot before dropping it. `query` is that
            // stable box address, and workers only borrow its immutable request.
            &*(self.query as *const QueryExecution<'static>)
        };
        execution.run_partition(self.slot, self.start..self.end);
    }

    fn report_panic(self) {
        if matches!(self.kind, WorkKind::Exact) {
            let execution = unsafe {
                // SAFETY: the same join-before-release proof as run applies.
                &*(self.query as *const ExactExecution<'static>)
            };
            execution.complete(
                self.slot,
                Err(super::ExactScanError::Query(QueryError::Scan(
                    ScanError::WorkerPanicked,
                ))),
            );
            return;
        }
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
    next_exact_worker: AtomicU64,
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
            next_exact_worker: AtomicU64::new(0),
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
                kind: WorkKind::Scan,
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

    // Four shared workers improve the measured one/two-caller FiQA scans while
    // bounding contention across callers; the quantized pool stays wider.
    fn exact_capacity(&self) -> usize {
        self.worker_count.min(4)
    }

    // Calibrated on row/dimension boundary cases, including all-score ties.
    pub(crate) fn exact_workers(&self, rows: usize, dimension: usize, requested: usize) -> usize {
        if rows < 256 || rows.saturating_mul(dimension) < 262_144 {
            return 1;
        }
        let requested = if requested == 0 { 4 } else { requested };
        requested.min(self.exact_capacity()).min(rows / 64).max(1)
    }

    pub(crate) fn reserve_exact_lane(&self, requested: usize) -> usize {
        let count = (if requested == 0 { 4 } else { requested })
            .min(self.exact_capacity())
            .max(1);
        self.next_exact_worker
            .fetch_add(count as u64, Ordering::Relaxed) as usize
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_exact(
        &self,
        vectors: &[f32],
        row_count: usize,
        alive: &crate::meta::AliveSet,
        query_vector: &[f32],
        k: usize,
        requested: usize,
        offset: usize,
        control: &QueryControl,
        lease: &SnapshotLease,
        accounting: &Arc<Accounting>,
        memory: &mut super::ExactScanMemory,
        #[cfg(any(test, feature = "test-support"))] controller: Option<
            &crate::scan::vector_fault::VectorFaultController,
        >,
        #[cfg(any(test, feature = "test-support"))]
        source: crate::scan::vector_fault::VectorRowSource,
        #[cfg(any(test, feature = "test-support"))]
        tier: crate::scan::vector_fault::VectorSearchTier,
    ) -> Result<ScanOutcome, QueryError> {
        let workers = self.exact_workers(row_count, query_vector.len(), requested);
        #[cfg(any(test, feature = "test-support"))]
        if controller.is_some_and(|controller| controller.deny_exact_reservation()) {
            return Err(QueryError::Store(StoreError::AllocationFailed {
                needed: std::mem::size_of::<ExactExecution<'_>>() as u64,
                component: "exact query execution",
            }));
        }
        // The fixed arena supplies both stable address and pre-allocation charge.
        let mut owner = Accounted::try_with_capacity(accounting, 1, AllocationComponent::Temporary)
            .map_err(QueryError::Store)?;
        owner
            .push(ExactExecution::new(
                vectors,
                row_count,
                alive,
                query_vector,
                k,
                control,
                lease,
                accounting,
                workers,
                #[cfg(any(test, feature = "test-support"))]
                controller,
                #[cfg(any(test, feature = "test-support"))]
                source,
                #[cfg(any(test, feature = "test-support"))]
                tier,
            )?)
            .map_err(QueryError::Store)?;
        let execution = owner
            .first()
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        let query = std::ptr::from_ref(execution) as usize;
        let guard = self.workers.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "query pool",
            })
        })?;
        let available = guard
            .as_ref()
            .ok_or(QueryError::Store(StoreError::Synchronization {
                component: "stopped query pool",
            }))?;
        let available = available
            .as_slice()
            .get(..self.exact_capacity())
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        let offset = offset
            .checked_rem(available.len())
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        for slot in 0..workers {
            let start = slot
                .checked_mul(row_count)
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?
                / workers;
            let end = (slot + 1)
                .checked_mul(row_count)
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?
                / workers;
            let worker = offset
                .checked_add(slot)
                .and_then(|index| available.get(index % available.len()))
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
            execution.admit()?;
            let work = WorkItem {
                kind: WorkKind::Exact,
                query,
                slot,
                start,
                end,
            };
            if worker.sender.send(WorkerMessage::Run(work)).is_err() {
                execution.complete(
                    slot,
                    Err(super::ExactScanError::Query(QueryError::Scan(
                        ScanError::WorkerPanicked,
                    ))),
                );
            }
        }
        drop(guard);
        #[cfg(any(test, feature = "test-support"))]
        if controller.is_some_and(|controller| controller.panic_exact_caller()) {
            std::panic::resume_unwind(Box::new("injected exact caller unwind"));
        }
        execution.wait_and_merge(k, memory)
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

struct OwnedExactPartition {
    outcome: ScanOutcome,
    _memory: super::ExactScanMemory,
}

struct ExactCompletion {
    remaining: usize,
    results: Accounted<Vec<Option<Result<OwnedExactPartition, super::ExactScanError>>>>,
}

struct ExactExecution<'a> {
    vectors: &'a [f32],
    row_count: usize,
    alive: &'a crate::meta::AliveSet,
    query: &'a [f32],
    k: usize,
    control: &'a QueryControl,
    lease: &'a SnapshotLease,
    accounting: &'a Arc<Accounting>,
    completion: Mutex<ExactCompletion>,
    changed: Condvar,
    #[cfg(any(test, feature = "test-support"))]
    queued_at: Option<std::time::Instant>,
    #[cfg(any(test, feature = "test-support"))]
    controller: Option<&'a crate::scan::vector_fault::VectorFaultController>,
    #[cfg(any(test, feature = "test-support"))]
    source: crate::scan::vector_fault::VectorRowSource,
    #[cfg(any(test, feature = "test-support"))]
    tier: crate::scan::vector_fault::VectorSearchTier,
}

impl<'a> ExactExecution<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        vectors: &'a [f32],
        row_count: usize,
        alive: &'a crate::meta::AliveSet,
        query: &'a [f32],
        k: usize,
        control: &'a QueryControl,
        lease: &'a SnapshotLease,
        accounting: &'a Arc<Accounting>,
        workers: usize,
        #[cfg(any(test, feature = "test-support"))] controller: Option<
            &'a crate::scan::vector_fault::VectorFaultController,
        >,
        #[cfg(any(test, feature = "test-support"))]
        source: crate::scan::vector_fault::VectorRowSource,
        #[cfg(any(test, feature = "test-support"))]
        tier: crate::scan::vector_fault::VectorSearchTier,
    ) -> Result<Self, QueryError> {
        let mut results =
            Accounted::try_with_capacity(accounting, workers, AllocationComponent::Temporary)
                .map_err(QueryError::Store)?;
        for _ in 0..workers {
            results.push(None).map_err(QueryError::Store)?;
        }
        Ok(Self {
            vectors,
            row_count,
            alive,
            query,
            k,
            control,
            lease,
            accounting,
            completion: Mutex::new(ExactCompletion {
                remaining: 0,
                results,
            }),
            changed: Condvar::new(),
            #[cfg(any(test, feature = "test-support"))]
            queued_at: controller.map(|_| std::time::Instant::now()),
            #[cfg(any(test, feature = "test-support"))]
            controller,
            #[cfg(any(test, feature = "test-support"))]
            source,
            #[cfg(any(test, feature = "test-support"))]
            tier,
        })
    }

    fn admit(&self) -> Result<(), QueryError> {
        let mut state = self.completion.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "exact query completion",
            })
        })?;
        state.remaining += 1;
        Ok(())
    }

    fn run_partition(&self, slot: usize, range: Range<usize>) {
        #[cfg(any(test, feature = "test-support"))]
        let started = self.queued_at.map(|_| std::time::Instant::now());
        #[cfg(any(test, feature = "test-support"))]
        let timed_range = range.clone();
        let result = (|| {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(controller) = self.controller {
                match controller.exact_worker_fault(slot) {
                    crate::scan::vector_fault::ExactWorkerFault::None => {}
                    crate::scan::vector_fault::ExactWorkerFault::Panic => {
                        std::panic::resume_unwind(Box::new("injected exact worker panic"))
                    }
                    crate::scan::vector_fault::ExactWorkerFault::NonFinite { row_id } => {
                        return Err(super::ExactScanError::Query(QueryError::Scan(
                            ScanError::NonFiniteScore { row_id },
                        )));
                    }
                }
            }
            let mut memory = super::ExactScanMemory::new(self.accounting)?;
            let cancellation = QueryCancellation::new(self.control, self.lease);
            let outcome = super::scan_exact_partition::<true>(
                self.vectors,
                self.row_count,
                range,
                self.alive,
                self.query,
                self.k,
                &cancellation,
                &mut memory,
                #[cfg(any(test, feature = "test-support"))]
                self.controller,
                #[cfg(any(test, feature = "test-support"))]
                self.source,
                #[cfg(any(test, feature = "test-support"))]
                self.tier,
            )?;
            Ok(OwnedExactPartition {
                outcome,
                _memory: memory,
            })
        })();
        #[cfg(any(test, feature = "test-support"))]
        if let (Some(controller), Some(started), Some(queued)) =
            (self.controller, started, self.queued_at)
        {
            controller.record_exact_worker_timing(crate::scan::vector_fault::ExactWorkerTiming {
                slot,
                range: timed_range,
                queue_wait: started.saturating_duration_since(queued),
                execution: started.elapsed(),
            });
        }
        self.complete(slot, result);
    }

    fn complete(&self, slot: usize, result: Result<OwnedExactPartition, super::ExactScanError>) {
        #[cfg(any(test, feature = "test-support"))]
        let notifier = self
            .controller
            .and_then(|controller| controller.exact_completion_notifier());
        let mut state = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(target) = state.results.as_mut_slice().get_mut(slot)
            && target.is_none()
        {
            *target = Some(result);
            state.remaining = state.remaining.saturating_sub(1);
        }
        self.changed.notify_all();
        drop(state);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(notifier) = notifier {
            let _ = notifier.send(slot);
        }
    }

    fn wait_and_merge(
        &self,
        k: usize,
        memory: &mut super::ExactScanMemory,
    ) -> Result<ScanOutcome, QueryError> {
        let mut state = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(deadline) = self.control.deadline()
                && self.control.error().is_none()
                && self.control.now().is_some_and(|now| now >= deadline)
            {
                self.control.mark_timed_out();
            }
            if state.remaining == 0 {
                break;
            }
            if let Some(deadline) = self.control.deadline()
                && self.control.error().is_none()
            {
                let remaining =
                    deadline.saturating_duration_since(self.control.now().unwrap_or(deadline));
                state = self
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            } else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        if self.lease.is_cancelled() {
            return Err(QueryError::ReadCancelled { partial: false });
        }
        if let Some(error) = self.control.error() {
            return Err(error);
        }
        if self.completion.is_poisoned() {
            return Err(QueryError::Store(StoreError::Synchronization {
                component: "exact query completion",
            }));
        }
        // Completion order never selects the failure. Check every ordered slot
        // before allocating the merge, and defer f32 overflow behind row errors.
        let mut first_error = None;
        let mut narrowing: Option<(usize, f64)> = None;
        let mut retained = 0_usize;
        for result in state.results.as_mut_slice() {
            if let Some(Ok(partition)) = result {
                retained = retained
                    .checked_add(partition.outcome.candidates.len())
                    .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
            } else {
                match result.take() {
                    Some(Err(super::ExactScanError::Query(error))) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                    Some(Err(super::ExactScanError::Narrowing { row_id, score })) => {
                        if narrowing.is_none_or(|(row, previous)| {
                            score.total_cmp(&previous).is_gt()
                                || (score.total_cmp(&previous).is_eq() && row_id < row)
                        }) {
                            narrowing = Some((row_id, score));
                        }
                    }
                    None => {
                        if first_error.is_none() {
                            first_error = Some(QueryError::Store(StoreError::Synchronization {
                                component: "exact query result slot",
                            }));
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        if let Some((row_id, _)) = narrowing {
            return Err(QueryError::Scan(ScanError::NonFiniteScore { row_id }));
        }
        let mut merged = crate::scan::topk::ExactTopK::try_new(k, retained, &mut memory.candidates)
            .map_err(QueryError::Store)?;
        let mut dims_touched = 0_u64;
        let mut bytes_read = 0_u64;
        let mut worst_score: Option<f32> = None;
        let mut worker_ids = Vec::new();
        memory
            .worker_ids
            .set(state.results.len() * std::mem::size_of::<std::thread::ThreadId>())
            .map_err(QueryError::Store)?;
        worker_ids
            .try_reserve_exact(state.results.len())
            .map_err(|_| {
                QueryError::Store(StoreError::AllocationFailed {
                    needed: memory.worker_ids.bytes(),
                    component: "exact worker IDs",
                })
            })?;
        for result in state.results.as_mut_slice() {
            let partition = result
                .take()
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?
                .map_err(super::ExactScanError::into_query)?;
            dims_touched = dims_touched
                .checked_add(partition.outcome.stats.dims_touched)
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
            bytes_read = bytes_read
                .checked_add(partition.outcome.stats.bytes_read)
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
            if let Some(score) = partition.outcome.worst_score
                && worst_score.is_none_or(|worst| score.total_cmp(&worst).is_lt())
            {
                worst_score = Some(score);
            }
            for id in partition.outcome.stats.worker_thread_ids {
                if !worker_ids.contains(&id) {
                    worker_ids.push(id);
                }
            }
            for candidate in partition.outcome.candidates {
                merged
                    .try_push(candidate, &mut memory.candidates)
                    .map_err(QueryError::Store)?;
            }
        }
        let (candidates, _) = merged
            .try_into_sorted_with_ties(&mut memory.candidates)
            .map_err(QueryError::Store)?;
        Ok(ScanOutcome {
            candidates,
            worst_score,
            stats: ScanStats {
                dims_touched,
                bytes_read,
                threads_used: worker_ids.len(),
                worker_thread_ids: worker_ids,
            },
        })
    }
}

impl Drop for ExactExecution<'_> {
    fn drop(&mut self) {
        // This is also the unwind/poison path. No admitted worker may retain
        // an erased pointer when its stable arena or borrowed snapshot drops.
        let mut state = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.remaining > 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
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
