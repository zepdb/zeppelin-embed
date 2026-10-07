//! ZE-53 S4: close, lifetime and release proofs through
//! `Store::execute_graph_query`.
//!
//! Every oracle here is independent of the result collector: the fixture's
//! own values, the statement's arithmetic, the store's lifecycle state, the
//! published generation after a reopen, and the store's exact accounting.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use super::super::{CompletedGraphResult, Outcome};
use super::entry::NoSearch;
use super::entry_probe::{Assign, Fixture, control, node_values, options, write_p};
use super::entry_tests::{FixedHits, lexical, search_nodes};
use super::error::GraphQueryErrorKind;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeGraphError;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreState};
use crate::property_graph::GraphGeneration;
use crate::property_graph::query::plan::SearchCallId;
use crate::property_graph::resources::GraphResources;
use crate::property_graph::wal::RequiredRef;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn directory(test: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zeppelin-ze53-s4-{test}-{}", std::process::id()))
}

fn open_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

/// The S3 fixture, closed and reopened on the real file system with `open`.
fn fixture(test: &str, open: OpenOptions) -> Fixture {
    let Fixture {
        directory,
        store,
        vfs,
        nodes,
        values,
    } = Fixture::create(directory(test), 0).expect("ze53 s4 fixture");
    store.close().unwrap();
    drop(store);
    let store = Store::open_native_graph(directory.join("native"), open, None).expect("reopen");
    Fixture {
        directory,
        vfs,
        store,
        nodes,
        values,
    }
}

/// Closes `fixture`, reopens it, and returns its generation and values.
fn reopen_and_read(fixture: Fixture) -> (u64, Vec<(u128, i64)>) {
    let Fixture {
        directory,
        store,
        vfs,
        nodes,
        values,
    } = fixture;
    store.close().unwrap();
    drop(store);
    let reopened = Fixture {
        store: Store::open_native_graph(directory.join("native"), open_options(), None)
            .expect("reopen after close"),
        directory,
        vfs,
        nodes,
        values,
    };
    let observed = (
        reopened.generation().unwrap(),
        node_values(&reopened.read().unwrap()).unwrap(),
    );
    reopened.remove().unwrap();
    observed
}

fn rows(fixture: &Fixture, delta: i64) -> Vec<(u128, i64)> {
    fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value + delta))
        .collect()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::yield_now();
    }
}

/// Everything a completed result can be observed by, as one comparable
/// value.
fn snapshot(result: &CompletedGraphResult) -> String {
    let pools = result.pools();
    let metadata = result.metadata();
    format!(
        "{:?}|{:?}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        metadata.generation,
        metadata.outcome,
        metadata.rows,
        pools.values,
        pools.bytes,
        pools.columns,
        pools.cells,
        pools.children,
        pools.names,
        pools.properties,
        pools.nodes,
        pools.relationships,
        pools.vectors,
        pools.reports,
        pools.receipts,
    )
}

// ---------------------------------------------------------------------------
// Close drain
// ---------------------------------------------------------------------------

