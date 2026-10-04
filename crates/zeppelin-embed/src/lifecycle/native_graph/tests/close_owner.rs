use super::super::{NativeGraphError, NativeReadLease};
use super::tempfile;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{OpenOptions, Store, StoreError};
use crate::property_graph::query::QueryError;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
enum CloseMode {
    Drain,
    BestEffort,
}

enum ProbeMessage {
    Progress(&'static str),
    Finished(Result<(), String>),
}

fn native_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
        .with_reader_drain_timeout(Duration::ZERO)
}

fn require(condition: bool, message: impl Into<String>) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn admit_three_with_leading_hole(
    store: &Store,
) -> Result<(NativeReadLease, NativeReadLease), String> {
    let gap = store
        .admit_native_read()
        .map_err(|error| format!("admit gap: {error}"))?;
    let target = store
        .admit_native_read()
        .map_err(|error| format!("admit target: {error}"))?;
    let survivor = store
        .admit_native_read()
        .map_err(|error| format!("admit survivor: {error}"))?;
    drop(gap);
    Ok((target, survivor))
}

fn arm_close_owner_hook(
    store: &Store,
    target: &NativeReadLease,
) -> Result<(Arc<Barrier>, Arc<Barrier>), String> {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let mut state = store
        .native_graph
        .state
        .lock()
        .map_err(|_| "lock publication to arm close-owner hook".to_owned())?;
    state.close_owner_hook = Some((target.token(), Arc::clone(&entered), Arc::clone(&release)));
    Ok((entered, release))
}

fn wait_for_survivor_cancellation(
    publication: &Arc<super::super::NativeGraphPublication>,
    survivor: &NativeReadLease,
) -> Result<(), String> {
    let mut state = publication
        .state
        .lock()
        .map_err(|_| "lock publication while waiting for survivor cancellation".to_owned())?;
    while survivor.check_active().is_ok() {
        state = publication
            .changed
            .wait(state)
            .map_err(|_| "wait for survivor cancellation".to_owned())?;
    }
    Ok(())
}

fn assert_last_owner_handoff(
    target: NativeReadLease,
    target_owner: &std::sync::Weak<super::super::NativeReadOwner>,
    progress: &Sender<ProbeMessage>,
) -> Result<(), String> {
    require(
        Arc::strong_count(&target.owner) == 2,
        format!(
            "target strong count after close upgrade was {}, expected 2",
            Arc::strong_count(&target.owner)
        ),
    )?;
    require(
        matches!(target.check_active(), Err(QueryError::ReadCancelled)),
        "target was not cancelled at close-owner hook",
    )?;
    drop(target);
    require(
        target_owner.strong_count() == 1,
        format!(
            "target strong count after external drop was {}, expected 1",
            target_owner.strong_count()
        ),
    )?;
    progress
        .send(ProbeMessage::Progress(
            "reached after-upgrade two-to-one last-owner handoff",
        ))
        .map_err(|_| "report last-owner handoff".to_owned())
}

