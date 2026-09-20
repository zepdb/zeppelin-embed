#![allow(clippy::expect_used, clippy::panic)]

use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};
use zeppelin_embed::property_graph::resources::{
    AllocationIntervalError, GraphResources, MAX_GRAPH_RESIDENT_BYTES,
};

fn open_graph_store() -> (tempfile::TempDir, Store, GraphResources) {
    let root = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(MAX_GRAPH_RESIDENT_BYTES),
    )
    .expect("store");
    let resources = GraphResources::from_store(&store).expect("shared accounting");
    (root, store, resources)
}

#[test]
fn graph_memory_interval_ignores_old_higher_peak() {
    let (_root, store, resources) = open_graph_store();
    let baseline = resources.reserved_bytes().expect("baseline");

    let old = resources.reserve(4_096).expect("old larger reservation");
    drop(old);
    assert_eq!(resources.reserved_bytes().expect("old released"), baseline);
    assert_eq!(
        resources.peak_reserved_bytes().expect("old lifetime peak"),
        baseline + 4_096
    );

    let interval = resources
        .begin_allocation_interval()
        .expect("fresh interval");
    let later = resources.reserve(192).expect("later smaller reservation");
    drop(later);
    let observed = interval.finish().expect("finish interval");
    assert_eq!(observed.start_reserved_bytes, baseline);
    assert_eq!(observed.current_reserved_bytes, baseline);
    assert_eq!(observed.peak_reserved_bytes, baseline + 192);
    assert!(resources.peak_reserved_bytes().expect("lifetime peak") >= baseline + 4_096);
    store.close().expect("close");
}

#[test]
fn graph_memory_interval_tracks_growth_and_excludes_failed_reservations() {
    let (_root, store, resources) = open_graph_store();
    let baseline = resources.reserved_bytes().expect("baseline");
    let interval = resources
        .begin_allocation_interval()
        .expect("fresh interval");
    let mut first = resources.reserve(96).expect("first reservation");
    let second = resources.reserve(64).expect("second reservation");
    first.resize(128).expect("grow first reservation");
    assert_eq!(
        interval.snapshot().expect("live overlap"),
        zeppelin_embed::property_graph::resources::AllocationIntervalSnapshot {
            start_reserved_bytes: baseline,
            current_reserved_bytes: baseline + 192,
            peak_reserved_bytes: baseline + 192,
        }
    );
    assert!(matches!(
        first.resize(MAX_GRAPH_RESIDENT_BYTES as usize),
        Err(StoreError::BudgetExceeded { .. })
    ));
    assert_eq!(first.bytes(), 128);
    assert_eq!(
        interval.snapshot().expect("failed grow excluded"),
        zeppelin_embed::property_graph::resources::AllocationIntervalSnapshot {
            start_reserved_bytes: baseline,
            current_reserved_bytes: baseline + 192,
            peak_reserved_bytes: baseline + 192,
        }
    );
    drop(second);
    drop(first);
    let finished = interval.finish().expect("finish interval");
    assert_eq!(finished.current_reserved_bytes, baseline);
    assert_eq!(finished.peak_reserved_bytes, baseline + 192);
    store.close().expect("close");
}

#[test]
fn graph_memory_interval_records_concurrent_overlap_without_sampling() {
    use std::sync::{Arc, Barrier};

    let (_root, store, resources) = open_graph_store();
    let baseline = resources.reserved_bytes().expect("baseline");
    let interval = resources
        .begin_allocation_interval()
        .expect("fresh interval");
    let held = Arc::new(Barrier::new(3));
    let release = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let first_resources = resources.clone();
        let first_held = Arc::clone(&held);
        let first_release = Arc::clone(&release);
        let first = scope.spawn(move || {
            let reservation = first_resources.reserve(128).expect("first worker");
            first_held.wait();
            first_release.wait();
            drop(reservation);
        });
        let second_resources = resources.clone();
        let second_held = Arc::clone(&held);
        let second_release = Arc::clone(&release);
        let second = scope.spawn(move || {
            let reservation = second_resources.reserve(64).expect("second worker");
            second_held.wait();
            second_release.wait();
            drop(reservation);
        });
        held.wait();
        release.wait();
        first.join().expect("first worker joined");
        second.join().expect("second worker joined");
    });
    let finished = interval.finish().expect("finish interval");
    assert_eq!(finished.start_reserved_bytes, baseline);
    assert_eq!(finished.current_reserved_bytes, baseline);
    assert_eq!(finished.peak_reserved_bytes, baseline + 192);
    store.close().expect("close");
}