/// `Store::close` that starts while a write statement is inside its writer
/// admission waits for that statement to stop before it releases anything.
/// The statement observes close at its next storage read and cannot commit
/// once close has begun: it is a definite `Closed` refusal, and a reopened
/// store holds the pre-statement state.
#[test]
fn ze53_s4_close_during_a_live_write_drains_it_before_release() {
    let fixture = fixture("close-write", open_options());
    let before = fixture.generation().unwrap();
    let expected = rows(&fixture, 0);
    let store = &fixture.store;
    let builds = Cell::new(0);
    let builder_finished = AtomicBool::new(false);
    let (outcome, close_after_builder) = std::thread::scope(|scope| {
        let mut closer = None;
        let outcome = store.execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |runtime, executor| {
                builds.set(builds.get() + 1);
                if builds.get() == 2 {
                    closer = Some(scope.spawn(|| {
                        let closed = store.close();
                        (closed, builder_finished.load(Ordering::SeqCst))
                    }));
                    wait_until("close has begun", || {
                        store.state().unwrap() == StoreState::Closing
                    });
                }
                let executed = write_p(runtime, executor, Assign::Increment);
                if builds.get() == 2 {
                    builder_finished.store(true, Ordering::SeqCst);
                }
                executed
            },
        );
        let (closed, finished) = closer.expect("close ran").join().unwrap();
        closed.expect("close succeeds after draining the statement");
        (outcome, finished)
    });
    assert_eq!(builds.get(), 2);
    assert!(
        close_after_builder,
        "close returned while the write statement was still running"
    );
    let error = match outcome {
        Ok(result) => panic!(
            "a statement committed after close began: {:?}",
            result.metadata().outcome
        ),
        Err(error) => error,
    };
    assert_eq!(error.kind(), GraphQueryErrorKind::Closed, "{error}");
    assert!(error.nothing_committed(), "{error}");
    assert_eq!(reopen_and_read(fixture), (before, expected));
}

/// Close that begins while a write statement is still being classified
/// under its read admission, and drains the writer before the statement
/// reaches it, refuses the statement as `Closed` with nothing committed,
/// never as corruption.
#[test]
fn ze53_s4_close_between_classification_and_the_writer_is_closed() {
    let fixture = fixture("close-classify", open_options());
    let before = fixture.generation().unwrap();
    let expected = rows(&fixture, 0);
    let store = &fixture.store;
    let builds = Cell::new(0);
    let outcome = std::thread::scope(|scope| {
        let mut closer = None;
        let outcome = store.execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |runtime, executor| {
                builds.set(builds.get() + 1);
                if builds.get() == 1 {
                    closer = Some(scope.spawn(|| store.close()));
                    wait_until("close has drained the writer", || {
                        store
                            .native_graph_writer_drained_for_test()
                            .expect("writer slot")
                    });
                }
                write_p(runtime, executor, Assign::Increment)
            },
        );
        closer.expect("close ran").join().unwrap().unwrap();
        outcome
    });
    assert_eq!(builds.get(), 1, "the writer admission never built");
    let error = match outcome {
        Ok(result) => panic!("committed after close: {:?}", result.metadata().outcome),
        Err(error) => error,
    };
    assert_eq!(error.kind(), GraphQueryErrorKind::Closed, "{error}");
    assert!(error.nothing_committed(), "{error}");
    assert_eq!(reopen_and_read(fixture), (before, expected));
}

// ---------------------------------------------------------------------------
// Lifetime
// ---------------------------------------------------------------------------

/// Read, write and post-relocation results obtained through the seam are
/// bit-for-bit unchanged by a record-relocating maintenance commit, by
/// `Store::close` and by a reopen, and remain fully readable after the
/// store that produced them is gone.
#[test]
fn ze53_s4_results_outlive_relocation_close_and_reopen() {
    let fixture = fixture("lifetime", open_options());
    fixture
        .store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..crate::property_graph::GraphMaintenancePolicy::default()
        })
        .unwrap();
    let before = fixture.generation().unwrap();
    let read = fixture.read().unwrap();
    let write = fixture
        .write(&control(), 16, Assign::Increment)
        .expect("write statement");
    assert_eq!(node_values(&read).unwrap(), rows(&fixture, 0));
    assert_eq!(node_values(&write).unwrap(), rows(&fixture, 1));
    assert_eq!(
        write.metadata().outcome,
        Outcome::Committed {
            changed: GraphGeneration::new(before + 1)
        }
    );
    let read_before = snapshot(&read);
    let write_before = snapshot(&write);

    let admission = fixture.store.admit_native_graph_maintenance().unwrap();
    let report = fixture
        .store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
        .expect("relocating maintenance");
    drop(admission);
    assert!(report.replaced_physical_refs > 0, "nothing was relocated");
    // Maintenance checkpoints the tail (+1) before publishing relocation (+1).
    assert_eq!(report.generation.get(), before + 3);
    assert_eq!(snapshot(&read), read_before);
    assert_eq!(snapshot(&write), write_before);

    let relocated = fixture.read().unwrap();
    assert_eq!(relocated.metadata().generation.get(), before + 3);
    assert_eq!(node_values(&relocated).unwrap(), rows(&fixture, 1));
    let relocated_before = snapshot(&relocated);

    let expected = rows(&fixture, 1);
    assert_eq!(reopen_and_read(fixture), (before + 3, expected.clone()));
    // The producing store is closed and its directory removed.
    assert_eq!(snapshot(&read), read_before);
    assert_eq!(snapshot(&write), write_before);
    assert_eq!(snapshot(&relocated), relocated_before);
    assert_eq!(node_values(&write).unwrap(), expected);
    assert_eq!(node_values(&relocated).unwrap(), expected);
}

