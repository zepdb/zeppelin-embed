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
use super::error::GraphQueryErrorKind;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeGraphError;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreState};
use crate::property_graph::GraphGeneration;
use crate::property_graph::resources::GraphResources;
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
                    // The structured writer reaches the writer slot first; it
                    // finds the slot empty once close has drained it, and
                    // still reports that as `Invalid` (ZE-201 tracks that
                    // path; fixing it must replace this probe).
                    wait_until("close has drained the writer", || {
                        matches!(
                            store.apply_native_graph(&[], &control()),
                            Err(NativeGraphError::Invalid(_))
                        )
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
    assert_eq!(report.generation.get(), before + 2);
    assert_eq!(snapshot(&read), read_before);
    assert_eq!(snapshot(&write), write_before);

    let relocated = fixture.read().unwrap();
    assert_eq!(relocated.metadata().generation.get(), before + 2);
    assert_eq!(node_values(&relocated).unwrap(), rows(&fixture, 1));
    let relocated_before = snapshot(&relocated);

    let expected = rows(&fixture, 1);
    assert_eq!(reopen_and_read(fixture), (before + 2, expected.clone()));
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
    (
        GraphResources::from_store(store)
            .unwrap()
            .reserved_bytes()
            .unwrap(),
        store.active_queries.load(Ordering::SeqCst),
    )
}

/// Folds the writer's per-commit bookkeeping through one maintenance
/// generation. That bookkeeping grows by a fixed amount per committed
/// generation until a checkpoint, for the structured writer exactly as for
/// this seam (96 bytes per commit on this fixture, measured both ways).
fn fold(store: &Store) {
    let admission = store.admit_native_graph_maintenance().unwrap();
    store
        .commit_native_graph_maintenance(&admission, &control())
        .expect("maintenance");
}

/// Repeated read, write and refused-write statements through the seam leave
/// the store's exact accounting where they found it. A held result charges
/// the store nothing: it is application-owned. A read or a refused
/// statement leaves exactly the charge it found. Once maintenance folds the
/// writer's per-commit bookkeeping, every round returns to one baseline, so
/// no query, lease, overlay or writer charge accumulates.
#[test]
fn ze53_s4_repeated_statements_return_accounting_to_baseline() {
    let fixture = fixture("release", open_options());
    // One warm-up round so lazily built store state is not counted as a leak.
    drop(fixture.read().unwrap());
    drop(fixture.write(&control(), 16, Assign::Increment).unwrap());
    fold(&fixture.store);
    let baseline = charged(&fixture.store);
    let start = fixture.generation().unwrap();
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

        let write = fixture.write(&control(), 16, Assign::Increment).unwrap();
        let committed = charged(&fixture.store);
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

        fold(&fixture.store);
        assert_eq!(charged(&fixture.store), baseline, "round {round}: folded");
    }
    // Each round publishes one write and one maintenance generation.
    assert_eq!(fixture.generation().unwrap(), start + 32);
    fixture.remove().unwrap();
}