#[test]
fn graph_memory_interval_counts_nested_capacity_once() {
    use zeppelin_embed::property_graph::query::resources::{QueryArena, QueryMemory};
    use zeppelin_embed::property_graph::staging::{StageError, WriteLimits, WriteMemory};

    let (_root, store, resources) = open_graph_store();
    let baseline = resources.reserved_bytes().expect("baseline");
    let interval = resources
        .begin_allocation_interval()
        .expect("fresh interval");
    let memory = QueryMemory::new(&resources, 4_096).expect("query memory");
    let arena = QueryArena::<u64>::new(&memory, 16).expect("query arena");
    assert_eq!(arena.capacity(), 16);
    let writer = WriteMemory::new(&resources, WriteLimits::default()).expect("writer memory");
    let mut control = |_| Ok::<(), StageError>(());
    let writer_reservation = writer
        .reserve(64, &mut control)
        .expect("writer reservation");
    let expected_delta = std::mem::size_of::<QueryMemory<'_>>()
        + std::mem::size_of::<QueryArena<'_, '_, u64>>()
        + 16 * std::mem::size_of::<u64>()
        + 64;
    let expected = baseline + u64::try_from(expected_delta).expect("expected bytes fit");
    let observed = interval.snapshot().expect("nested overlap");
    assert_eq!(observed.current_reserved_bytes, expected);
    assert_eq!(observed.peak_reserved_bytes, expected);
    assert_eq!(writer.reserved_bytes(), 64);
    drop(writer_reservation);
    drop(arena);
    drop(memory);
    let finished = interval.finish().expect("finish interval");
    assert_eq!(finished.current_reserved_bytes, baseline);
    assert_eq!(finished.peak_reserved_bytes, expected);
    store.close().expect("close");
}

#[test]
fn graph_memory_interval_busy_drop_finish_and_reuse_are_exact() {
    let (_root, store, resources) = open_graph_store();
    let (_other_root, other_store, other_resources) = open_graph_store();
    let baseline = resources.reserved_bytes().expect("baseline");
    let first = resources
        .begin_allocation_interval()
        .expect("first interval");
    let before_busy = first.snapshot().expect("first snapshot");
    assert!(matches!(
        resources.begin_allocation_interval(),
        Err(AllocationIntervalError::Busy)
    ));
    assert_eq!(first.snapshot().expect("unchanged first"), before_busy);
    assert_eq!(
        resources.reserved_bytes().expect("unchanged bytes"),
        baseline
    );
    drop(first);

    let after_drop = resources
        .begin_allocation_interval()
        .expect("drop released slot");
    let finished = after_drop.finish().expect("finish released slot");
    assert_eq!(finished.start_reserved_bytes, baseline);
    assert_eq!(finished.current_reserved_bytes, baseline);
    assert_eq!(finished.peak_reserved_bytes, baseline);

    let replacement = resources
        .begin_allocation_interval()
        .expect("finished guard did not clear replacement");
    let independent = other_resources
        .begin_allocation_interval()
        .expect("other store has an independent slot");
    assert_eq!(
        replacement.snapshot().expect("replacement remains active"),
        finished
    );
    independent.finish().expect("finish independent interval");
    replacement.finish().expect("finish replacement interval");
    resources
        .begin_allocation_interval()
        .expect("slot reusable again")
        .finish()
        .expect("finish final interval");
    other_store.close().expect("close other store");
    store.close().expect("close");
}

#[test]
fn graph_memory_interval_survives_store_close_without_admission() {
    let (_root, store, resources) = open_graph_store();
    let baseline = resources.reserved_bytes().expect("baseline");
    let interval = resources
        .begin_allocation_interval()
        .expect("fresh interval");
    store.close().expect("close while observer lives");
    let after_close = resources.reserved_bytes().expect("post-close accounting");
    assert!(after_close <= baseline);
    let observed = interval.snapshot().expect("post-close snapshot");
    assert_eq!(observed.start_reserved_bytes, baseline);
    assert_eq!(observed.current_reserved_bytes, after_close);
    assert_eq!(observed.peak_reserved_bytes, baseline);
    assert_eq!(interval.finish().expect("post-close finish"), observed);
}

#[cfg(feature = "allocation-audit")]
#[test]
fn graph_memory_interval_observation_is_allocation_free() {
    use zeppelin_embed::adversarial_test_support::audit_engine_path;

    let (_root, store, resources) = open_graph_store();
    let (finished, finish_audit) = audit_engine_path(|| {
        resources.begin_allocation_interval().and_then(|interval| {
            let snapshot = interval.snapshot()?;
            let finished = interval.finish()?;
            Ok((snapshot, finished))
        })
    });
    let (snapshot, finished) = finished.expect("begin, snapshot, finish");
    assert_eq!(snapshot, finished);
    assert_eq!(finish_audit.allocations, 0);
    assert_eq!(finish_audit.attributed_bytes, 0);
    assert_eq!(finish_audit.unattributed_bytes, 0);

    let (dropped, drop_audit) = audit_engine_path(|| {
        resources.begin_allocation_interval().map(|interval| {
            drop(interval);
        })
    });
    dropped.expect("begin and drop");
    assert_eq!(drop_audit.allocations, 0);
    assert_eq!(drop_audit.attributed_bytes, 0);
    assert_eq!(drop_audit.unattributed_bytes, 0);
    store.close().expect("close");
}
