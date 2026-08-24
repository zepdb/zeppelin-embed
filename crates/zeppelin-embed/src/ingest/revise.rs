//! Idempotent document-revision admission.

use crate::wal::LogSeq;

use super::{DocumentVersion, Revision};

/// Row-location-independent revision comparison used across active and sealed state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RevisionDecision {
    Insert,
    Replay { seq: LogSeq },
    Replace,
    Reject { current: Revision },
}

pub(crate) fn classify_revision(
    existing: Option<(DocumentVersion, LogSeq)>,
    attempted: DocumentVersion,
) -> RevisionDecision {
    let Some((current, seq)) = existing else {
        return RevisionDecision::Insert;
    };
    match attempted.revision().cmp(&current.revision()) {
        std::cmp::Ordering::Less => RevisionDecision::Reject {
            current: current.revision(),
        },
        std::cmp::Ordering::Equal => RevisionDecision::Replay { seq },
        std::cmp::Ordering::Greater => RevisionDecision::Replace,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;
    use tempfile::tempdir;

    use crate::ingest::{DocId, DocumentVersion, Revision};
    use crate::ingest::{IngestBatch, IngestDocument, SearchRequest};
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use crate::scan::ScanOptions;
    use crate::wal::LogSeq;

    use super::{RevisionDecision, classify_revision};

    #[test]
    fn revision_order_is_total_and_explicit() {
        let id = DocId::new(7);
        let current = DocumentVersion::new(id, Revision::new(4));
        let existing = Some((current, LogSeq::new(11)));

        assert_eq!(
            classify_revision(existing, DocumentVersion::new(id, Revision::new(3))),
            RevisionDecision::Reject {
                current: Revision::new(4)
            }
        );
        assert_eq!(
            classify_revision(existing, current),
            RevisionDecision::Replay {
                seq: LogSeq::new(11)
            }
        );
        assert_eq!(
            classify_revision(existing, DocumentVersion::new(id, Revision::new(5))),
            RevisionDecision::Replace
        );
        assert_eq!(classify_revision(None, current), RevisionDecision::Insert);
    }

    #[test]
    fn prop_interleaved_ingest_and_search_never_misses_or_returns_superseded() {
        let name = "ingest::revise::tests::prop_interleaved_ingest_and_search_never_misses_or_returns_superseded";
        let mut seeded = crate::test_support::seeded_rng(name);
        let mut runner = TestRunner::new(Config {
            cases: 32,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let operations = prop::collection::vec((0_u8..4, 1_u8..=4), 1..=24);
        let result = runner.run(&operations, |operations| {
            let directory = tempdir().expect("store directory");
            let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
            let mut expected = BTreeMap::<DocId, Revision>::new();
            for (doc_slot, increment) in operations {
                let doc_id = DocId::new(u128::from(doc_slot).saturating_add(1));
                let current = expected.get(&doc_id).copied().unwrap_or(Revision::new(0));
                let revision = Revision::new(
                    current
                        .get()
                        .checked_add(u64::from(increment))
                        .expect("bounded generated revision"),
                );
                let version = DocumentVersion::new(doc_id, revision);
                let ack = store
                    .ingest(IngestBatch::new(vec![IngestDocument::new(
                        version,
                        vec![f32::from(doc_slot) + 1.0, 1.0],
                    )]))
                    .expect("generated monotonic ingest");
                expected.insert(doc_id, revision);
                prop_assert_eq!(
                    ack.generation(),
                    store.snapshot().expect("acked snapshot").generation()
                );

                let query = [1.0_f32, 0.0];
                let outcome = store
                    .search(
                        SearchRequest::new(&query),
                        expected.len(),
                        ScanOptions { thread_budget: 1 },
                        QueryControl::Cancel(CancelToken::new()),
                    )
                    .expect("search acked state");
                let returned = outcome
                    .candidates
                    .iter()
                    .filter_map(|candidate| candidate.document())
                    .collect::<BTreeSet<_>>();
                let current_versions = expected
                    .iter()
                    .map(|(id, revision)| DocumentVersion::new(*id, *revision))
                    .collect::<BTreeSet<_>>();
                prop_assert_eq!(returned, current_versions);
            }
            Ok(())
        });
        result.expect("generated interleaving preserves visibility");
    }
}
