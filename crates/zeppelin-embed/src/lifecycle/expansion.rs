//! Vocabulary expansion and the lifetime of its optional phonetic companion.
use super::*;
use crate::fts::{phonetic, phonetic_index::PhoneticIndex, query};

pub(super) enum ExpansionError {
    Query(QueryError),
    Shape(query::LexicalQueryError),
}
impl From<QueryError> for ExpansionError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}
impl From<query::LexicalQueryError> for ExpansionError {
    fn from(error: query::LexicalQueryError) -> Self {
        Self::Shape(error)
    }
}

pub(super) struct CachedPhonetic {
    index: PhoneticIndex,
    version: u32,
    // A reader can retain this charge after replacement or cache eviction.
    _memory: stats::AccountedCounter,
}

impl CachedVocabulary {
    fn phonetic(
        &self,
        version: u32,
        accounting: &Arc<stats::Accounting>,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<Arc<CachedPhonetic>, QueryError> {
        let mut slot = self.phonetic.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "phonetic vocabulary cache",
            })
        })?;
        if let Some(cached) = slot.as_ref().filter(|cached| cached.version == version) {
            return Ok(Arc::clone(cached));
        }
        let mut memory =
            stats::AccountedCounter::new(accounting, stats::AllocationComponent::Cache)
                .map_err(QueryError::Store)?;
        let mut temporary =
            stats::AccountedCounter::new(accounting, stats::AllocationComponent::Temporary)
                .map_err(QueryError::Store)?;
        let index = PhoneticIndex::build(&self.view, |owned, scratch| {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            prepared_lexical::reserve_weighted_scratch(&mut temporary, scratch)?;
            let bytes = owned
                .and_then(|bytes| bytes.checked_add(std::mem::size_of::<CachedPhonetic>()))
                .ok_or(QueryError::Store(StoreError::BudgetExceeded {
                    needed: u64::MAX,
                    budget: u64::MAX,
                    component: "phonetic vocabulary cache",
                }))?;
            memory.set(bytes).map_err(QueryError::Store)
        })?;
        let cached = Arc::new(CachedPhonetic {
            index,
            version,
            _memory: memory,
        });
        *slot = Some(Arc::clone(&cached));
        Ok(cached)
    }
}

