#![allow(clippy::expect_used, clippy::panic)]

use super::*;
use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use std::sync::Barrier;

// Release blocked test work even when a parent assertion fails.
struct ReleaseOnDrop<'a>(&'a Barrier);
impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.wait();
    }
}

fn append(store: &Store, id: u128) {
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("common pair"),
        ]))
        .expect("append");
}

fn fixture() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    append(&store, 1);
    append(&store, 2);
    store.seal().expect("seal");
    append(&store, 3);
    (directory, store)
}

fn assemble(store: &Store, admission: &AdmittedLexicalQuery<'_>) -> Arc<LexicalAssembly> {
    assemble_lexical_index(
        LexicalInputs {
            filter: None,
            generation: admission.generation,
            cache: &store.lexical_index_cache,
            snapshot: &admission.snapshot,
            active: &admission.active,
            accounting: &store.accounting,
        },
        true,
        None,
    )
    .unwrap_or_else(|_| panic!("assemble"))
    .assembly
}

#[test]
fn astra_16_evicted_assembly_remains_accounted_until_last_query_drops() {
    let (_directory, store) = fixture();
    let old_admission = store.admit_lexical_query().expect("old query");
    let old = assemble(&store, &old_admission);
    let old_charge = store
        .accounting
        .audit()
        .expect("old accounting")
        .cache_bytes;
    assert!(old_charge > old.memory.bytes());
    assert!(old.contributions.iter().all(|c| c._memory.bytes() > 0));
    let ready = Barrier::new(2);
    let release = Barrier::new(2);
    std::thread::scope(|scope| {
        let held = scope.spawn(|| {
            let (old, admission) = (old, old_admission);
            ready.wait();
            release.wait();
            assert_eq!(old.index.document_count(), 3);
            assert_eq!(old.index.total_tokens(), 6);
            assert_eq!(old.sources.len(), 2);
            drop(old);
            drop(admission);
        });
        ready.wait();
        let release_on_failure = ReleaseOnDrop(&release);
        append(&store, 4);
        let new_admission = store.admit_lexical_query().expect("new query");
        let new = assemble(&store, &new_admission);
        assert_eq!(new.index.document_count(), 4);
        assert!(store.accounting.audit().expect("both queries").cache_bytes > old_charge);
        drop(
            store
                .lexical_index_cache
                .entry
                .lock()
                .expect("evict")
                .take(),
        );
        drop(new);
        drop(new_admission);
        assert_eq!(
            store
                .accounting
                .audit()
                .expect("old still held")
                .cache_bytes,
            old_charge
        );
        drop(release_on_failure);
        held.join().expect("old query joins");
    });
    assert_eq!(
        store
            .accounting
            .audit()
            .expect("last query gone")
            .cache_bytes,
        0
    );
    store.close().expect("close");
}

#[test]
fn astra_16_delayed_old_query_does_not_replace_newer_assembly() {
    let (_directory, store) = fixture();
    let old = store.admit_lexical_query().expect("old query");
    let ready = Barrier::new(2);
    let proceed = Barrier::new(2);
    std::thread::scope(|scope| {
        let delayed = scope.spawn(|| {
            let old = old;
            ready.wait();
            proceed.wait();
            let assembly = assemble(&store, &old);
            assert_eq!(assembly.index.document_count(), 3);
            assert_eq!(assembly.index.total_tokens(), 6);
        });
        ready.wait();
        let release_on_failure = ReleaseOnDrop(&proceed);
        append(&store, 4);
        let current = store.admit_lexical_query().expect("new query");
        let assembly = assemble(&store, &current);
        drop(release_on_failure);
        delayed.join().expect("old query joins");
        let cached = store.lexical_index_cache.entry.lock().expect("cache");
        let cached = cached.as_ref().expect("new entry retained");
        assert_eq!(cached.generation, current.generation);
        assert!(Arc::ptr_eq(&cached.assembly, &assembly));
        assert_eq!(cached.assembly.index.document_count(), 4);
    });
    store.close().expect("close");
}

