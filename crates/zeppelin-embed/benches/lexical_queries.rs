#![allow(clippy::expect_used)]

use std::process::Command;
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tempfile::TempDir;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::query::LexicalQuery;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};

const DOCUMENTS: usize = 500;
const SEGMENT_SIZES: [usize; 3] = [167, 167, 166];
const LOAD_LIMIT: f64 = 1.0;

struct Fixture {
    store: Store,
    _directory: TempDir,
}

impl Fixture {
    fn build() -> Self {
        let directory = tempfile::tempdir().expect("lexical benchmark directory");
        let store = Store::open(directory.path(), OpenOptions::default())
            .expect("open lexical benchmark store");
        let mut first = 0_usize;
        for segment_size in SEGMENT_SIZES {
            let documents = (first..first + segment_size)
                .map(|row| {
                    let text = if row % 5 == 0 {
                        "zeppelin common alpha"
                    } else {
                        "airship common beta"
                    };
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128), Revision::new(1)),
                        vec![1.0, 0.0],
                    )
                    .with_text(text)
                })
                .collect();
            store
                .ingest(IngestBatch::new(documents))
                .expect("ingest lexical benchmark segment");
            store.seal().expect("seal lexical benchmark segment");
            first += segment_size;
        }
        assert_eq!(first, DOCUMENTS);
        Self {
            store,
            _directory: directory,
        }
    }
}

fn load_average() -> Option<f64> {
    let output = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout).ok()?;
        return text
            .trim_matches(|character: char| character.is_whitespace() || character == '{')
            .split_whitespace()
            .next()?
            .parse()
            .ok();
    }
    let uptime = Command::new("uptime").output().ok()?;
    let text = std::str::from_utf8(&uptime.stdout).ok()?;
    text.rsplit_once("load averages:")?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn print_load_taint() {
    match load_average() {
        Some(load) => println!(
            "LOAD_TAINT load1={load:.2} limit={LOAD_LIMIT:.2} status={}",
            if load > LOAD_LIMIT {
                "tainted"
            } else {
                "clean"
            }
        ),
        None => println!("LOAD_TAINT load1=unreadable limit={LOAD_LIMIT:.2} status=tainted"),
    }
}

fn lexical_queries(criterion: &mut Criterion) {
    let fixture = Fixture::build();
    let control = CancelToken::new();
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let structured = LexicalQuery::prefix(b"zep".to_vec(), DEFAULT_FIELD);
    print_load_taint();

    let mut group = criterion.benchmark_group("lexical_queries/500_docs/3_segments");
    group.bench_function("search_lexical", |bencher| {
        bencher.iter(|| {
            black_box(
                fixture
                    .store
                    .search_lexical(black_box(&term), 10, QueryControl::Cancel(control.clone()))
                    .expect("search_lexical benchmark query"),
            );
        });
    });
    group.bench_function("search_lexical_structured", |bencher| {
        bencher.iter(|| {
            black_box(
                fixture
                    .store
                    .search_lexical_structured(
                        black_box(&structured),
                        10,
                        64,
                        QueryControl::Cancel(control.clone()),
                    )
                    .expect("search_lexical_structured benchmark query"),
            );
        });
    });
    group.finish();
    fixture
        .store
        .close()
        .expect("close lexical benchmark store");
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = lexical_queries
}
criterion_main!(benches);