fn run_close_probe(mode: CloseMode, progress: Sender<ProbeMessage>) -> Result<(), String> {
    let parent = tempfile::tempdir().map_err(|error| format!("temporary parent: {error}"))?;
    let store = Store::create_native_graph(parent.path().join("native"), native_options(), None)
        .map_err(|error| format!("create native graph: {error}"))?;
    let (target, survivor) = admit_three_with_leading_hole(&store)?;
    let target_owner = Arc::downgrade(&target.owner);
    let publication = Arc::clone(&store.native_graph);
    let (entered, release) = arm_close_owner_hook(&store, &target)?;
    progress
        .send(ProbeMessage::Progress("fixture ready and close hook armed"))
        .map_err(|_| "report fixture ready".to_owned())?;

    match mode {
        CloseMode::Drain => {
            let store = Arc::new(store);
            let closing_store = Arc::clone(&store);
            let (done_tx, done_rx) = mpsc::channel();
            let closer = std::thread::spawn(move || {
                let result = closing_store.close();
                let _ = done_tx.send(result);
            });
            entered.wait();
            assert_last_owner_handoff(target, &target_owner, &progress)?;
            release.wait();
            wait_for_survivor_cancellation(&publication, &survivor)?;
            require(
                matches!(done_rx.try_recv(), Err(TryRecvError::Empty)),
                "normal close returned while survivor remained live",
            )?;
            require(
                matches!(
                    store.admit_native_read(),
                    Err(NativeGraphError::Store(
                        StoreError::Closing | StoreError::Closed
                    ))
                ),
                "normal close admitted a reader after closing began",
            )?;
            drop(survivor);
            done_rx
                .recv()
                .map_err(|_| "normal close result channel disconnected".to_owned())?
                .map_err(|error| format!("normal close failed: {error}"))?;
            closer
                .join()
                .map_err(|_| "normal close thread panicked".to_owned())?;
            require(
                target_owner.upgrade().is_none(),
                "target owner survived normal close",
            )?;
            let state = publication
                .state
                .lock()
                .map_err(|_| "lock publication after normal close".to_owned())?;
            require(
                state.leases.iter().all(Option::is_none),
                "normal close left a registered lease",
            )?;
            require(state.current.is_none(), "normal close left current graph")?;
        }
        CloseMode::BestEffort => {
            let (done_tx, done_rx) = mpsc::channel();
            let closer = std::thread::spawn(move || {
                drop(store);
                let _ = done_tx.send(());
            });
            entered.wait();
            assert_last_owner_handoff(target, &target_owner, &progress)?;
            release.wait();
            done_rx
                .recv()
                .map_err(|_| "best-effort teardown channel disconnected".to_owned())?;
            closer
                .join()
                .map_err(|_| "best-effort close thread panicked".to_owned())?;
            require(
                matches!(survivor.check_active(), Err(QueryError::ReadCancelled)),
                "best-effort close did not cancel survivor",
            )?;
            {
                let state = publication
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                require(
                    state.current.is_none(),
                    "best-effort close left current graph",
                )?;
                require(
                    state.leases.iter().flatten().any(|entry| {
                        entry.token == survivor.token() && entry.owner.strong_count() != 0
                    }),
                    "best-effort close did not retain survivor registration",
                )?;
            }
            drop(survivor);
            require(
                target_owner.upgrade().is_none(),
                "target owner survived best-effort close",
            )?;
            let state = publication
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            require(
                state.leases.iter().all(Option::is_none),
                "best-effort release left a registered lease",
            )?;
        }
    }
    Ok(())
}

fn run_poison_probe(_: Sender<ProbeMessage>) -> Result<(), String> {
    let parent = tempfile::tempdir().map_err(|error| format!("temporary parent: {error}"))?;
    let normal = Store::create_native_graph(parent.path().join("normal"), native_options(), None)
        .map_err(|error| format!("create normal native graph: {error}"))?;
    let poison_publication = Arc::clone(&normal.native_graph);
    require(
        std::thread::spawn(move || {
            let _state = poison_publication.state.lock().unwrap();
            panic!("poison normal native graph publication");
        })
        .join()
        .is_err(),
        "normal poison thread did not panic",
    )?;
    require(
        matches!(
            normal.close(),
            Err(StoreError::Synchronization {
                component: "native graph publication"
            })
        ),
        "normal close did not preserve publication poison error",
    )?;

    let best_effort =
        Store::create_native_graph(parent.path().join("best-effort"), native_options(), None)
            .map_err(|error| format!("create best-effort native graph: {error}"))?;
    let first = best_effort
        .admit_native_read()
        .map_err(|error| format!("admit first poison reader: {error}"))?;
    let second = best_effort
        .admit_native_read()
        .map_err(|error| format!("admit second poison reader: {error}"))?;
    let publication = Arc::clone(&best_effort.native_graph);
    let poison_publication = Arc::clone(&publication);
    require(
        std::thread::spawn(move || {
            let _state = poison_publication.state.lock().unwrap();
            panic!("poison best-effort native graph publication");
        })
        .join()
        .is_err(),
        "best-effort poison thread did not panic",
    )?;
    drop(best_effort);
    require(
        matches!(first.check_active(), Err(QueryError::ReadCancelled))
            && matches!(second.check_active(), Err(QueryError::ReadCancelled)),
        "best-effort poisoned close did not cancel both readers",
    )?;
    {
        let state = publication
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        require(
            state.current.is_none(),
            "best-effort poisoned close left current graph",
        )?;
        require(
            state.leases.iter().flatten().count() == 2,
            "best-effort poisoned close drained external readers",
        )?;
    }
    drop(first);
    drop(second);
    let state = publication
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    require(
        state.leases.iter().all(Option::is_none),
        "poison control left registered readers",
    )
}