#[test]
fn stats_remains_available_while_old_active_generation_is_retained() {
    let (_directory, store) = fixture();
    let old = store.admit_lexical_query().expect("old query");
    let old_active_bytes = old.active.resident_bytes();

    append(&store, 4);

    let retained = store
        .stats()
        .expect("stats while the old active generation is retained");
    assert_eq!(retained.retired_active_segment_bytes, old_active_bytes);
    assert_eq!(
        retained
            .active_segment_bytes
            .checked_add(retained.retired_active_segment_bytes),
        Some(
            store
                .accounting
                .audit()
                .expect("active accounting")
                .active_bytes
        )
    );
    drop(old);

    let released = store.stats().expect("stats after old query releases");
    assert_eq!(released.retired_active_segment_bytes, 0);
    assert_eq!(released.active_segment_bytes, retained.active_segment_bytes);
    assert_eq!(
        retained
            .resident_owned_bytes
            .checked_sub(released.resident_owned_bytes),
        Some(old_active_bytes)
    );
    store.close().expect("close");
}

#[test]
fn astra_16_refused_contribution_leaves_previous_cache_and_no_charge() {
    let (_directory, store) = fixture();
    let old = store.admit_lexical_query().expect("old");
    let assembly = assemble(&store, &old);
    append(&store, 4);
    let current = store.admit_lexical_query().expect("new");
    current
        .active
        .sealed_lexical(&store.accounting)
        .expect("prime decode");
    let refused = Arc::new(stats::Accounting::new(0, u64::MAX));
    let result = assemble_lexical_index(
        LexicalInputs {
            filter: None,
            generation: current.generation,
            cache: &store.lexical_index_cache,
            snapshot: &current.snapshot,
            active: &current.active,
            accounting: &refused,
        },
        true,
        None,
    );
    assert!(matches!(
        result,
        Err(LexicalAssemblyError::Store(StoreError::BudgetExceeded {
            component: "cache",
            ..
        }))
    ));
    assert_eq!(refused.audit().expect("failed reservation").cache_bytes, 0);
    assert!(Arc::ptr_eq(
        &store
            .lexical_index_cache
            .entry
            .lock()
            .expect("cache")
            .as_ref()
            .expect("previous entry")
            .assembly,
        &assembly
    ));
    let fresh = assemble(&store, &current);
    assert_eq!(fresh.index.document_count(), 4);
    drop(fresh);
    drop(current);
    drop(assembly);
    drop(old);
    store.close().expect("close");
}

#[test]
fn astra_17_live_df_cache_memory_released_with_reader() {
    let (_directory, store) = fixture();
    store
        .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(1)]))
        .expect("tombstone");
    let admission = store.admit_lexical_query().expect("query");
    let assembly = assemble(&store, &admission);
    assert_eq!(
        assembly
            .index
            .prepared_document_frequency(b"common", &[crate::fts::index::DEFAULT_FIELD])
            .expect("DF"),
        2
    );
    let cache = assembly
        .contributions
        .iter()
        .find_map(|c| c.statistics.frequency_cache.as_ref())
        .cloned()
        .expect("tombstoned contribution cache");
    let bytes = cache.charged_bytes();
    assert!(bytes > 4096, "keys, entries and Arc storage stay reserved");
    let before = store.accounting.audit().expect("audit").cache_bytes;
    store
        .lexical_index_cache
        .entry
        .lock()
        .expect("cache")
        .take();
    assert_eq!(
        store.accounting.audit().expect("held assembly").cache_bytes,
        before
    );
    drop(assembly);
    drop(admission);
    let held = store
        .accounting
        .audit()
        .expect("last cache owner")
        .cache_bytes;
    assert!(held >= bytes);
    drop(cache);
    assert_eq!(
        store
            .accounting
            .audit()
            .expect("cache released")
            .cache_bytes,
        held - bytes
    );
    let accounting = Arc::clone(&store.accounting);
    store.close().expect("close");
    assert_eq!(accounting.audit().expect("closed").cache_bytes, 0);
}

fn filtered_assembly(store: &Store, predicate: &crate::meta::Predicate) -> Arc<LexicalAssembly> {
    let filter = QueryFilter::new(store.schema(), Some(predicate), None)
        .expect("filter")
        .expect("present");
    let admission = store.admit_lexical_query().expect("admit");
    assemble_lexical_index(
        LexicalInputs {
            filter: Some(&filter),
            generation: admission.generation,
            cache: &store.lexical_index_cache,
            snapshot: &admission.snapshot,
            active: &admission.active,
            accounting: &store.accounting,
        },
        true,
        None,
    )
    .unwrap_or_else(|_| panic!("filtered assembly"))
    .assembly
}

