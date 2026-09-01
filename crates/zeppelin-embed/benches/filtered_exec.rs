use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::meta::{
    Predicate, PredicateValue, RangeBound, RangePredicate, TIMESTAMP_COLUMN,
};

const ROWS: usize = 5_000;
const DIMENSIONS: usize = 64;
const TOP_K: usize = 10;

struct Fixture {
    _directory: TempDir,
    store: Store,
    query: Vec<f32>,
    predicate: Predicate,
    cancellation: CancelToken,
}

fn build_fixture() -> Fixture {
    let directory = tempdir().expect("filtered executor benchmark directory");
    let store = Store::open(directory.path(), OpenOptions::default())
        .expect("open filtered executor benchmark store");
    let documents = (0..ROWS)
        .map(|row| {
            let value = (row % 97) as f32 / 97.0;
            IngestDocument::new(
                DocumentVersion::new(DocId::new((row + 1) as u128), Revision::new(1)),
                vec![value; DIMENSIONS],
            )
            .with_timestamp(row as i64)
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest filtered executor benchmark rows");
    store.seal().expect("seal filtered executor benchmark rows");
    Fixture {
        _directory: directory,
        store,
        query: vec![0.25; DIMENSIONS],
        predicate: Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::I64(0))),
            upper: Some(RangeBound::exclusive(PredicateValue::I64(
                (ROWS / 2) as i64,
            ))),
        }),
        cancellation: CancelToken::new(),
    }
}

fn filtered_executor(criterion: &mut Criterion) {
    let fixture = build_fixture();
    let options = SearchOptions::default().with_tier(SearchTier::Scan);
    criterion.bench_function(
        "planner/execute_pinned/sealed_scan_5k_64d_range_50pct",
        |bencher| {
            bencher.iter(|| {
                black_box(
                    fixture
                        .store
                        .search_filtered(
                            SearchRequest::new(black_box(&fixture.query)),
                            black_box(&fixture.predicate),
                            TOP_K,
                            options,
                            QueryControl::Cancel(fixture.cancellation.clone()),
                        )
                        .expect("run filtered executor benchmark query"),
                );
            });
        },
    );
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(std::time::Duration::from_secs(3))
        .measurement_time(std::time::Duration::from_secs(7));
    targets = filtered_executor
}
criterion_main!(benches);