impl LexicalAssembly {
    pub(super) fn expand(
        &self,
        query: &query::LexicalQuery,
        accounting: &Arc<stats::Accounting>,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<Vec<query::LexicalExpansion>, ExpansionError> {
        let vocabulary = if query.needs_vocabulary() {
            Some(self.vocabulary(accounting, cancellation)?)
        } else {
            None
        };
        let empty = crate::fts::vocabulary::Vocabulary::empty();
        let view = vocabulary.as_ref().map_or(&empty, |cached| &cached.view);
        query::expand_with_phonetic(query, view, |code| {
            // The same immutable assembly supplies the dictionary and its index.
            // Both owners stay alive until all selected term bytes are copied.
            let vocabulary = self.vocabulary(accounting, cancellation)?;
            let cached =
                vocabulary.phonetic(phonetic::ENCODER_VERSION, accounting, cancellation)?;
            Ok(query::phonetic_expansions(
                &vocabulary.view,
                &cached.index,
                code,
            ))
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::fts::{
        index::DEFAULT_FIELD, preparation_observer as observer, vocabulary::Vocabulary,
    };
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};

    fn dictionary(store: &Store) -> Arc<CachedVocabulary> {
        let guard = store.lexical_index_cache.entry.lock().expect("assembly");
        let guard = guard
            .as_ref()
            .expect("cached assembly")
            .assembly
            .vocabulary
            .lock()
            .expect("dictionary");
        Arc::clone(guard.as_ref().expect("cached dictionary"))
    }
    fn ingest(store: &Store, id: u128, text: &str) {
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text(text),
            ]))
            .expect("ingest");
    }
    fn search(store: &Store) -> Vec<query::LexicalExpansion> {
        store
            .search_lexical_structured(
                &query::LexicalQuery::phonetic(b"night".to_vec(), DEFAULT_FIELD),
                10,
                64,
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("phonetic")
            .expansions
    }

    #[test]
    fn astra_14_phonetic_cache_tracks_vocabulary_and_algorithm_identity() {
        let dir = tempfile::tempdir().expect("directory");
        let store = Store::open(dir.path(), OpenOptions::default()).expect("open");
        ingest(&store, 1, "knight night apple");
        store.seal().expect("seal");
        store
            .search_lexical_structured(
                &query::LexicalQuery::prefix(b"n".to_vec(), DEFAULT_FIELD),
                1,
                64,
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("prefix");
        let old = dictionary(&store);
        assert!(old.phonetic.lock().expect("slot").is_none());
        let original = search(&store);
        let first = Arc::clone(old.phonetic.lock().expect("slot").as_ref().expect("first"));
        let control = QueryControl::Cancel(CancelToken::new());
        let lease = store.snapshot().expect("snapshot");
        let cancellation = QueryCancellation::new(&control, &lease);
        let reused = old
            .phonetic(phonetic::ENCODER_VERSION, &store.accounting, &cancellation)
            .expect("reuse");
        assert!(Arc::ptr_eq(&first, &reused));
        drop(reused);
        let canceled = CancelToken::new();
        canceled.cancel();
        let canceled_control = QueryControl::Cancel(canceled);
        let canceled_query = QueryCancellation::new(&canceled_control, &lease);
        let refused = old.phonetic(
            phonetic::ENCODER_VERSION + 1,
            &store.accounting,
            &canceled_query,
        );
        assert!(matches!(
            refused,
            Err(QueryError::Scan(crate::scan::ScanError::Cancelled {
                partial: false
            }))
        ));
        assert!(Arc::ptr_eq(
            old.phonetic
                .lock()
                .expect("unchanged slot")
                .as_ref()
                .expect("old map"),
            &first
        ));
        assert_eq!(
            store
                .stats()
                .expect("refused replacement scratch")
                .temporary_bytes,
            0
        );
        observer::begin();
        let replaced = old
            .phonetic(
                phonetic::ENCODER_VERSION + 1,
                &store.accounting,
                &cancellation,
            )
            .expect("new algorithm identity");
        let work = observer::phonetic_index_work();
        observer::take();
        assert_eq!(work.0, 1);
        assert!(!Arc::ptr_eq(&first, &replaced));
        assert_eq!(
            query::phonetic_expansions(&old.view, &first.index, "NT"),
            original
        );
        assert_eq!(
            query::phonetic_expansions(&old.view, &replaced.index, "NT"),
            original
        );
        drop(lease);
        ingest(&store, 2, "knit");
        observer::begin();
        let updated = search(&store);
        assert_eq!(observer::phonetic_index_work(), (1, 3));
        observer::take();
        assert!(!Arc::ptr_eq(&old, &dictionary(&store)));
        assert_eq!(
            updated
                .iter()
                .map(|e| e.term.as_slice())
                .collect::<Vec<_>>(),
            vec![b"knight".as_slice(), b"knit", b"night"]
        );
        store.seal().expect("new snapshot");
        observer::begin();
        assert_eq!(search(&store), updated);
        assert_eq!(observer::phonetic_index_work(), (1, 3));
        observer::take();
        store
            .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(2)]))
            .expect("delete");
        observer::begin();
        assert_eq!(search(&store), updated);
        assert_eq!(observer::phonetic_index_work(), (1, 3));
        observer::take();
        let purge = store.purge(&[DocId::new(2)]).expect("purge");
        store.await_physical_purge(purge).expect("physical purge");
        observer::begin();
        assert_eq!(search(&store), original);
        assert_eq!(observer::phonetic_index_work(), (1, 2));
        observer::take();
        store.close().expect("close");
        let held = old._memory.bytes() + first._memory.bytes() + replaced._memory.bytes();
        assert_eq!(
            store.accounting.audit().expect("held readers").cache_bytes,
            held
        );
        drop(old);
        assert_eq!(
            store.accounting.audit().expect("held indexes").cache_bytes,
            first._memory.bytes() + replaced._memory.bytes()
        );
        drop(first);
        drop(replaced);
        assert_eq!(
            store.accounting.audit().expect("last owners").cache_bytes,
            0
        );
        assert_eq!(
            store.accounting.audit().expect("temporary").temporary_bytes,
            0
        );
    }

    #[test]
    fn astra_14_refused_phonetic_index_is_not_published() {
        let dir = tempfile::tempdir().expect("directory");
        let store =
            Store::open(dir.path(), OpenOptions::default().with_max_temp_bytes(128)).expect("open");
        let terms = [vec![b'a'; 1024]];
        let mut memory =
            stats::AccountedCounter::new(&store.accounting, stats::AllocationComponent::Cache)
                .expect("charge");
        let view = Vocabulary::build(
            terms.iter().map(|term| (term.as_slice(), DEFAULT_FIELD)),
            |_, bytes| {
                memory
                    .set(bytes.expect("bounded fixture") + std::mem::size_of::<CachedVocabulary>())
            },
        )
        .expect("dictionary");
        let dictionary = CachedVocabulary {
            view,
            phonetic: Mutex::new(None),
            _memory: memory,
        };
        let control = QueryControl::Cancel(CancelToken::new());
        let lease = store.snapshot().expect("snapshot");
        let cancellation = QueryCancellation::new(&control, &lease);
        for _ in 0..2 {
            let result =
                dictionary.phonetic(phonetic::ENCODER_VERSION, &store.accounting, &cancellation);
            assert!(matches!(
                result,
                Err(QueryError::Store(StoreError::BudgetExceeded {
                    component: "temporary",
                    ..
                }))
            ));
            assert!(dictionary.phonetic.lock().expect("slot").is_none());
            assert_eq!(store.stats().expect("released").temporary_bytes, 0);
            assert_eq!(
                store.stats().expect("no partial charge").cache_bytes,
                dictionary._memory.bytes()
            );
        }
        drop(lease);
        drop(dictionary);
        store.close().expect("close");
        assert_eq!(store.accounting.audit().expect("released").cache_bytes, 0);
    }
}