#[test]
fn ze_292_repeated_filter_recomputes_and_releases_rows() {
    let (_dir, store) = fixture();
    let admission = store.admit_lexical_query().expect("admit");
    let base = assemble(&store, &admission);
    let baseline = store.accounting.audit().expect("baseline");
    let predicate = crate::meta::Predicate::Exists(crate::meta::TIMESTAMP_COLUMN);
    crate::fts::preparation_observer::begin();
    let first = filtered_assembly(&store, &predicate);
    let first_work = crate::fts::preparation_observer::filter_work();
    assert!(first_work.0 > 0 && first_work.2 > 0);
    let weak = Arc::downgrade(first.alive_sets.first().expect("set"));
    crate::fts::preparation_observer::begin();
    let second = filtered_assembly(&store, &predicate);
    assert_eq!(first.alive_sets, second.alive_sets);
    assert_eq!(crate::fts::preparation_observer::filter_work(), first_work);
    let eligible_bytes = first
        .alive_sets
        .iter()
        .map(|alive| {
            alive.resident_bytes().expect("bitmap bytes")
                + std::mem::size_of::<crate::meta::AliveSet>()
                + 2 * std::mem::size_of::<usize>()
        })
        .sum::<usize>();
    assert_eq!(
        first.memory.bytes(),
        u64::try_from(first.owned_bytes().expect("owned") + eligible_bytes).expect("charge")
    );
    assert_eq!(
        store.accounting.audit().expect("filtered").temporary_bytes,
        baseline.temporary_bytes + first.memory.bytes() + second.memory.bytes()
    );
    assert_eq!(
        store.accounting.audit().expect("cache").cache_bytes,
        baseline.cache_bytes
    );
    drop(first);
    assert!(
        weak.upgrade().is_none(),
        "eligible rows released with filtered assembly"
    );
    drop(second);
    assert_eq!(
        store.accounting.audit().expect("released").temporary_bytes,
        baseline.temporary_bytes
    );
    assert!(Arc::ptr_eq(&base, &assemble(&store, &admission)));
}

fn reopened_fixture() -> (tempfile::TempDir, Store) {
    let (directory, store) = fixture();
    store.close().expect("close");
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen");
    (directory, reopened)
}
fn exact_query(store: &Store) -> crate::ingest::StoreLexicalSearchOutcome {
    store
        .search_lexical(
            &crate::fts::search::TermQuery::flat(
                vec![b"common".to_vec()],
                &[crate::fts::index::DEFAULT_FIELD],
            ),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query")
}

#[test]
fn ze_265_warm_prepares_first_query() {
    let (_dir, store) = reopened_fixture();
    assert_eq!(store.lexical_index_cache_counters().1, 0);
    crate::fts::preparation_observer::begin();
    store
        .warm_lexical(QueryControl::Cancel(CancelToken::new()))
        .expect("warm");
    assert_eq!(
        store.lexical_index_cache_counters().1,
        1,
        "warm must prepare assembly"
    );
    assert!(
        store
            .lexical_index_cache
            .entry
            .lock()
            .expect("cache")
            .as_ref()
            .expect("assembly")
            .document_identity_verified
    );
    println!(
        "ZE-265 warmed fixture cache bytes={}",
        store.stats().expect("stats").cache_bytes
    );
    let first = exact_query(&store);
    assert_eq!(first.candidates.len(), 3);
    assert_eq!(first.candidates, exact_query(&store).candidates);
    assert_eq!(
        store.lexical_index_cache_counters().1,
        1,
        "first query builds zero assemblies"
    );
    assert_eq!(
        crate::fts::preparation_observer::vocabulary_work().builds,
        1
    );
    assert_eq!(crate::fts::preparation_observer::filter_work(), (0, 0, 0));
    crate::fts::preparation_observer::take();
}

#[test]
fn ze_265_repeated_warm_and_mutation() {
    crate::fts::preparation_observer::begin();
    let (_dir, store) = reopened_fixture();
    for _ in 0..2 {
        store
            .warm_lexical(QueryControl::Cancel(CancelToken::new()))
            .expect("warm");
    }
    assert_eq!(store.lexical_index_cache_counters().1, 1);
    assert_eq!(
        crate::fts::preparation_observer::vocabulary_work().builds,
        1
    );
    append(&store, 4);
    store
        .warm_lexical(QueryControl::Cancel(CancelToken::new()))
        .expect("warm mutation");
    assert_eq!(store.lexical_index_cache_counters().1, 2);
    assert_eq!(
        crate::fts::preparation_observer::vocabulary_work().builds,
        2
    );
    crate::fts::preparation_observer::take();
    assert_eq!(exact_query(&store).candidates.len(), 4);
    assert_eq!(store.lexical_index_cache_counters().1, 2);
}

#[test]
fn ze_265_cancelled_warm_publishes_no_assembly() {
    let (_dir, store) = reopened_fixture();
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        store.warm_lexical(QueryControl::Cancel(token)),
        Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
            crate::scan::ScanError::Cancelled { partial: false }
        )))
    ));
    assert_eq!(store.lexical_index_cache_counters().1, 0);
    assert!(
        store
            .lexical_index_cache
            .entry
            .lock()
            .expect("cache")
            .is_none()
    );
    store
        .warm_lexical(QueryControl::Cancel(CancelToken::new()))
        .expect("retry with fresh control");
    assert_eq!(store.lexical_index_cache_counters().1, 1);
}

