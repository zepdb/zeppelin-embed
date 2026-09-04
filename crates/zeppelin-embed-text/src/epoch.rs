use crate::bundle::Bundle;

pub(crate) fn embedding_epoch(bundle: &Bundle) -> zeppelin_embed::epoch::EmbeddingEpoch {
    zeppelin_embed::epoch::EmbeddingEpoch {
        document: bundle.document_tower().embedding.clone(),
        query: bundle.query_tower().embedding.clone(),
        alignment_digest: bundle.alignment_digest().to_vec(),
    }
}

pub(crate) fn document_epoch(bundle: &Bundle) -> zeppelin_embed::epoch::EmbeddingEpoch {
    let document = bundle.document_tower().embedding.clone();
    zeppelin_embed::epoch::EmbeddingEpoch {
        document: document.clone(),
        query: document,
        alignment_digest: Vec::new(),
    }
}
