//! Bounded exact term-frequency memoization for one immutable live membership.

use std::sync::{Arc, Mutex};

use super::index::{FieldId, IndexError};
use super::sealed::SealedSegment;
use super::search::ControlledSearchError;
use crate::lifecycle::{StoreError, stats};
use crate::meta::DocBitmap;

const ENTRIES: usize = 64;
const KEY_BYTES: usize = 4096;

#[derive(Clone, Copy)]
struct Entry {
    start: usize,
    term_len: usize,
    fields_len: usize,
    frequency: u32,
    used: u64,
}

struct Table {
    entries: Vec<Entry>,
    keys: Vec<u8>,
    clock: u64,
}

/// All backing capacity is reserved before this object is published. Values
/// are filled on demand without further allocations. The reservation follows
/// this Arc, including contributions retained by old query admissions.
pub(crate) struct LiveFrequencyCache {
    table: Mutex<Table>,
    _memory: stats::AccountedCounter,
}

impl std::fmt::Debug for LiveFrequencyCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveFrequencyCache").finish_non_exhaustive()
    }
}

fn invalid() -> IndexError {
    IndexError::LiveFrequencyCache {
        reason: "invalid cache offsets",
    }
}

impl LiveFrequencyCache {
    #[cfg(test)]
    pub(crate) fn charged_bytes(&self) -> u64 {
        self._memory.bytes()
    }

