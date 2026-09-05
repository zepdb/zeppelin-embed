use crate::bundle::Bundle;

fn evaluated_tower(
    tower: &zeppelin_embed::epoch::EmbeddingTower,
) -> zeppelin_embed::epoch::EmbeddingTower {
    let mut tower = tower.clone();
    // The source checkpoint is unchanged, but earlier text runtimes copied
    // strided outputs incorrectly. Version this interpretation in the existing
    // host-selected model version so those persisted vectors cannot be reused
    // or mixed with corrected output. Bundle metadata remains the source truth.
    tower.model_version.push_str(";ze-text-output-layout=2");
    tower
}

pub(crate) fn embedding_epoch(bundle: &Bundle) -> zeppelin_embed::epoch::EmbeddingEpoch {
    zeppelin_embed::epoch::EmbeddingEpoch {
        document: evaluated_tower(&bundle.document_tower().embedding),
        query: evaluated_tower(&bundle.query_tower().embedding),
        alignment_digest: bundle.alignment_digest().to_vec(),
    }
}

pub(crate) fn document_epoch(bundle: &Bundle) -> zeppelin_embed::epoch::EmbeddingEpoch {
    let document = evaluated_tower(&bundle.document_tower().embedding);
    zeppelin_embed::epoch::EmbeddingEpoch {
        document: document.clone(),
        query: document,
        alignment_digest: Vec::new(),
    }
}
