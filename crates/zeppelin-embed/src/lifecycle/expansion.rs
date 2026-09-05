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
        let mut slot = cancellation.cache_lock(&self.phonetic, "phonetic vocabulary cache")?;
        if let Some(cached) = slot.as_ref().filter(|cached| cached.version == version) {
            return Ok(Arc::clone(cached));
        }
        let mut memory =
            stats::AccountedCounter::new(accounting, stats::AllocationComponent::Cache)
                .map_err(QueryError::Store)?;
        let mut temporary =
            stats::AccountedCounter::new(accounting, stats::AllocationComponent::Temporary)
                .map_err(QueryError::Store)?;
        let mut work = crate::fts::control::WorkCheck::new(|| {
            cancellation.check_graph().map_err(QueryError::Scan)
        });
        let index = PhoneticIndex::build_controlled(
            &self.view,
            |owned, scratch| {
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
            },
            &mut work,
        )?;
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
        let mut work = crate::fts::control::WorkCheck::new(|| {
            cancellation
                .check_graph()
                .map_err(QueryError::Scan)
                .map_err(ExpansionError::Query)
        });
        query::expand_with_phonetic_controlled(query, view, &mut work, |code| {
            // The same immutable assembly supplies the dictionary and its index.
            // Both owners stay alive until all selected term bytes are copied.
            let vocabulary = self.vocabulary(accounting, cancellation)?;
            let cached =
                vocabulary.phonetic(phonetic::ENCODER_VERSION, accounting, cancellation)?;
            let mut work = crate::fts::control::WorkCheck::new(|| {
                cancellation
                    .check_graph()
                    .map_err(QueryError::Scan)
                    .map_err(ExpansionError::Query)
            });
            query::phonetic_expansions_controlled(&vocabulary.view, &cached.index, code, &mut work)
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

    #[test]
    fn astra_18_public_cold_active_seal_cancels_without_cache_publication() {
        use crate::fts::sealed::SEAL_LENGTH_PROBES;
        use std::time::{Duration, Instant};
        struct SealClock {
            base: Instant,
        }
        impl MonotonicClock for SealClock {
            fn now(&self) -> Instant {
                if SEAL_LENGTH_PROBES.with(std::cell::Cell::get) >= 64 {
                    self.base + Duration::from_secs(2)
                } else {
                    self.base
                }
            }
        }
        let directory = tempfile::tempdir().expect("directory");
        let reference_directory = tempfile::tempdir().expect("reference directory");
        let mut store = Store::open(directory.path(), OpenOptions::default()).expect("store");
        let mut reference = Store::open(reference_directory.path(), OpenOptions::default())
            .expect("reference store");
        for target in [&mut store, &mut reference] {
            target
                .ingest(IngestBatch::new(
                    (0..4_096)
                        .map(|id| {
                            IngestDocument::new(
                                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                                vec![1.0, 0.0],
                            )
                            .with_text("alpha beta")
                        })
                        .collect(),
                ))
                .expect("active documents");
        }
        let query = crate::fts::search::TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let expected = reference
            .search_lexical(&query, 10, QueryControl::Cancel(CancelToken::new()))
            .expect("complete reference");
        let before = store.stats().expect("before");
        SEAL_LENGTH_PROBES.with(|value| value.set(0));
        let clock = Arc::new(SealClock {
            base: Instant::now(),
        });
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("original deadline");
        store.clock = clock;
        let result = store.search_lexical(&query, 10, QueryControl::Deadline(deadline));
        let probes = SEAL_LENGTH_PROBES.with(std::cell::Cell::get);
        let after = store.stats().expect("after");
        println!(
            "cold active seal rows={probes}; active bytes={}->{}; cache={}->{}; temporary={}",
            before.active_segment_bytes,
            after.active_segment_bytes,
            before.cache_bytes,
            after.cache_bytes,
            after.temporary_bytes
        );
        assert!(matches!(
            result,
            Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
                crate::scan::ScanError::Timeout { partial: false }
            )))
        ));
        assert!((64..=128).contains(&probes));
        assert_eq!(after.active_segment_bytes, before.active_segment_bytes);
        assert_eq!(after.cache_bytes, before.cache_bytes);
        assert_eq!(after.temporary_bytes, 0);
        assert!(
            store
                .lexical_index_cache
                .entry
                .lock()
                .expect("assembly cache")
                .is_none()
        );
        store.clock = Arc::new(SystemMonotonicClock);
        let clean = store
            .search_lexical(&query, 10, QueryControl::Cancel(CancelToken::new()))
            .expect("fresh complete query");
        assert_eq!(clean.candidates, expected.candidates);
        assert_eq!(clean.generation, expected.generation);
    }

    #[test]
    fn astra_18_public_statistics_timeout_releases_contribution_reservation() {
        use std::time::{Duration, Instant};
        struct StatisticsClock {
            base: Instant,
            expired: std::sync::atomic::AtomicBool,
        }
        impl MonotonicClock for StatisticsClock {
            fn now(&self) -> Instant {
                if observer::live_statistics_rows() >= 64 {
                    self.expired
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if self.expired.load(std::sync::atomic::Ordering::Relaxed) {
                    self.base + Duration::from_secs(2)
                } else {
                    self.base
                }
            }
        }
        let directory = tempfile::tempdir().expect("directory");
        let mut store = Store::open(directory.path(), OpenOptions::default()).expect("store");
        store
            .ingest(IngestBatch::new(
                (0..4_096)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0, 0.0],
                        )
                        .with_text("alpha beta")
                    })
                    .collect(),
            ))
            .expect("documents");
        store.seal().expect("sealed source");
        let query = crate::fts::search::TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let before_result = store
            .search_lexical(&query, 10, QueryControl::Cancel(CancelToken::new()))
            .expect("complete reference");
        assert_eq!(before_result.candidates.len(), 10);
        drop(
            store
                .lexical_index_cache
                .entry
                .lock()
                .expect("evict assembly")
                .take(),
        );
        let before = store.stats().expect("baseline").cache_bytes;
        observer::begin();
        let clock = Arc::new(StatisticsClock {
            base: Instant::now(),
            expired: std::sync::atomic::AtomicBool::new(false),
        });
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("original deadline");
        store.clock = clock;
        let result = store.search_lexical(&query, 10, QueryControl::Deadline(deadline));
        let rows = observer::live_statistics_rows();
        observer::take();
        let after = store.stats().expect("released accounting");
        println!(
            "public live rows={rows}, cache={before}->{}, temporary={}",
            after.cache_bytes, after.temporary_bytes
        );
        assert!(matches!(
            result,
            Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
                crate::scan::ScanError::Timeout { partial: false }
            )))
        ));
        assert!((64..=128).contains(&rows));
        assert!(
            store
                .lexical_index_cache
                .entry
                .lock()
                .expect("no partial assembly")
                .is_none()
        );
        assert_eq!(after.cache_bytes, before);
        assert_eq!(after.temporary_bytes, 0);
        store.clock = Arc::new(SystemMonotonicClock);
        let clean = store
            .search_lexical(&query, 10, QueryControl::Cancel(CancelToken::new()))
            .expect("fresh complete query");
        assert_eq!(clean.candidates, before_result.candidates);
        assert_eq!(clean.generation, before_result.generation);
    }

    #[test]
    fn astra_18_public_lexical_deadline_reaches_scoring() {
        use std::time::{Duration, Instant};
        struct ScoringClock {
            base: Instant,
            expired: std::sync::atomic::AtomicBool,
        }
        impl MonotonicClock for ScoringClock {
            fn now(&self) -> Instant {
                if observer::corpus_statistics_calls() > 0 {
                    self.expired
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if self.expired.load(std::sync::atomic::Ordering::Relaxed) {
                    self.base + Duration::from_secs(2)
                } else {
                    self.base
                }
            }
        }
        let directory = tempfile::tempdir().expect("directory");
        let mut store = Store::open(directory.path(), OpenOptions::default()).expect("store");
        store
            .ingest(IngestBatch::new(
                (0..4_097)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0, 0.0],
                        )
                        .with_text("alpha beta")
                    })
                    .collect(),
            ))
            .expect("documents");
        store.seal().expect("sealed source");
        store
            .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(4_096)]))
            .expect("one tombstone");
        let query = crate::fts::search::TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let before_result = store
            .search_lexical(&query, 10, QueryControl::Cancel(CancelToken::new()))
            .expect("complete reference");
        assert_eq!(before_result.candidates.len(), 10);
        drop(
            store
                .lexical_index_cache
                .entry
                .lock()
                .expect("evict assembly")
                .take(),
        );
        // Prime only the assembly, leaving its new live-DF cache empty.
        // The deadline becomes expired after the scorer reads corpus statistics.
        let admission = store.admit_lexical_query().expect("prime admission");
        let primed = assemble_lexical_index(
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
        .ok()
        .expect("prime assembly");
        drop(primed);
        drop(admission);
        let before = store.stats().expect("baseline").cache_bytes;
        observer::begin();
        let clock = Arc::new(ScoringClock {
            base: Instant::now(),
            expired: std::sync::atomic::AtomicBool::new(false),
        });
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("original deadline");
        store.clock = clock;
        let result = store.search_lexical(&query, 10, QueryControl::Deadline(deadline));
        let rows = observer::live_df_work().docids;
        observer::take();
        let after = store.stats().expect("released accounting");
        println!(
            "public scoring DF rows={rows}, cache={before}->{}, temporary={}",
            after.cache_bytes, after.temporary_bytes
        );
        assert!(matches!(
            result,
            Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
                crate::scan::ScanError::Timeout { partial: false }
            )))
        ));
        assert!(
            rows <= 128,
            "deadline must reach scoring before the complete live-DF walk"
        );
        assert!(
            store
                .lexical_index_cache
                .entry
                .lock()
                .expect("retained complete assembly")
                .is_some()
        );
        assert_eq!(after.cache_bytes, before);
        assert_eq!(after.temporary_bytes, 0);
        store.clock = Arc::new(SystemMonotonicClock);
        let clean = store
            .search_lexical(&query, 10, QueryControl::Cancel(CancelToken::new()))
            .expect("fresh complete query");
        assert_eq!(clean.candidates, before_result.candidates);
        assert_eq!(clean.generation, before_result.generation);
    }

    #[test]
    fn astra_18_assembly_cache_wait_uses_original_deadline() {
        use std::sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::{Duration, Instant};
        struct Clock {
            inner: ManualMonotonicClock,
            entered: Barrier,
            resume: Barrier,
            armed: AtomicBool,
        }
        impl MonotonicClock for Clock {
            fn now(&self) -> Instant {
                let observed = self.inner.now();
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.wait();
                    self.resume.wait();
                }
                observed
            }
        }
        let directory = tempfile::tempdir().expect("directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store");
        ingest(&store, 1, "night knight");
        assert_eq!(search(&store).len(), 2);
        let before = store.stats().expect("baseline").cache_bytes;
        let clock = Arc::new(Clock {
            inner: ManualMonotonicClock::new(),
            entered: Barrier::new(2),
            resume: Barrier::new(2),
            armed: AtomicBool::new(false),
        });
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("deadline");
        let control = QueryControl::Deadline(deadline);
        let lease = store.snapshot().expect("lease");
        let cache = &store.lexical_index_cache;
        let held = cache.entry.lock().expect("hold actual assembly cache");
        clock.armed.store(true, Ordering::SeqCst);
        let before_unlock = std::thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(1);
            let worker = scope.spawn(move || {
                let cancellation = QueryCancellation::new(&control, &lease);
                let result = cache.lock_controlled(Some(&cancellation));
                send.send(matches!(
                    result,
                    Err(LexicalAssemblyError::Cancelled(
                        crate::scan::ScanError::Timeout { partial: false }
                    ))
                ))
                .expect("cache admission result");
            });
            clock.entered.wait();
            clock.inner.advance(Duration::from_secs(2));
            clock.resume.wait();
            let before_unlock = receive.recv_timeout(Duration::from_secs(2));
            drop(held);
            worker.join().expect("join even after failed watchdog");
            before_unlock
        });
        println!("assembly deadline returned while holder retained cache: {before_unlock:?}");
        assert_eq!(before_unlock, Ok(true));
        let retained = cache.lock_controlled(None).ok().expect("legacy admission");
        assert!(
            retained.is_some(),
            "cancellation does not evict the complete assembly"
        );
        drop(retained);
        let after = store.stats().expect("released scratch");
        assert_eq!(after.cache_bytes, before);
        assert_eq!(after.temporary_bytes, 0);
    }

    #[test]
    fn astra_18_public_phrase_timeout_releases_partial_position_work() {
        use crate::fts::postings::POSITION_DECODE_PROBES;
        use std::time::{Duration, Instant};
        struct DecodeClock {
            base: Instant,
            expired: std::sync::atomic::AtomicBool,
        }
        impl MonotonicClock for DecodeClock {
            fn now(&self) -> Instant {
                if POSITION_DECODE_PROBES.with(std::cell::Cell::get) >= 64 {
                    self.expired
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if self.expired.load(std::sync::atomic::Ordering::Relaxed) {
                    self.base + Duration::from_secs(2)
                } else {
                    self.base
                }
            }
        }
        let dir = tempfile::tempdir().expect("directory");
        let mut store = Store::open(dir.path(), OpenOptions::default()).expect("store");
        ingest(&store, 1, &format!("{}beta", "alpha ".repeat(4_096)));
        store.seal().expect("persist source positions");
        let query = query::LexicalQuery::phrase(
            vec![b"alpha".to_vec(), b"beta".to_vec()],
            0,
            DEFAULT_FIELD,
        );
        let clean_before = store
            .search_lexical_structured(&query, 1, 64, QueryControl::Cancel(CancelToken::new()))
            .expect("warm complete assembly");
        assert_eq!(
            clean_before.candidates.len(),
            1,
            "literal adjacent final pair"
        );
        let before = store.stats().expect("baseline").cache_bytes;
        POSITION_DECODE_PROBES.with(|value| value.set(0));
        observer::begin();
        let clock = Arc::new(DecodeClock {
            base: Instant::now(),
            expired: std::sync::atomic::AtomicBool::new(false),
        });
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("original deadline");
        store.clock = clock;
        let result =
            store.search_lexical_structured(&query, 1, 64, QueryControl::Deadline(deadline));
        let probes = POSITION_DECODE_PROBES.with(std::cell::Cell::get);
        let observed = observer::phrase_position_work();
        observer::take();
        let after = store.stats().expect("released scratch");
        println!(
            "public phrase probes={probes}, position_work={observed:?}, temporary={}, cache={before}->{}",
            after.temporary_bytes, after.cache_bytes
        );
        assert!(matches!(
            result,
            Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
                crate::scan::ScanError::Timeout { partial: false }
            )))
        ));
        assert!(
            (64..=128).contains(&probes),
            "public deadline must reach the position loop"
        );
        assert_eq!(
            observed.0, probes,
            "partial decoded positions remain counted"
        );
        assert_eq!(after.temporary_bytes, 0);
        assert_eq!(after.cache_bytes, before);
        store.clock = Arc::new(SystemMonotonicClock);
        let clean_after = store
            .search_lexical_structured(&query, 1, 64, QueryControl::Cancel(CancelToken::new()))
            .expect("next query completes");
        assert_eq!(clean_after.candidates, clean_before.candidates);
        assert_eq!(clean_after.expansions, clean_before.expansions);
        assert_eq!(clean_after.generation, clean_before.generation);
    }

    #[test]
    fn astra_18_public_expansion_timeout_does_not_publish_partial_caches() {
        use std::time::{Duration, Instant};
        struct BuildClock {
            base: Instant,
            phonetic: bool,
            expired: std::sync::atomic::AtomicBool,
        }
        impl MonotonicClock for BuildClock {
            fn now(&self) -> Instant {
                let expired = if self.phonetic {
                    observer::phonetic_encoding_calls() >= 64
                } else {
                    observer::vocabulary_group_checks() > 0
                };
                if expired {
                    self.expired
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if self.expired.load(std::sync::atomic::Ordering::Relaxed) {
                    self.base + Duration::from_secs(2)
                } else {
                    self.base
                }
            }
        }
        for phonetic in [false, true] {
            let dir = tempfile::tempdir().expect("directory");
            let mut store = Store::open(dir.path(), OpenOptions::default()).expect("store");
            let text = (0..4_096)
                .map(|i| format!("night{i:04}"))
                .collect::<Vec<_>>()
                .join(" ");
            ingest(&store, 1, &text);
            let query = if phonetic {
                query::LexicalQuery::phonetic(b"night".to_vec(), DEFAULT_FIELD)
            } else {
                query::LexicalQuery::prefix(b"n".to_vec(), DEFAULT_FIELD)
            };
            let warm = if phonetic {
                query::LexicalQuery::prefix(b"n".to_vec(), DEFAULT_FIELD)
            } else {
                query::LexicalQuery::Term(crate::fts::search::TermQuery::flat(
                    vec![b"night".to_vec()],
                    &[DEFAULT_FIELD],
                ))
            };
            store
                .search_lexical_structured(&warm, 1, 64, QueryControl::Cancel(CancelToken::new()))
                .expect("warm assembly and optional dictionary");
            let before = store.stats().expect("baseline").cache_bytes;
            observer::begin();
            let clock = Arc::new(BuildClock {
                base: Instant::now(),
                phonetic,
                expired: std::sync::atomic::AtomicBool::new(false),
            });
            let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
                .expect("deadline");
            store.clock = clock;
            let result =
                store.search_lexical_structured(&query, 1, 64, QueryControl::Deadline(deadline));
            let calls = observer::phonetic_encoding_calls();
            let groups = observer::vocabulary_group_checks();
            observer::take();
            assert!(matches!(
                result,
                Err(crate::ingest::StoreLexicalError::Query(QueryError::Scan(
                    crate::scan::ScanError::Timeout { partial: false }
                )))
            ));
            let entry = store.lexical_index_cache.entry.lock().expect("assembly");
            let cached = entry
                .as_ref()
                .expect("cached assembly")
                .assembly
                .vocabulary
                .lock()
                .expect("dictionary slot");
            if phonetic {
                assert!(
                    cached
                        .as_ref()
                        .expect("retained dictionary")
                        .phonetic
                        .lock()
                        .expect("phonetic slot")
                        .is_none()
                );
                assert!(calls >= 64 && calls <= 128);
            } else {
                assert!(cached.is_none());
                assert!(groups > 0);
            }
            drop(cached);
            drop(entry);
            let after = store.stats().expect("released allocations");
            assert_eq!(after.temporary_bytes, 0);
            assert_eq!(after.cache_bytes, before, "no partial retained charge");
            store.clock = Arc::new(SystemMonotonicClock);
            let clean = store
                .search_lexical_structured(&query, 1, 64, QueryControl::Cancel(CancelToken::new()))
                .expect("fresh request succeeds");
            assert!(!clean.expansions.is_empty());
            println!(
                "public phonetic={phonetic}, encodings={calls}, group_checks={groups}, cache_bytes={before}, temporary=0"
            );
        }
    }

    #[test]
    fn astra_18_dictionary_cache_waits_use_the_original_deadline() {
        use std::sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::{Duration, Instant};
        struct Clock {
            inner: ManualMonotonicClock,
            entered: Barrier,
            resume: Barrier,
            armed: AtomicBool,
        }
        impl MonotonicClock for Clock {
            fn now(&self) -> Instant {
                let observed = self.inner.now();
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.wait();
                    self.resume.wait();
                }
                observed
            }
        }
        fn exercise<T: Send>(
            mutex: &Mutex<T>,
            store: &Store,
            run: impl Fn(&QueryCancellation<'_>) -> bool + Sync,
        ) -> bool {
            let clock = Arc::new(Clock {
                inner: ManualMonotonicClock::new(),
                entered: Barrier::new(2),
                resume: Barrier::new(2),
                armed: AtomicBool::new(false),
            });
            let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
                .expect("deadline");
            let control = QueryControl::Deadline(deadline);
            let lease = store.snapshot().expect("lease");
            let held = mutex.lock().expect("held cache");
            clock.armed.store(true, Ordering::SeqCst);
            std::thread::scope(|scope| {
                let (send, receive) = mpsc::channel();
                let run = &run;
                let worker = scope.spawn(move || {
                    let cancellation = QueryCancellation::new(&control, &lease);
                    send.send(run(&cancellation)).expect("completion");
                });
                // The entry checkpoint has captured an unexpired instant. Move
                // time forward before it can attempt the already-held mutex.
                clock.entered.wait();
                clock.inner.advance(Duration::from_secs(2));
                clock.resume.wait();
                let before_unlock = receive.recv_timeout(Duration::from_secs(2));
                drop(held);
                worker.join().expect("worker");
                before_unlock == Ok(true)
            })
        }
        let dir = tempfile::tempdir().expect("directory");
        let store = Store::open(dir.path(), OpenOptions::default()).expect("store");
        ingest(&store, 1, "night knight");
        store
            .search_lexical_structured(
                &query::LexicalQuery::prefix(b"n".to_vec(), DEFAULT_FIELD),
                1,
                64,
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("prepare dictionary");
        let vocabulary = dictionary(&store);
        let assembly = Arc::clone(
            &store
                .lexical_index_cache
                .entry
                .lock()
                .expect("assembly")
                .as_ref()
                .expect("cached")
                .assembly,
        );
        let vocabulary_done = exercise(&assembly.vocabulary, &store, |control| {
            matches!(
                assembly.vocabulary(&store.accounting, control),
                Err(QueryError::Scan(crate::scan::ScanError::Timeout {
                    partial: false
                }))
            )
        });
        let phonetic_done = exercise(&vocabulary.phonetic, &store, |control| {
            matches!(
                vocabulary.phonetic(phonetic::ENCODER_VERSION, &store.accounting, control),
                Err(QueryError::Scan(crate::scan::ScanError::Timeout {
                    partial: false
                }))
            )
        });
        println!(
            "returned while locks held: vocabulary={vocabulary_done}, phonetic={phonetic_done}"
        );
        assert!(
            vocabulary_done && phonetic_done,
            "cache waits must observe the original deadline before their holder releases"
        );
        assert!(vocabulary.phonetic.lock().expect("unpublished").is_none());
        assert_eq!(store.stats().expect("released scratch").temporary_bytes, 0);
        assert_eq!(
            search(&store).len(),
            2,
            "fresh control may complete after cancellation"
        );
    }

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
