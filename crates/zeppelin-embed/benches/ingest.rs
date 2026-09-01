use std::cell::Cell;
use std::time::Duration;

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{OpenOptions, Store};

const DIMENSIONS: usize = 64;
const PRELOADED_DOCUMENTS: usize = 2_000;
const INGEST_BATCH_SIZE: usize = 64;
const DELETE_BATCH_SIZE: usize = 32;

fn document(doc_id: u128, revision: u64, with_text: bool) -> IngestDocument {
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(doc_id), Revision::new(revision)),
        vec![1.0; DIMENSIONS],
    );
    if with_text {
        document.with_text(format!("document {doc_id}"))
    } else {
        document
    }
}

fn preloaded_store() -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("create ingest benchmark directory");
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("open ingest benchmark store");
    let documents = (0..PRELOADED_DOCUMENTS)
        .map(|index| document(index as u128, 1, false))
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("preload ingest benchmark store");
    (directory, store)
}

fn ingest_batch(iteration: u64) -> IngestBatch {
    let documents = (0..INGEST_BATCH_SIZE)
        .map(|index| {
            let (doc_id, revision) = if index < INGEST_BATCH_SIZE / 2 {
                (index as u128, iteration + 2)
            } else {
                let inserted = index - INGEST_BATCH_SIZE / 2;
                (
                    PRELOADED_DOCUMENTS as u128
                        + u128::from(iteration) * (INGEST_BATCH_SIZE / 2) as u128
                        + inserted as u128,
                    1,
                )
            };
            document(doc_id, revision, index % 2 == 0)
        })
        .collect();
    IngestBatch::new(documents)
}

fn ingest_64_documents(criterion: &mut Criterion) {
    let (_directory, store) = preloaded_store();
    let iteration = Cell::new(0_u64);
    criterion.bench_function("ingest/64_docs_preloaded_2000", |bencher| {
        bencher.iter_batched(
            || {
                let current = iteration.get();
                iteration.set(current + 1);
                ingest_batch(current)
            },
            |batch| black_box(store.ingest(batch).expect("ingest benchmark batch")),
            BatchSize::SmallInput,
        );
    });
}

fn delete_32_documents(criterion: &mut Criterion) {
    let (_directory, store) = preloaded_store();
    let iteration = Cell::new(0_usize);
    criterion.bench_function("ingest/delete_32_existing_preloaded_2000", |bencher| {
        bencher.iter_batched(
            || {
                let current = iteration.get();
                iteration.set(current + 1);
                let start = current * DELETE_BATCH_SIZE;
                let doc_ids = (0..DELETE_BATCH_SIZE)
                    .map(|offset| DocId::new(((start + offset) % PRELOADED_DOCUMENTS) as u128))
                    .collect();
                DeleteBatch::new(doc_ids)
            },
            |batch| black_box(store.delete(batch).expect("delete benchmark batch")),
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = ingest_64_documents, delete_32_documents
}
criterion_main!(benches);
