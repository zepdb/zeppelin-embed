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
