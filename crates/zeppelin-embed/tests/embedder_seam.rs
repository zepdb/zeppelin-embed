#![allow(clippy::expect_used)]

use zeppelin_embed::epoch::{Embedder, EmbedderError, EmbedderFailure, EmbedderTimeout};
use zeppelin_embed::ingest::{DocId, Revision};

#[derive(Clone, Copy)]
enum DeterministicOutcome {
    Vector,
    Failure,
    Timeout,
}

struct DeterministicEmbedder {
    outcome: DeterministicOutcome,
}

impl DeterministicEmbedder {
    const fn returning(outcome: DeterministicOutcome) -> Self {
        Self { outcome }
    }
}

impl Embedder for DeterministicEmbedder {
    fn embed(
        &mut self,
        doc_id: DocId,
        revision: Revision,
        source: &[u8],
    ) -> Result<Vec<f32>, EmbedderError> {
        let partial = vec![
            doc_id.get() as f32,
            revision.get() as f32,
            source.len() as f32,
        ];
        match self.outcome {
            DeterministicOutcome::Vector => Ok(partial),
            DeterministicOutcome::Failure => {
                Err(EmbedderFailure::new("delegate rejected input").into())
            }
            DeterministicOutcome::Timeout => {
                Err(EmbedderTimeout::new("delegate deadline elapsed").into())
            }
        }
    }
}

fn publish_only_success(
    embedder: &mut dyn Embedder,
    published: &mut Vec<Vec<f32>>,
) -> Result<(), EmbedderError> {
    let vector = embedder.embed(DocId::new(7), Revision::new(3), b"source bytes")?;
    published.push(vector);
    Ok(())
}

#[test]
fn a_delegate_failure_or_timeout_pauses_migration_with_typed_status_and_never_half_publishes() {
    let mut published = Vec::new();
    let mut failure = DeterministicEmbedder::returning(DeterministicOutcome::Failure);
    let error = publish_only_success(&mut failure, &mut published).expect_err("typed failure");
    assert!(matches!(error, EmbedderError::Failure(_)));
    assert!(published.is_empty(), "failure exposed a partial vector");

    let mut timeout = DeterministicEmbedder::returning(DeterministicOutcome::Timeout);
    let error = publish_only_success(&mut timeout, &mut published).expect_err("typed timeout");
    assert!(matches!(error, EmbedderError::Timeout(_)));
    assert!(published.is_empty(), "timeout exposed a partial vector");

    let mut success = DeterministicEmbedder::returning(DeterministicOutcome::Vector);
    publish_only_success(&mut success, &mut published).expect("complete vector");
    assert_eq!(published, vec![vec![7.0, 3.0, 12.0]]);
}