    pub(crate) fn new(accounting: &Arc<stats::Accounting>) -> Result<Arc<Self>, StoreError> {
        let overhead = std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>();
        let bytes = overhead + ENTRIES * std::mem::size_of::<Entry>() + KEY_BYTES;
        let mut memory =
            stats::AccountedCounter::new(accounting, stats::AllocationComponent::Cache)?;
        memory.set(bytes)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(ENTRIES)
            .map_err(|_| StoreError::AllocationFailed {
                needed: bytes as u64,
                component: "live document frequency cache",
            })?;
        let mut keys = Vec::new();
        keys.try_reserve_exact(KEY_BYTES)
            .map_err(|_| StoreError::AllocationFailed {
                needed: bytes as u64,
                component: "live document frequency cache",
            })?;
        // Account actual Vec capacities even if the allocator over-reserves.
        let actual = entries
            .capacity()
            .checked_mul(std::mem::size_of::<Entry>())
            .and_then(|n| n.checked_add(keys.capacity()))
            .and_then(|n| n.checked_add(overhead))
            .ok_or(StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "cache",
            })?;
        memory.set(actual)?;
        Ok(Arc::new(Self {
            table: Mutex::new(Table {
                entries,
                keys,
                clock: 0,
            }),
            _memory: memory,
        }))
    }

    /// `fields` comes from normalized FieldWeights: sorted and unique.
    pub(crate) fn frequency(
        &self,
        segment: &SealedSegment,
        live: &DocBitmap,
        term: &[u8],
        fields: &[FieldId],
    ) -> Result<u32, IndexError> {
        self.frequency_inner::<false, std::convert::Infallible>(segment, live, term, fields, || {
            Ok(())
        })
        .map_err(ControlledSearchError::into_index_error)
    }

    pub(crate) fn frequency_controlled<E>(
        &self,
        segment: &SealedSegment,
        live: &DocBitmap,
        term: &[u8],
        fields: &[FieldId],
        check: impl FnMut() -> Result<(), E>,
    ) -> Result<u32, ControlledSearchError<E>> {
        self.frequency_inner::<true, E>(segment, live, term, fields, check)
    }

    fn frequency_inner<const CONTROLLED: bool, E>(
        &self,
        segment: &SealedSegment,
        live: &DocBitmap,
        term: &[u8],
        fields: &[FieldId],
        mut check: impl FnMut() -> Result<(), E>,
    ) -> Result<u32, ControlledSearchError<E>> {
        check().map_err(ControlledSearchError::Control)?;
        if let Some(frequency) = segment.single_posting_live_frequency(term, fields, live) {
            return Ok(frequency);
        }
        let Some(key_bytes) = fields
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(term.len()))
        else {
            return segment
                .live_document_frequency_controlled(term, fields, live, check)
                .map_err(ControlledSearchError::Control);
        };
        // This limits cache admission, never the accepted query or exact DF.
        if key_bytes > KEY_BYTES {
            return segment
                .live_document_frequency_controlled(term, fields, live, check)
                .map_err(ControlledSearchError::Control);
        }
        let mut table = if CONTROLLED {
            loop {
                match self.table.try_lock() {
                    Ok(table) => break table,
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err(IndexError::LiveFrequencyCache {
                            reason: "poisoned cache lock",
                        }
                        .into());
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        check().map_err(ControlledSearchError::Control)?;
                        std::thread::park_timeout(std::time::Duration::from_millis(1));
                    }
                }
            }
        } else {
            self.table
                .lock()
                .map_err(|_| IndexError::LiveFrequencyCache {
                    reason: "poisoned cache lock",
                })?
        };
        check().map_err(ControlledSearchError::Control)?;
        table.clock = table.clock.saturating_add(1);
        let clock = table.clock;
        let mut matched = None;
        for (slot, entry) in table.entries.iter().enumerate() {
            if table.matches(entry, term, fields)? {
                matched = Some(slot);
                break;
            }
        }
        if let Some(slot) = matched {
            let entry = table.entries.get_mut(slot).ok_or_else(invalid)?;
            entry.used = clock;
            return Ok(entry.frequency);
        }
        // Serialize misses for this immutable contribution. No partially built
        // value is published, and concurrent queries cannot multiply capacity.
        let frequency = segment
            .live_document_frequency_controlled(term, fields, live, &mut check)
            .map_err(ControlledSearchError::Control)?;
        while table.entries.len() == ENTRIES || table.keys.len() > KEY_BYTES - key_bytes {
            table.evict()?;
        }
        let start = table.keys.len();
        table.keys.extend_from_slice(term);
        for field in fields {
            table.keys.extend_from_slice(&field.0.to_le_bytes());
        }
        table.entries.push(Entry {
            start,
            term_len: term.len(),
            fields_len: fields.len(),
            frequency,
            used: clock,
        });
        Ok(frequency)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::items_after_test_module,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::fts::{
        bm25::Bm25Params,
        index::{Document, LexicalIndex, LiveSegmentStatistics, SegmentIndex},
        preparation_observer as observer,
        search::{PreparedTermQuery, TermQuery},
        tokenizer::{Analyzer, Profile},
    };

    const TEXTS: [[&str; 3]; 6] = [
        ["alpha", "alpha", "beta"],
        ["beta", "alpha", "beta"],
        ["alpha", "beta", "alpha"],
        ["beta", "beta", "alpha"],
        ["alpha", "alpha", "alpha"],
        ["beta", "beta", "beta"],
    ];

    #[test]
    fn astra_18_cancelled_live_df_does_not_publish_partial_cache() {
        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        let cache = LiveFrequencyCache::new(&accounting).unwrap();
        let analyzer = Analyzer::new(Profile::Code.config()).unwrap();
        let mut builder = SegmentIndex::new();
        for _ in 0..4_096 {
            builder
                .push_document(&analyzer, &Document::with_text("alpha"))
                .unwrap();
        }
        let segment = SealedSegment::seal(&builder).unwrap();
        let live = DocBitmap::from_ids((0..4_096).step_by(2));
        observer::begin();
        let mut checks = 0;
        let result = cache.frequency_controlled(&segment, &live, b"alpha", &[FieldId(0)], || {
            checks += 1;
            if checks == 5 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let work = observer::live_df_work();
        observer::take();
        println!("checks={checks}, DF work={work:?}");
        assert!(matches!(
            result,
            Err(super::super::search::ControlledSearchError::Control(
                "cancelled"
            ))
        ));
        assert!(
            work.docids > 0 && work.docids <= 128,
            "must stop during the exact count"
        );
        assert!(
            cache.table.lock().unwrap().entries.is_empty(),
            "an aborted count must publish nothing"
        );
        assert_eq!(
            cache
                .frequency(&segment, &live, b"alpha", &[FieldId(0)])
                .unwrap(),
            2_048
        );
        assert_eq!(cache.table.lock().unwrap().entries.len(), 1);
        observer::begin();
        assert!(matches!(
            cache.frequency_controlled(&segment, &live, b"alpha", &[FieldId(0)], || Ok::<(), ()>(
                ()
            )),
            Ok(2_048)
        ));
        assert_eq!(
            observer::live_df_work(),
            observer::LiveDfWork::default(),
            "complete values remain reusable"
        );
        observer::take();
    }

    #[test]
    fn astra_18_live_df_lock_wait_is_cancelable() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        let cache = LiveFrequencyCache::new(&accounting).unwrap();
        let segment = segment();
        let live = DocBitmap::from_ids([0, 2, 3, 5]);
        let canceled = AtomicBool::new(false);
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let held = cache.table.lock().unwrap();
            let (cache, segment, live, canceled) = (&cache, &segment, &live, &canceled);
            let worker = scope.spawn(move || {
                let mut checks = 0;
                let result =
                    cache.frequency_controlled(segment, live, b"alpha", &[FieldId(0)], || {
                        checks += 1;
                        if checks == 2 {
                            // The second checkpoint is reached after try_lock
                            // reports WouldBlock. No timing establishes this order.
                            waiting_tx.send(()).unwrap();
                            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        }
                        if canceled.load(Ordering::SeqCst) {
                            Err("cancelled")
                        } else {
                            Ok(())
                        }
                    });
                finished_tx.send(()).unwrap();
                result
            });
            let reached_wait = waiting_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            canceled.store(true, Ordering::SeqCst);
            resume_tx.send(()).unwrap();
            let returned_while_locked = finished_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            // Always release and join before asserting, including a bad wait.
            drop(held);
            let result = worker.join().unwrap();
            assert!(
                reached_wait,
                "the contended-lock checkpoint must be reached"
            );
            assert!(
                returned_while_locked,
                "cancellation must not wait for the lock holder"
            );
            assert!(matches!(
                result,
                Err(ControlledSearchError::Control("cancelled"))
            ));
        });
        assert!(cache.table.lock().unwrap().entries.is_empty());
        assert_eq!(
            cache
                .frequency(&segment, &live, b"alpha", &[FieldId(0)])
                .unwrap(),
            2
        );
    }

    fn segment() -> Arc<SealedSegment> {
        let analyzer = Analyzer::new(Profile::Code.config()).unwrap();
        let mut builder = SegmentIndex::new();
        for fields in TEXTS {
            let mut document = Document::new();
            for (id, text) in fields.iter().enumerate() {
                document.set(FieldId(id as u16), text);
            }
            builder.push_document(&analyzer, &document).unwrap();
        }
        Arc::new(SealedSegment::seal(&builder).unwrap())
    }

    #[test]
    fn astra_17_live_df_field_union_is_exact() {
        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        for rows in [vec![0, 2, 3, 5], vec![0, 1, 2, 3, 4, 5], vec![]] {
            let segment = segment();
            let live = DocBitmap::from_ids(rows.iter().copied());
            let mut statistics = LiveSegmentStatistics::build(0, &segment, &live).unwrap();
            if !rows.is_empty() && rows.len() != TEXTS.len() {
                statistics.frequency_cache = Some(LiveFrequencyCache::new(&accounting).unwrap());
            }
            let mut index = LexicalIndex::new();
            index.push_shared_with_statistics(segment, Arc::new(statistics));
            assert_eq!(index.document_count(), rows.len() as u64);
            assert_eq!(index.total_tokens(), (rows.len() * 3) as u64);
            for fields in [
                vec![0],
                vec![1],
                vec![2],
                vec![0, 1],
                vec![0, 2],
                vec![1, 2],
                vec![0, 1, 2],
            ] {
                let field_ids = fields
                    .iter()
                    .map(|v| FieldId(*v as u16))
                    .collect::<Vec<_>>();
                let df = rows
                    .iter()
                    .filter(|row| {
                        fields
                            .iter()
                            .any(|field| TEXTS[**row as usize][*field] == "alpha")
                    })
                    .count() as u32;
                assert_eq!(
                    index.document_frequency(b"alpha", &field_ids),
                    df,
                    "uncached independent DF"
                );
                assert_eq!(
                    index
                        .prepared_document_frequency(b"alpha", &field_ids)
                        .unwrap(),
                    df
                );
                observer::begin();
                assert_eq!(
                    index
                        .prepared_document_frequency(b"alpha", &field_ids)
                        .unwrap(),
                    df
                );
                if rows.len() == 4 {
                    assert_eq!(observer::live_df_work(), observer::LiveDfWork::default());
                }
                observer::take();
            }
            if rows.is_empty() {
                assert!(
                    index.corpus_stats().is_err(),
                    "undefined empty-corpus average retained"
                );
                continue;
            }
            // Fields normalize before reaching the cache. A repeated query
            // term reuses DF while retaining the existing additive score rule.
            let query = TermQuery::flat(
                vec![b"alpha".to_vec(); 2],
                &[FieldId(1), FieldId(0), FieldId(1)],
            );
            observer::begin();
            let prepared = PreparedTermQuery::new(&index, &query, Bm25Params::default()).unwrap();
            let df = rows
                .iter()
                .filter(|row| TEXTS[**row as usize][..2].contains(&"alpha"))
                .count() as u32;
            assert_eq!(prepared.frequencies(), &[df, df]);
            assert_eq!(observer::live_df_work(), observer::LiveDfWork::default());
            observer::take();
            let answer = crate::planner::search_lexical_filtered(
                &index,
                &query,
                10,
                Bm25Params::default(),
                &[live],
                None,
            )
            .unwrap();
            let n = rows.len() as f64;
            let idf = (1.0 + (n - f64::from(df) + 0.5) / (f64::from(df) + 0.5)).ln();
            let expected_rows = rows
                .iter()
                .filter(|row| TEXTS[**row as usize][..2].contains(&"alpha"))
                .count();
            assert_eq!(answer.result.hits.len(), expected_rows);
            for hit in answer.result.hits {
                assert!(rows.contains(&hit.doc.row));
                let tf = TEXTS[hit.doc.row as usize][..2]
                    .iter()
                    .filter(|t| **t == "alpha")
                    .count() as f64;
                // All documents have three analyzed tokens; selected fields
                // contribute two length units. Two query terms add twice.
                let expected = 2.0 * idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * 2.0 / 3.0));
                assert!(
                    (hit.score - expected).abs() < 1e-12,
                    "literal BM25 {expected}, actual {}",
                    hit.score
                );
            }
        }
        assert_eq!(accounting.audit().unwrap().cache_bytes, 0);
    }

    #[test]
    fn astra_17_live_df_concurrent_misses_share_one_count() {
        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        let cache = LiveFrequencyCache::new(&accounting).unwrap();
        let segment = segment();
        let live = DocBitmap::from_ids([0, 2, 3, 5]);
        let barrier = std::sync::Barrier::new(8);
        observer::begin();
        let observation = observer::current();
        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let observation = observation.clone();
                    let (cache, segment, live, barrier) = (&cache, &segment, &live, &barrier);
                    scope.spawn(move || {
                        let _scope = observer::enter(observation);
                        barrier.wait();
                        assert_eq!(
                            cache
                                .frequency(segment, live, b"alpha", &[FieldId(0)])
                                .unwrap(),
                            2
                        );
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(observer::live_df_work().walks, 1);
        assert_eq!(observer::live_df_work().docids, 3);
        observer::take();
        assert_eq!(cache.table.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn astra_17_single_posting_queries_do_not_displace_repeated_terms() {
        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        let cache = LiveFrequencyCache::new(&accounting).unwrap();
        let analyzer = Analyzer::new(Profile::Code.config()).unwrap();
        let mut builder = SegmentIndex::new();
        for text in ["common first", "common second", "common third"] {
            builder
                .push_document(&analyzer, &Document::with_text(text))
                .unwrap();
        }
        let segment = SealedSegment::seal(&builder).unwrap();
        let live = DocBitmap::from_ids([0, 2]);
        assert_eq!(
            cache
                .frequency(&segment, &live, b"common", &[FieldId(0)])
                .unwrap(),
            2
        );
        let initial = cache.table.lock().unwrap().entries.len();
        for (term, expected) in [
            (b"first".as_slice(), 1),
            (b"second".as_slice(), 0),
            (b"absent".as_slice(), 0),
        ] {
            assert_eq!(
                cache
                    .frequency(&segment, &live, term, &[FieldId(0)])
                    .unwrap(),
                expected
            );
            assert_eq!(
                cache.table.lock().unwrap().entries.len(),
                initial,
                "one or zero postings need no cache admission or eviction"
            );
        }
        observer::begin();
        assert_eq!(
            cache
                .frequency(&segment, &live, b"common", &[FieldId(0)])
                .unwrap(),
            2
        );
        assert_eq!(observer::live_df_work(), observer::LiveDfWork::default());
        observer::take();
    }

    #[test]
    fn astra_17_live_df_eviction_and_large_keys_preserve_exact_values() {
        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        let cache = LiveFrequencyCache::new(&accounting).unwrap();
        let segment = segment();
        let live = DocBitmap::from_ids([0, 2, 3, 5]);
        let bytes = accounting.audit().unwrap().cache_bytes;
        assert!(bytes > 0);
        for term in 0..256 {
            // Field sets force both entry-capacity and byte-capacity eviction.
            // Only field zero has postings; the absent fields still identify
            // distinct exact keys and must not grow the reserved arena.
            let fields = std::iter::once(FieldId(0))
                .chain((3..3 + term % 120).map(FieldId))
                .chain(std::iter::once(FieldId(1000 + term)))
                .collect::<Vec<_>>();
            assert_eq!(
                cache.frequency(&segment, &live, b"alpha", &fields).unwrap(),
                2
            );
            assert_eq!(
                cache
                    .frequency(&segment, &live, b"alpha", &[FieldId(0), FieldId(2)])
                    .unwrap(),
                3
            );
            let table = cache.table.lock().unwrap();
            assert!(table.entries.len() <= ENTRIES && table.keys.len() <= KEY_BYTES);
            assert_eq!(accounting.audit().unwrap().cache_bytes, bytes);
        }
        let before = cache.table.lock().unwrap().entries.len();
        assert_eq!(
            cache
                .frequency(&segment, &live, &vec![b'z'; KEY_BYTES + 1], &[FieldId(0)])
                .unwrap(),
            0
        );
        assert_eq!(cache.table.lock().unwrap().entries.len(), before);
        drop(cache);
        assert_eq!(accounting.audit().unwrap().cache_bytes, 0);
    }

    #[test]
    fn astra_17_live_df_failures_are_typed_and_publish_no_partial_cache() {
        let refused = Arc::new(stats::Accounting::new(0, u64::MAX));
        assert!(matches!(
            LiveFrequencyCache::new(&refused),
            Err(StoreError::BudgetExceeded {
                component: "cache",
                ..
            })
        ));
        assert_eq!(refused.audit().unwrap().cache_bytes, 0);
        let accounting = Arc::new(stats::Accounting::new(u64::MAX, u64::MAX));
        let cache = LiveFrequencyCache::new(&accounting).unwrap();
        let poisoned = Arc::clone(&cache);
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.table.lock().unwrap();
                panic!("deliberate poisoned-cache boundary");
            })
            .join()
            .is_err()
        );
        assert!(matches!(
            cache.frequency(
                &segment(),
                &DocBitmap::from_ids([0, 2]),
                b"alpha",
                &[FieldId(0)]
            ),
            Err(IndexError::LiveFrequencyCache {
                reason: "poisoned cache lock"
            })
        ));
        drop(cache);
        assert_eq!(accounting.audit().unwrap().cache_bytes, 0);
    }
}