#[test]
fn ze_265_warm_prepares_first_prefix_query() {
    use crate::fts::{index::DEFAULT_FIELD, query::LexicalQuery, search::FieldWeights};
    let query = LexicalQuery::TermsWithPrefix {
        terms: vec![b"common".to_vec()],
        prefix: b"pa".to_vec(),
        fields: FieldWeights::flat(&[DEFAULT_FIELD]),
    };
    let run = |store: &Store| {
        store
            .search_lexical_structured(&query, 10, 64, QueryControl::Cancel(CancelToken::new()))
            .expect("prefix query")
    };
    let (_cold_dir, cold) = reopened_fixture();
    let expected = run(&cold);
    let (_dir, store) = reopened_fixture();
    crate::fts::preparation_observer::begin();
    store
        .warm_lexical(QueryControl::Cancel(CancelToken::new()))
        .expect("warm");
    assert_eq!(
        crate::fts::preparation_observer::vocabulary_work().builds,
        1
    );
    crate::fts::preparation_observer::begin();
    assert_eq!(run(&store).candidates, expected.candidates);
    assert_eq!(
        exact_query(&store).candidates,
        exact_query(&cold).candidates
    );
    assert_eq!(store.lexical_index_cache_counters().1, 1);
    assert_eq!(
        crate::fts::preparation_observer::vocabulary_work().builds,
        0
    );
    crate::fts::preparation_observer::take();
}

#[test]
fn ze_265_cancelled_vocabulary_is_not_published_and_retry_succeeds() {
    use std::time::{Duration, Instant};
    struct BuildClock {
        base: Instant,
    }
    impl MonotonicClock for BuildClock {
        fn now(&self) -> Instant {
            if crate::fts::preparation_observer::vocabulary_group_checks() > 0 {
                return self.base + Duration::from_secs(2);
            }
            self.base
        }
    }
    let dir = tempfile::tempdir().expect("directory");
    let mut store = Store::open(dir.path(), OpenOptions::default()).expect("open");
    let text = (0..4096)
        .map(|i| format!("word{i:04}"))
        .collect::<Vec<_>>()
        .join(" ");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(text),
        ]))
        .expect("ingest");
    exact_query(&store); // Only the assembly is prepared.
    let before = store.stats().expect("stats").cache_bytes;
    let clock = Arc::new(BuildClock {
        base: Instant::now(),
    });
    let deadline =
        Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone()).expect("deadline");
    store.clock = clock;
    crate::fts::preparation_observer::begin();
    assert!(matches!(
        store.warm_lexical(QueryControl::Deadline(deadline)),
        Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
            crate::scan::ScanError::Timeout { partial: false }
        )))
    ));
    assert!(crate::fts::preparation_observer::vocabulary_group_checks() > 0);
    crate::fts::preparation_observer::take();
    assert!(
        store
            .lexical_index_cache
            .entry
            .lock()
            .expect("cache")
            .as_ref()
            .expect("assembly")
            .assembly
            .vocabulary
            .lock()
            .expect("vocabulary")
            .is_none()
    );
    assert_eq!(store.stats().expect("stats").cache_bytes, before);
    assert_eq!(store.stats().expect("stats").temporary_bytes, 0);
    store.clock = Arc::new(SystemMonotonicClock);
    store
        .warm_lexical(QueryControl::Cancel(CancelToken::new()))
        .expect("retry");
    assert_eq!(store.lexical_index_cache_counters().1, 1);
    assert!(
        store
            .lexical_index_cache
            .entry
            .lock()
            .expect("cache")
            .as_ref()
            .expect("assembly")
            .assembly
            .vocabulary
            .lock()
            .expect("vocabulary")
            .is_some()
    );
}
