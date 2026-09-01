use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{OpenOptions, Store};

const DIMS: usize = 64;
const PRELOADED_DOCUMENTS: u128 = 2_000;
const INGEST_DOCUMENTS: u128 = 64;
const DELETE_DOCUMENTS: u128 = 32;

struct Fixture {
    _directory: TempDir,
    store: Store,
}

fn document(id: u128, revision: u64) -> IngestDocument {
    let value = (id % 31) as f32 / 31.0;
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![value; DIMS],
    );
    if id.is_multiple_of(2) {
        document.with_text(format!("document {id}"))
    } else {
        document
    }
}

fn documents(count: u128, revision: u64) -> Vec<IngestDocument> {
    (1..=count).map(|id| document(id, revision)).collect()
}

fn fixture() -> Fixture {
    let directory = tempdir().expect("create ingest benchmark directory");
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("open ingest benchmark store");
    store
        .ingest(IngestBatch::new(documents(PRELOADED_DOCUMENTS, 1)))
        .expect("preload ingest benchmark store");
    Fixture {
        _directory: directory,
        store,
    }
}

fn ingest_64_documents(criterion: &mut Criterion) {
    let fixture = fixture();
    let mut revision = 1_u64;
    criterion.bench_function("ingest/64_documents", |bencher| {
        bencher.iter_batched(
            || {
                revision = revision.saturating_add(1);
                IngestBatch::new(documents(INGEST_DOCUMENTS, revision))
            },
            |batch| black_box(fixture.store.ingest(batch).expect("ingest benchmark batch")),
            BatchSize::SmallInput,
        );
    });
}

fn delete_32_documents(criterion: &mut Criterion) {
    let fixture = fixture();
    let ids = (1..=DELETE_DOCUMENTS).map(DocId::new).collect::<Vec<_>>();
    let mut revision = 1_u64;
    criterion.bench_function("ingest/delete_32_documents", |bencher| {
        bencher.iter_batched(
            || {
                revision = revision.saturating_add(1);
                fixture
                    .store
                    .ingest(IngestBatch::new(documents(DELETE_DOCUMENTS, revision)))
                    .expect("restore delete benchmark documents");
                DeleteBatch::new(ids.clone())
            },
            |batch| black_box(fixture.store.delete(batch).expect("delete benchmark batch")),
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(std::time::Duration::from_secs(1))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = ingest_64_documents, delete_32_documents
}
criterion_main!(benches);