impl Table {
    fn matches(&self, entry: &Entry, term: &[u8], fields: &[FieldId]) -> Result<bool, IndexError> {
        if entry.term_len != term.len() || entry.fields_len != fields.len() {
            return Ok(false);
        }
        let term_end = entry
            .start
            .checked_add(entry.term_len)
            .ok_or_else(invalid)?;
        if self.keys.get(entry.start..term_end).ok_or_else(invalid)? != term {
            return Ok(false);
        }
        let end = entry
            .fields_len
            .checked_mul(2)
            .and_then(|n| term_end.checked_add(n))
            .ok_or_else(invalid)?;
        let stored = self.keys.get(term_end..end).ok_or_else(invalid)?;
        Ok(stored
            .chunks_exact(2)
            .zip(fields)
            .all(|(bytes, field)| bytes == field.0.to_le_bytes()))
    }

    fn evict(&mut self) -> Result<(), IndexError> {
        let slot = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.used)
            .map(|(slot, _)| slot)
            .ok_or_else(invalid)?;
        let entry = self.entries.get(slot).copied().ok_or_else(invalid)?;
        let len = entry
            .fields_len
            .checked_mul(2)
            .and_then(|n| n.checked_add(entry.term_len))
            .ok_or_else(invalid)?;
        let end = entry.start.checked_add(len).ok_or_else(invalid)?;
        self.keys.get(entry.start..end).ok_or_else(invalid)?;
        self.entries.remove(slot);
        self.keys.drain(entry.start..end);
        for other in &mut self.entries {
            if other.start >= end {
                other.start = other.start.checked_sub(len).ok_or_else(invalid)?;
            }
        }
        Ok(())
    }
}