fn run_bounded(probe: impl FnOnce(Sender<ProbeMessage>) -> Result<(), String> + Send + 'static) {
    let (message_tx, message_rx): (Sender<ProbeMessage>, Receiver<ProbeMessage>) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = probe(message_tx.clone());
        let _ = message_tx.send(ProbeMessage::Finished(result));
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut latest = "worker started";
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match message_rx.recv_timeout(remaining) {
            Ok(ProbeMessage::Progress(progress)) => latest = progress,
            Ok(ProbeMessage::Finished(Ok(()))) => {
                worker.join().expect("close-owner worker");
                return;
            }
            Ok(ProbeMessage::Finished(Err(error))) => {
                worker.join().expect("close-owner worker");
                panic!("close-owner probe failed after {latest}: {error}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(worker);
                panic!("close-owner probe timed out after {latest}");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(worker);
                panic!("close-owner probe disconnected after {latest}");
            }
        }
    }
}

#[cfg_attr(test, test)]
pub(super) fn native_close_drain_releases_last_temporary_owner() {
    run_bounded(|progress| run_close_probe(CloseMode::Drain, progress));
}

#[cfg_attr(test, test)]
pub(super) fn native_close_best_effort_releases_last_temporary_owner() {
    run_bounded(|progress| run_close_probe(CloseMode::BestEffort, progress));
}

#[cfg_attr(test, test)]
pub(super) fn native_close_owner_walk_preserves_poison_policy() {
    run_bounded(run_poison_probe);
}

fn ze201_drained_writer_states(operation: impl Fn(&Store) -> Result<(), NativeGraphError>) {
    let parent = tempfile::tempdir().expect("parent");
    let store = Store::create_native_graph(parent.path().join("native"), native_options(), None)
        .expect("store");
    // Exercise the same drainage as close, retaining the publication so an
    // earlier read cancellation cannot hide the writer admission under test.
    *store.state.lock().expect("state") = crate::lifecycle::StoreState::Closing;
    store.native_graph.drain_writer_for_close().expect("drain");
    assert!(matches!(
        operation(&store),
        Err(NativeGraphError::Store(StoreError::Closing))
    ));
    *store.state.lock().expect("state") = crate::lifecycle::StoreState::Closed;
    assert!(matches!(
        operation(&store),
        Err(NativeGraphError::Store(StoreError::Closed))
    ));
    *store.state.lock().expect("state") = crate::lifecycle::StoreState::Open;
    store.close().expect("cleanup");
}

#[test]
fn ze201_apply_native_graph_drained_writer_is_closed() {
    ze201_drained_writer_states(|store| {
        store
            .apply_native_graph(
                &[],
                &crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new()),
            )
            .map(|_| ())
    });
}

#[test]
fn ze201_checkpoint_native_graph_drained_writer_is_closed() {
    ze201_drained_writer_states(|store| {
        store.checkpoint_native_graph(&crate::lifecycle::QueryControl::Cancel(
            crate::lifecycle::CancelToken::new(),
        ))
    });
}

#[test]
fn ze201_maintenance_admission_drained_writer_is_closed() {
    ze201_drained_writer_states(|store| store.admit_native_graph_maintenance().map(|_| ()));
}

#[test]
fn ze201_open_store_missing_writer_is_internal() {
    let parent = tempfile::tempdir().expect("parent");
    let store = Store::create_native_graph(parent.path().join("native"), native_options(), None)
        .expect("store");
    store.native_graph.drain_writer_for_close().expect("drain");
    assert!(matches!(
        store.admit_native_graph_maintenance(),
        Err(NativeGraphError::WriterAbsent)
    ));
    store.close().expect("cleanup");
}