// ---------------------------------------------------------------------------
// Paired release
// ---------------------------------------------------------------------------

/// The store-owned charges a statement may hold while it runs: every exact
/// reservation on the store's shared accounting owner (query memory, write
/// memory, storage memory, read-lease owners, native registries), and the
/// admitted-query count.
fn charged(store: &Store) -> (u64, u64) {
    let snapshot = store.snapshot.read().unwrap();
    let snapshot = snapshot.as_ref().unwrap();
    let manifest_backing = snapshot
        .graph_manifest
        .as_ref()
        .unwrap()
        .backing_bytes()
        .unwrap() as u64;
    let wal = store.wal_writer.lock().unwrap();
    let wal_bytes: u64 = wal
        .as_ref()
        .unwrap()
        .unabsorbed_records(snapshot.absorbed_through())
        .unwrap()
        .iter()
        .map(|record| record.encoded().unwrap().len() as u64)
        .sum();
    let audit = store.accounting.audit().unwrap();
    assert_eq!(
        audit.wal_bytes, wal_bytes,
        "retained WAL charge equals encoded records"
    );
    assert_eq!(audit.resident_owned_bytes, audit.component_sum());
    (
        GraphResources::from_store(store)
            .unwrap()
            .reserved_bytes()
            .unwrap()
            - wal_bytes
            - manifest_backing,
        store.active_queries.load(Ordering::SeqCst),
    )
}

/// Folds prepared-inventory bookkeeping through an actual consolidation.
/// Reclaim drain and retirement each retain another RequiredRef; neither folds
/// inventories. Allow those two phases, then require consolidation within the
/// three-phase cycle. Return the number of maintenance generations committed.
fn fold(store: &Store) -> u64 {
    for committed in 1..=3 {
        for attempt in 0..2 {
            let admission = store.admit_native_graph_maintenance().unwrap();
            match store.commit_native_graph_maintenance(&admission, &control()) {
                Ok(report) if report.replaced_physical_refs > 0 => return committed,
                Ok(_) => break,
                Err(NativeGraphError::StalePreparation) => {
                    assert!(attempt == 0, "maintenance stayed stale across two attempts");
                }
                Err(error) => panic!("maintenance failed: {error:?}"),
            }
        }
    }
    panic!("maintenance did not fold inventories within three phases")
}

