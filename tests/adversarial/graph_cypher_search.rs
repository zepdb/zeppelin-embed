//! ZE-58 real Cypher retrieval, scheduled execution deadline, and release.
use super::coverage::CoverageRegistry;
use super::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledQueryClock, ScheduledVfs,
};
use rand::RngCore;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, ManualMonotonicClock, MonotonicClock, OpenOptions, QueryControl,
};
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryErrorKind, GraphQueryOptions, Value,
};
use zeppelin_embed::property_graph::query::runtime::WorkKind;
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, EntityKind, GraphRevision, GraphStore,
};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_cypher::{CompileLimits, StatementError, execute};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.cypher-search.invocations",
    "property-graph.cypher-search.empty-input",
    "property-graph.cypher-search.projected-report",
    "property-graph.cypher-search.deadline.retrieval-fire",
    "property-graph.cypher-search.cancel.admission-fire",
    "property-graph.cypher-search.close.retrieval-fire",
    "property-graph.cypher-search.resource.execution-fire",
    "property-graph.cypher-search.release",
    "property-graph.cypher-search.oracle.can-fire",
    "property-graph.cypher-search.same-seed-control",
];
struct CountClock {
    inner: ScheduledQueryClock<StdVfs>,
    calls: AtomicUsize,
}
impl MonotonicClock for CountClock {
    fn now(&self) -> Instant {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.now()
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Observation {
    rows: u32,
    calls: u64,
    reports: Vec<(u32, u64, u64)>,
    scores: Vec<u64>,
}
fn observe(r: &CompletedGraphResult) -> Result<Observation, String> {
    let mut scores = Vec::new();
    for row in 0..r.metadata().rows as usize {
        match r.cell(row, 0) {
            Some(Value::F64(bits)) => scores.push(*bits),
            _ => return Err("search score type".into()),
        }
    }
    Ok(Observation {
        rows: r.metadata().rows,
        calls: r.metadata().counters.get(WorkKind::SearchInvocations),
        reports: r
            .pools()
            .reports
            .iter()
            .map(|r| (r.call.0, r.generation.get(), r.candidate_count))
            .collect(),
        scores,
    })
}
const QUERY: &str = "CALL ze.text_search('amber',64) YIELD node,score RETURN score";
#[allow(clippy::result_large_err)]
fn run(
    store: &GraphStore,
    control: &QueryControl,
    options: &GraphQueryOptions,
    text: &str,
) -> Result<CompletedGraphResult, StatementError> {
    execute(
        store.statement_store(),
        control,
        options,
        text,
        &[],
        CompileLimits::default(),
    )
}
fn trial(
    store: &GraphStore,
    seed: u64,
    fire: Option<usize>,
) -> Result<(Result<CompletedGraphResult, StatementError>, usize, usize), String> {
    let manual = Arc::new(ManualMonotonicClock::new());
    let schedule = fire
        .map(|nth_match| {
            FaultSchedule::single(FaultEvent {
                id: format!("ZE58-{seed}-{nth_match}"),
                op_index: 0,
                layer: Layer::Clock,
                site: FaultSite::Clock,
                mode: FaultMode::ClockJump { seconds: 30 },
                nth_match,
                expected_matches: None,
                deadline_budget_seconds: Some(1),
                path_contains: Some("clock".into()),
                fired: false,
                fire_count: 0,
                path: None,
            })
        })
        .unwrap_or_default();
    let vfs = Arc::new(ScheduledVfs::new_with_clock(
        StdVfs,
        schedule,
        Arc::clone(&manual),
    ));
    vfs.set_operation(0);
    let clock = Arc::new(CountClock {
        inner: ScheduledQueryClock::new(manual, Arc::clone(&vfs)),
        calls: AtomicUsize::new(0),
    });
    let control = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .map_err(|e| e.to_string())?,
    );
    clock.calls.store(0, Ordering::SeqCst);
    clock.inner.arm_query();
    let result = run(store, &control, &GraphQueryOptions::default(), QUERY);
    clock.inner.finish_query()?;
    Ok((
        result,
        clock.calls.load(Ordering::SeqCst),
        vfs.events().iter().map(|e| e.fire_count).sum(),
    ))
}
fn check(actual: &Observation, score: f64) -> Result<(), String> {
    if actual.rows != 64
        || actual.calls != 1
        || actual.reports.len() != 1
        || actual.reports[0].0 != 0
        || actual.reports[0].2 != 64
        || actual
            .scores
            .iter()
            .any(|bits| (f64::from_bits(*bits) - score).abs() > 1e-6)
    {
        return Err(format!("independent search oracle mismatch: {actual:?}"));
    }
    Ok(())
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Arc::new(
        GraphStore::create(
            directory.path().join("graph"),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
            None,
        )
        .map_err(|e| e.to_string())?,
    );
    let mut rng = super::test_support::seeded_rng("ze58-cypher-search", seed);
    let term = if rng.next_u32() & 1 == 0 {
        "amber birch"
    } else {
        "amber cedar"
    };
    let contents =
        CanonicalContents::node(&mut [], &mut [], Some(term), None).map_err(|e| e.to_string())?;
    let keys: Vec<_> = (0..64).map(|i| i.to_string()).collect();
    let writes: Vec<_> = keys
        .iter()
        .map(|key| {
            Ok(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze58", key)
                    .map_err(|e| e.to_string())?,
                revision: GraphRevision::new(1).map_err(|e| e.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&contents)),
            })
        })
        .collect::<Result<_, String>>()?;
    store
        .apply_batch(&writes, &QueryControl::Cancel(CancelToken::new()))
        .map_err(|e| e.to_string())?;
    let shared = GraphResources::from_store(store.statement_store()).map_err(|e| e.to_string())?;
    let (clean, polls, fires) = trial(&store, seed, None)?;
    if fires != 0 {
        return Err("clean schedule fired".into());
    }
    let clean = clean.map_err(|e| e.to_string())?;
    let observation = observe(&clean)?;
    let generation = clean.metadata().generation;
    let postings = clean.metadata().counters.get(WorkKind::LexicalPostings);
    // Every text has tf=1, df=N=64, len=avgdl=2: BM25 equals IDF.
    let expected = (1.0_f64 + 0.5 / 64.5).ln();
    check(&observation, expected)?;
    drop(clean);
    let baseline = shared.reserved_bytes().map_err(|e| e.to_string())?;
    // Locate the first posting checkpoint from measured execution counters.
    // Bisection avoids a coarse schedule skipping the narrow retrieval phase.
    let mut low = 1;
    let mut high = polls;
    while low < high {
        let nth = low + (high - low) / 2;
        let (result, _, _) = trial(&store, seed, Some(nth))?;
        let after_posting = match result {
            Err(StatementError::Compile(_)) => false,
            Err(StatementError::Query(e)) => e
                .counters()
                .is_some_and(|c| c.get(WorkKind::LexicalPostings) > 0),
            Ok(_) => true,
        };
        if after_posting {
            high = nth;
        } else {
            low = nth + 1;
        }
        if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
            return Err("deadline leaked temporary ownership".into());
        }
    }
    // Move inside the measured posting phase, away from the first-posting
    // boundary whose few admission checkpoints depend on mapping reuse.
    let nth = low + 64;
    let (result, _, fires) = trial(&store, seed, Some(nth))?;
    let fired_postings = match &result {
        Err(StatementError::Query(e)) => {
            e.counters().map_or(0, |c| c.get(WorkKind::LexicalPostings))
        }
        _ => 0,
    };
    match result {
        Err(StatementError::Query(e))
            if e.kind() == GraphQueryErrorKind::Timeout
                && e.nothing_committed()
                && fires == 1
                && e.counters().is_some_and(|c| {
                    c.get(WorkKind::SearchInvocations) == 1
                        && c.get(WorkKind::LexicalPostings) > 0
                        && c.get(WorkKind::LexicalPostings) < postings
                }) => {}
        other => {
            return Err(format!(
                "no retrieval deadline can-fire at {nth}/{polls}: {}",
                match other {
                    Err(e) => e.to_string(),
                    Ok(_) => "returned result".into(),
                }
            ));
        }
    }
    if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("fired retrieval leaked".into());
    }
    let (control, _, fires) = trial(&store, seed, None)?;
    let control = control.map_err(|e| e.to_string())?;
    if fires != 0 || observe(&control)? != observation {
        return Err("same-seed clean mismatch".into());
    }
    drop(control);
    let token = CancelToken::new();
    token.cancel();
    match run(
        &store,
        &QueryControl::Cancel(token),
        &GraphQueryOptions::default(),
        QUERY,
    ) {
        Err(StatementError::Query(e))
            if e.kind() == GraphQueryErrorKind::Cancelled && e.nothing_committed() => {}
        _ => return Err("cancel admission did not fire".into()),
    }
    let options = GraphQueryOptions::default()
        .with_result_row_limit(1)
        .map_err(|e| e.to_string())?;
    match run(
        &store,
        &QueryControl::Cancel(CancelToken::new()),
        &options,
        QUERY,
    ) {
        Err(StatementError::Query(e))
            if e.kind() == GraphQueryErrorKind::Limit
                && e.nothing_committed()
                && e.counters()
                    .is_some_and(|c| c.get(WorkKind::SearchInvocations) == 1) => {}
        _ => return Err("execution resource refusal did not fire".into()),
    }
    for (q, rows, calls) in [
        (
            "CALL ze.text_search('amber',2) YIELD node AS a CALL ze.text_search('amber',3) YIELD node AS b RETURN a,b",
            6,
            2,
        ),
        (
            "MATCH (n:Absent) CALL ze.text_search('amber',2) YIELD node RETURN node",
            0,
            1,
        ),
        (
            "CALL ze.text_search('amber',2) YIELD node RETURN count(*)",
            1,
            1,
        ),
        (
            "CALL ze.text_search('amber',2) YIELD node RETURN node LIMIT 0",
            0,
            1,
        ),
    ] {
        let r = run(
            &store,
            &QueryControl::Cancel(CancelToken::new()),
            &Default::default(),
            q,
        )
        .map_err(|e| e.to_string())?;
        if r.metadata().rows != rows
            || r.metadata().generation != generation
            || r.pools().reports.len() != calls
            || r.metadata().counters.get(WorkKind::SearchInvocations) != calls as u64
        {
            return Err(format!("eager report mismatch {q}"));
        }
    }
    let mut missing_report = observation.clone();
    missing_report.reports.clear();
    if check(&missing_report, expected).is_ok() {
        return Err("oracle accepted a dropped report".into());
    }
    let mut perturbed = observation;
    perturbed.scores[0] = 0;
    if check(&perturbed, expected).is_ok() {
        return Err("oracle accepted perturbed score".into());
    }
    if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("search temporaries retained".into());
    }
    println!(
        "ZE58 seed={seed} polls={polls} retrieval_deadline_nth={nth} postings={fired_postings}/{postings}; score/report perturbations rejected; reservations={baseline}"
    );
    // Reuse the measured retrieval checkpoint to close-cancel an admitted read.
    // The clock gates an ordinary close thread; no engine hook is introduced.
    struct CloseClock {
        base: Instant,
        polls: AtomicUsize,
        nth: usize,
        start_sentinel: std::sync::mpsc::Sender<()>,
        start_close: std::sync::mpsc::Sender<()>,
        ready: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        cancelled: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl MonotonicClock for CloseClock {
        fn now(&self) -> Instant {
            if self.polls.fetch_add(1, Ordering::SeqCst) == self.nth {
                self.start_sentinel
                    .send(())
                    .expect("start later read sentinel");
                self.ready
                    .lock()
                    .expect("sentinel ready mutex")
                    .recv_timeout(Duration::from_secs(2))
                    .expect("later read admitted");
                self.start_close.send(()).expect("start ordinary close");
                self.cancelled
                    .lock()
                    .expect("sentinel cancellation mutex")
                    .recv_timeout(Duration::from_secs(2))
                    .expect("close cancelled later read");
            }
            self.base
        }
    }
    let (start_sentinel, begin) = std::sync::mpsc::channel();
    let (start_close, receive) = std::sync::mpsc::channel();
    let (ready_send, ready) = std::sync::mpsc::channel();
    let (cancel_send, cancelled) = std::sync::mpsc::channel();
    let clock = Arc::new(CloseClock {
        base: Instant::now(),
        polls: AtomicUsize::new(0),
        nth,
        start_sentinel,
        start_close,
        ready: std::sync::Mutex::new(ready),
        cancelled: std::sync::Mutex::new(cancelled),
    });
    let control = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .map_err(|e| e.to_string())?,
    );
    clock.polls.store(0, Ordering::SeqCst);
    let result = std::thread::scope(|scope| {
        let sentinel_store = Arc::clone(&store);
        let sentinel=scope.spawn(move || {
            begin.recv_timeout(Duration::from_secs(2)).expect("retrieval checkpoint fired");
            // This lease is admitted after the search lease. Close cancels
            // leases in slot order; its cancellation proves the earlier search
            // lease was cancelled before its clock gate resumes.
            let mut saw_close=false;
            let result=sentinel_store.statement_store().execute_graph_statement(&QueryControl::Cancel(CancelToken::new()),&Default::default(),|runtime,_| {
                ready_send.send(()).expect("sentinel admitted");
                loop {
                    if let Err(error)=runtime.checkpoint() {
                        cancel_send.send(()).expect("sentinel cancelled");
                        saw_close=matches!(error,zeppelin_embed::property_graph::query::runtime::RuntimeError::Value(zeppelin_embed::property_graph::query::QueryError::ReadCancelled));
                        return Err(zeppelin_embed::property_graph::query::completed::GraphQueryError::builder_rejected());
                    }
                    std::thread::yield_now();
                }
            });
            match result {Err(_) if saw_close=>Ok(()),_=>Err("sentinel did not observe close cancellation".to_owned())}
        });
        let close_store = Arc::clone(&store);
        let closer = scope.spawn(move || {
            receive
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| e.to_string())?;
            close_store.close().map_err(|e| e.to_string())
        });
        let result = run(&store, &control, &Default::default(), QUERY);
        sentinel
            .join()
            .map_err(|_| "sentinel thread panicked".to_owned())??;
        closer
            .join()
            .map_err(|_| "close thread panicked".to_owned())??;
        Ok::<_, String>(result)
    })?;
    match result {
        Err(StatementError::Query(e))
            if e.kind() == GraphQueryErrorKind::Closed
                && e.nothing_committed()
                && e.counters().is_some_and(|c| {
                    c.get(WorkKind::SearchInvocations) == 1
                        && c.get(WorkKind::LexicalPostings) > 0
                        && c.get(WorkKind::LexicalPostings) < postings
                }) => {}
        other => {
            return Err(format!(
                "close cancellation did not reach retrieval: {}",
                match other {
                    Err(e) => e.to_string(),
                    Ok(_) => "returned rows".into(),
                }
            ));
        }
    }
    println!(
        "ZE58 seed={seed} close cancelled retrieval at nth={nth}; reopened values/reports unchanged"
    );
    drop(clock);
    let reopened = GraphStore::open(
        directory.path().join("graph"),
        OpenOptions::new()
            .with_max_resident_bytes(256 * 1024 * 1024)
            .with_reader_drain_timeout(Duration::ZERO),
        None,
    )
    .map_err(|e| e.to_string())?;
    let again = run(
        &reopened,
        &QueryControl::Cancel(CancelToken::new()),
        &Default::default(),
        QUERY,
    )
    .map_err(|e| e.to_string())?;
    let mut expected_observation = observe(&again)?;
    check(&expected_observation, expected)?;
    expected_observation.scores[0] = 0;
    if expected_observation != perturbed {
        return Err("close cancellation mutated durable observations".into());
    }
    drop(again);
    reopened.close().map_err(|e| e.to_string())?;

    for key in REQUIRED_COVERAGE {
        coverage.hit(*key);
    }
    Ok(())
}