/// Repeated read, search, failed-search, write and refused-write statements
/// through the seam leave the store's exact accounting where they found it.
/// A held result charges the store nothing: it is application-owned. A read,
/// a search or a refused statement leaves exactly the charge it found. A
/// Unified WAL record bytes and manifest backing are independently sized and
/// removed from the transient-owner comparison. A commit grows the remaining
/// charge by exactly one prepared-inventory `RequiredRef`, and once
/// maintenance folds that bookkeeping every round returns to one baseline,
/// so no query, lease, search, overlay or writer charge accumulates.
#[test]
fn ze53_s4_repeated_statements_return_accounting_to_baseline() {
    let fixture = fixture("release", open_options());
    fixture
        .store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..crate::property_graph::GraphMaintenancePolicy::default()
        })
        .unwrap();
    // One warm-up round so lazily built store state is not counted as a leak.
    drop(fixture.read().unwrap());
    drop(fixture.write(&control(), 16, Assign::Increment).unwrap());
    fold(&fixture.store);
    let baseline = charged(&fixture.store);
    let start = fixture.generation().unwrap();
    let mut adapter = FixedHits {
        hits: vec![fixture.nodes[2], fixture.nodes[0]],
        calls: 0,
        fail: false,
    };
    let mut maintenance_generations = 0;
    for round in 0..16_i64 {
        let read = fixture.read().unwrap();
        assert_eq!(
            charged(&fixture.store),
            baseline,
            "round {round}: held read"
        );
        assert_eq!(node_values(&read).unwrap(), rows(&fixture, 1 + round));
        drop(read);
        assert_eq!(
            charged(&fixture.store),
            baseline,
            "round {round}: dropped read"
        );

        adapter.fail = false;
        let search = fixture
            .store
            .execute_graph_query(&control(), &options(16), Some(&mut adapter), search_nodes)
            .expect("search statement");
        assert_eq!(
            charged(&fixture.store),
            baseline,
            "round {round}: held search"
        );
        assert_eq!(search.metadata().rows, 2);
        assert_eq!(
            search.pools().reports,
            &[lexical(SearchCallId(0), search.metadata().generation, 2)]
        );
        drop(search);
        assert_eq!(
            charged(&fixture.store),
            baseline,
            "round {round}: dropped search"
        );

        adapter.fail = true;
        let failed = fixture
            .store
            .execute_graph_query(&control(), &options(16), Some(&mut adapter), search_nodes)
            .map(|_| ())
            .expect_err("the adapter fails");
        assert_eq!(failed.kind(), GraphQueryErrorKind::Limit, "{failed}");
        assert!(failed.nothing_committed(), "{failed}");
        assert_eq!(
            charged(&fixture.store),
            baseline,
            "round {round}: failed search"
        );
        assert_eq!(adapter.calls, 2 * (round as usize + 1));

        let write = fixture.write(&control(), 16, Assign::Increment).unwrap();
        let committed = charged(&fixture.store);
        assert_eq!(
            committed,
            (
                baseline.0 + std::mem::size_of::<RequiredRef>() as u64,
                baseline.1
            ),
            "round {round}: one commit grows the charge by one RequiredRef"
        );
        assert_eq!(node_values(&write).unwrap(), rows(&fixture, 2 + round));
        drop(write);
        assert_eq!(
            charged(&fixture.store),
            committed,
            "round {round}: dropping a write result changed store accounting"
        );

        let refused = fixture.write(
            &control(),
            16,
            Assign::DivideAround(fixture.values[1] + 2 + round),
        );
        assert_eq!(
            refused.map(|_| ()).expect_err("division by zero").kind(),
            GraphQueryErrorKind::Expression
        );
        assert_eq!(
            charged(&fixture.store),
            committed,
            "round {round}: refused write"
        );

        maintenance_generations += fold(&fixture.store);
        assert_eq!(charged(&fixture.store), baseline, "round {round}: folded");
    }
    // Each write adds one generation. Each maintenance phase checkpoints (+1)
    // and publishes its maintenance mutation (+1).
    assert_eq!(
        fixture.generation().unwrap(),
        start + 16 + 2 * maintenance_generations
    );
    fixture.remove().unwrap();
}

#[test]
fn ze275_close_is_observed_at_a_write_runtime_checkpoint_without_storage_read() {
    super::entry_probe::close_probe(0, super::entry_probe::CloseSchedule::Runtime).unwrap();
}

#[test]
fn ze275_close_before_wal_append_refuses_the_write() {
    super::entry_probe::close_probe(0, super::entry_probe::CloseSchedule::BeforeAppend).unwrap();
}

#[test]
fn ze275_close_after_wal_append_reports_the_commit() {
    super::entry_probe::close_probe(0, super::entry_probe::CloseSchedule::AfterAppend).unwrap();
}
