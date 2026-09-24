//! ZE-218: additive schema evolution on an existing store.
//!
//! A store opened with a declared schema that adds nullable attributes keeps
//! every persisted attribute, commits the evolved schema as one generation,
//! and reads every row written before the addition as null. Sealed segments
//! are never rewritten for it.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, DocumentFields, DocumentScanRequest, OpenOptions, QueryControl, SearchOptions,
    Store, StoreError, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{
    ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, RangeBound, RangePredicate,
    Schema,
};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

const DIMS: usize = 8;
const RANK: ColumnId = ColumnId::new(1);
const LANG: ColumnId = ColumnId::new(2);
const SCORE: ColumnId = ColumnId::new(3);

fn fixture_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "schema-evolution-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x21, 0x8a],
        dims: DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn rank() -> ColumnDefinition {
    ColumnDefinition::new(RANK, "rank", ColumnType::U64, false)
}

fn lang() -> ColumnDefinition {
    ColumnDefinition::new(LANG, "lang", ColumnType::DictionaryString, true)
}

fn score() -> ColumnDefinition {
    ColumnDefinition::new(SCORE, "score", ColumnType::I64, true)
}

/// The schema every store in this file is created with.
fn release_one_schema() -> Schema {
    Schema::new(vec![rank()]).expect("release-one schema")
}

/// The next release adds two nullable attributes.
fn release_two_schema() -> Schema {
    Schema::new(vec![rank(), lang(), score()]).expect("release-two schema")
}

fn options(schema: Schema) -> OpenOptions {
    OpenOptions::default()
        .with_epoch(fixture_epoch())
        .with_schema(schema)
}

fn fixture_vector(doc: u64) -> Vec<f32> {
    (0..DIMS)
        .map(|index| ((doc as usize * 7 + index * 3) % 17) as f32)
        .collect()
}

/// Release-one documents carry only `rank`, equal to the document id.
fn release_one_documents(ids: std::ops::RangeInclusive<u64>) -> IngestBatch {
    IngestBatch::new(
        ids.map(|doc| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(doc)), Revision::new(1)),
                fixture_vector(doc),
            )
            .with_timestamp(doc as i64)
            .with_columns(vec![(RANK, PredicateValue::U64(doc))])
        })
        .collect(),
    )
    .with_epoch(fixture_epoch().identity())
}

fn release_two_document(doc: u64, lang: Option<&str>, score: Option<i64>) -> IngestDocument {
    let mut columns = vec![(RANK, PredicateValue::U64(doc))];
    if let Some(lang) = lang {
        columns.push((LANG, PredicateValue::String(lang.to_owned())));
    }
    if let Some(score) = score {
        columns.push((SCORE, PredicateValue::I64(score)));
    }
    IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(doc)), Revision::new(1)),
        fixture_vector(doc),
    )
    .with_timestamp(doc as i64)
    .with_columns(columns)
}

fn ingest_release_two(store: &Store, documents: Vec<IngestDocument>) {
    store
        .ingest(IngestBatch::new(documents).with_epoch(fixture_epoch().identity()))
        .expect("ingest release-two rows");
}

/// Release one: docs `1..=first_sealed` sealed, the rest up to `last` left
/// in the WAL tail, then closed.
fn build_release_one_store(path: &Path, sealed: u64, last: u64) {
    let store = Store::open(path, options(release_one_schema())).expect("create release-one");
    store
        .ingest(release_one_documents(1..=sealed))
        .expect("ingest sealed rows");
    store.seal().expect("seal release-one rows");
    store
        .ingest(release_one_documents(sealed + 1..=last))
        .expect("ingest WAL tail");
    store.close().expect("close release-one store");
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn matching_ids(store: &Store, predicate: &Predicate) -> BTreeSet<u64> {
    let page = store
        .scan_documents(
            DocumentScanRequest::new(1_000, DocumentFields::NONE, control())
                .with_predicate(predicate),
        )
        .unwrap_or_else(|error| panic!("scan {predicate:?}: {error}"));
    assert!(page.continuation.is_none());
    page.documents
        .iter()
        .map(|document| u64::try_from(document.doc_id.get()).expect("small doc id"))
        .collect()
}

fn ids(values: impl IntoIterator<Item = u64>) -> BTreeSet<u64> {
    values.into_iter().collect()
}

fn lang_is(value: &str) -> Predicate {
    Predicate::Eq {
        column: LANG,
        value: PredicateValue::String(value.to_owned()),
    }
}

fn manifest_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path.join("manifest.ze")).expect("read manifest")
}

fn generation(store: &Store) -> u64 {
    store
        .count_documents(None, None)
        .expect("count documents")
        .generation
}

fn published_segments(store: &Store) -> usize {
    store.snapshot().expect("snapshot").segments().len()
}

fn schema_mismatch(result: Result<Store, StoreError>) -> String {
    match result {
        Err(error @ StoreError::SchemaMismatch { .. }) => error.to_string(),
        Err(other) => panic!("expected SchemaMismatch, got {other}"),
        Ok(_) => panic!("expected SchemaMismatch, open succeeded"),
    }
}

/// Filters over docs 1..=6 (release one: 1..=4 sealed, 5..=6 WAL tail) and
/// release-two docs 7 (en, 70), 8 (de, 80) sealed, then 9 (en, 90) and 10
/// (no lang, no score) active.
fn assert_mixed_filters(store: &Store) {
    let everything = ids(1..=10);
    assert_eq!(matching_ids(store, &lang_is("en")), ids([7, 9]));
    assert_eq!(
        matching_ids(
            store,
            &Predicate::In {
                column: LANG,
                values: vec![
                    PredicateValue::String("en".to_owned()),
                    PredicateValue::String("de".to_owned()),
                ],
            },
        ),
        ids([7, 8, 9])
    );
    assert_eq!(
        matching_ids(
            store,
            &Predicate::Range(RangePredicate {
                column: SCORE,
                lower: Some(RangeBound {
                    value: PredicateValue::I64(75),
                    inclusive: true,
                }),
                upper: None,
            }),
        ),
        ids([8, 9])
    );
    assert_eq!(
        matching_ids(store, &Predicate::Exists(LANG)),
        ids([7, 8, 9])
    );
    assert_eq!(
        matching_ids(store, &Predicate::IsNull(LANG)),
        ids([1, 2, 3, 4, 5, 6, 10])
    );
    assert_eq!(
        matching_ids(store, &Predicate::IsNull(SCORE)),
        ids([1, 2, 3, 4, 5, 6, 10])
    );
    let not_en = matching_ids(store, &Predicate::Not(Box::new(lang_is("en"))));
    assert_eq!(not_en, &everything - &ids([7, 9]));
    assert_eq!(
        matching_ids(
            store,
            &Predicate::Eq {
                column: RANK,
                value: PredicateValue::U64(3),
            },
        ),
        ids([3])
    );

    let outcome = store
        .search_filtered(
            SearchRequest::new(&fixture_vector(7)),
            &lang_is("en"),
            10,
            SearchOptions::default(),
            control(),
        )
        .expect("filtered vector search on an added attribute");
    let found = outcome
        .candidates
        .iter()
        .map(|candidate| {
            u64::try_from(
                candidate
                    .document()
                    .expect("document identity")
                    .doc_id()
                    .get(),
            )
            .expect("small doc id")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(found, ids([7, 9]));

    let fetched = store
        .get_documents(
            &[DocId::new(2), DocId::new(5), DocId::new(8)],
            DocumentFields::ATTRIBUTES,
        )
        .expect("get attributes");
    let attributes = |index: usize| {
        fetched[index]
            .as_ref()
            .expect("live document")
            .attributes
            .clone()
            .expect("attributes selected")
    };
    assert_eq!(attributes(0), vec![(RANK, PredicateValue::U64(2))]);
    assert_eq!(attributes(1), vec![(RANK, PredicateValue::U64(5))]);
    assert_eq!(
        attributes(2),
        vec![
            (RANK, PredicateValue::U64(8)),
            (LANG, PredicateValue::String("de".to_owned())),
            (SCORE, PredicateValue::I64(80)),
        ]
    );
}

fn write_release_two_rows(store: &Store) {
    ingest_release_two(
        store,
        vec![
            release_two_document(7, Some("en"), Some(70)),
            release_two_document(8, Some("de"), Some(80)),
        ],
    );
    store.seal().expect("seal release-two rows");
    ingest_release_two(
        store,
        vec![
            release_two_document(9, Some("en"), Some(90)),
            release_two_document(10, None, None),
        ],
    );
}

fn consolidate(store: &Store, graph_min_rows: u32) {
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(600),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows },
    );
    assert!(
        matches!(report.status, MaintenanceStatus::Complete),
        "maintenance status {:?}",
        report.status
    );
    assert_eq!(report.consolidations, 1, "old and new segments merge");
}

#[test]
fn an_added_nullable_attribute_reads_null_on_old_rows_and_filters_everywhere() {
    let directory = tempdir().expect("store directory");
    build_release_one_store(directory.path(), 4, 6);

    let store = Store::open(directory.path(), options(release_two_schema()))
        .expect("open release-one store with added attributes");
    assert_eq!(store.schema(), &release_two_schema());
    write_release_two_rows(&store);
    assert_eq!(published_segments(&store), 2);
    assert_mixed_filters(&store);

    store.seal().expect("seal active rows before consolidation");
    consolidate(&store, 10);
    assert_eq!(published_segments(&store), 1);
    assert_mixed_filters(&store);
    store.close().expect("close evolved store");

    let reopened = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(fixture_epoch()),
    )
    .expect("reopen without a declaration");
    assert_eq!(reopened.schema(), &release_two_schema());
    assert_mixed_filters(&reopened);
    reopened.close().expect("close reopened store");
}

#[test]
fn schema_evolution_commits_one_durable_generation() {
    let directory = tempdir().expect("store directory");
    build_release_one_store(directory.path(), 4, 6);
    let before = {
        let store = Store::open(directory.path(), options(release_one_schema()))
            .expect("reopen release one unchanged");
        let before = generation(&store);
        store.close().expect("close");
        before
    };
    let untouched = manifest_bytes(directory.path());
    let store = Store::open(directory.path(), options(release_one_schema())).expect("reopen");
    assert_eq!(
        generation(&store),
        before,
        "an equal declaration is a no-op"
    );
    store.close().expect("close");
    assert_eq!(manifest_bytes(directory.path()), untouched);

    let evolved = Store::open(directory.path(), options(release_two_schema())).expect("evolve");
    assert_eq!(generation(&evolved), before + 1);
    // Drop without close: the evolution is already committed.
    drop(evolved);

    let reordered = Schema::new(vec![score(), rank(), lang()]).expect("reordered schema");
    let store = Store::open(directory.path(), options(reordered)).expect("reordered declaration");
    assert_eq!(
        store.schema(),
        &release_two_schema(),
        "persisted order is kept"
    );
    let after = generation(&store);
    assert!(
        after > before,
        "generation {after} after evolution {before}"
    );
    store.close().expect("close");

    let read_only = Store::open(
        directory.path(),
        OpenOptions::read_only().with_epoch(fixture_epoch()),
    )
    .expect("read-only open adopts the committed schema");
    assert_eq!(read_only.schema(), &release_two_schema());
    assert_eq!(
        matching_ids(&read_only, &Predicate::IsNull(LANG)),
        ids(1..=6)
    );
    read_only.close().expect("close read-only");

    let downgrade = schema_mismatch(Store::open(directory.path(), options(release_one_schema())));
    assert!(downgrade.contains("'lang'"), "{downgrade}");
}

#[test]
fn non_additive_declarations_stay_schema_mismatch_and_name_the_attribute() {
    let directory = tempdir().expect("store directory");
    build_release_one_store(directory.path(), 2, 3);
    let persisted = Schema::new(vec![rank(), lang()]).expect("persisted schema");
    Store::open(directory.path(), options(persisted))
        .expect("add lang")
        .close()
        .expect("close");
    let untouched = manifest_bytes(directory.path());

    let cases = [
        (
            "removed",
            Schema::new(vec![lang()]).expect("schema"),
            "'rank'",
            "cannot be removed",
        ),
        (
            "type changed",
            Schema::new(vec![
                rank(),
                ColumnDefinition::new(LANG, "lang", ColumnType::RawString, true),
            ])
            .expect("schema"),
            "'lang'",
            "RawString",
        ),
        (
            "renamed",
            Schema::new(vec![
                rank(),
                ColumnDefinition::new(LANG, "language", ColumnType::DictionaryString, true),
            ])
            .expect("schema"),
            "'language'",
            "cannot change",
        ),
        (
            "nullability changed",
            Schema::new(vec![
                ColumnDefinition::new(RANK, "rank", ColumnType::U64, true),
                lang(),
            ])
            .expect("schema"),
            "'rank'",
            "cannot change",
        ),
        (
            "added not nullable",
            Schema::new(vec![
                rank(),
                lang(),
                ColumnDefinition::new(SCORE, "score", ColumnType::I64, false),
            ])
            .expect("schema"),
            "'score'",
            "must be nullable",
        ),
    ];
    for (case, declared, attribute, reason) in cases {
        let message = schema_mismatch(Store::open(directory.path(), options(declared)));
        assert!(message.contains(attribute), "{case}: {message}");
        assert!(message.contains(reason), "{case}: {message}");
        assert_eq!(
            manifest_bytes(directory.path()),
            untouched,
            "{case}: a refused declaration must not commit"
        );
    }
}

#[test]
fn a_read_only_open_refuses_an_added_attribute() {
    let directory = tempdir().expect("store directory");
    build_release_one_store(directory.path(), 2, 3);
    let untouched = manifest_bytes(directory.path());
    let message = schema_mismatch(Store::open(
        directory.path(),
        OpenOptions::read_only()
            .with_epoch(fixture_epoch())
            .with_schema(release_two_schema()),
    ));
    assert!(message.contains("'lang'"), "{message}");
    assert!(message.contains("read-only"), "{message}");
    assert_eq!(manifest_bytes(directory.path()), untouched);
}

/// A VFS that behaves like the process dying at one mutating operation:
/// that operation and every later one fail before touching the disk.
struct CrashAtVfs {
    remaining: Arc<AtomicUsize>,
    performed: Arc<AtomicUsize>,
}

impl CrashAtVfs {
    fn new(crash_at: usize) -> Self {
        Self {
            remaining: Arc::new(AtomicUsize::new(crash_at)),
            performed: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn admit(remaining: &AtomicUsize, performed: &AtomicUsize) -> std::io::Result<()> {
        remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .map(|_| {
                performed.fetch_add(1, Ordering::SeqCst);
            })
            .map_err(|_| std::io::Error::other("simulated crash"))
    }

    fn mutate(&self) -> std::io::Result<()> {
        Self::admit(&self.remaining, &self.performed)
    }
}

struct CrashAtFile {
    inner: Box<dyn VfsFile>,
    remaining: Arc<AtomicUsize>,
    performed: Arc<AtomicUsize>,
}

impl VfsFile for CrashAtFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        CrashAtVfs::admit(&self.remaining, &self.performed)?;
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        CrashAtVfs::admit(&self.remaining, &self.performed)?;
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        CrashAtVfs::admit(&self.remaining, &self.performed)?;
        self.inner.sync(kind)
    }
}

impl Vfs for CrashAtVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }
    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        self.mutate()?;
        StdVfs.create_directory(path)
    }
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }
    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        StdVfs.open_for_map(path)
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        StdVfs.read(path)
    }
    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        StdVfs.read_range(path, offset, length)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.mutate()?;
        StdVfs.write(path, bytes)
    }
    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.mutate()?;
        StdVfs.create_new(path, bytes)
    }
    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.mutate()?;
        Ok(Box::new(CrashAtFile {
            inner: StdVfs.open_append(path)?,
            remaining: Arc::clone(&self.remaining),
            performed: Arc::clone(&self.performed),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.mutate()?;
        StdVfs.rename(from, to)
    }
    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.mutate()?;
        StdVfs.sync(path, kind)
    }
    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }
    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        StdVfs.for_each_direct_child(directory, visitor)
    }
    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.mutate()?;
        StdVfs.delete(path)
    }
}

fn copy_store(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create store copy");
    for entry in std::fs::read_dir(from).expect("list store") {
        let entry = entry.expect("store entry");
        if entry.file_type().expect("entry type").is_file() && entry.file_name() != "writer.lock" {
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy store file");
        }
    }
}

/// Opens `path` with the release-two declaration through a VFS that dies at
/// mutating operation `crash_at`, returning whether the open succeeded and
/// how many mutating operations completed.
fn open_crashing_at(path: &Path, crash_at: usize) -> (bool, usize) {
    let vfs = CrashAtVfs::new(crash_at);
    let performed = Arc::clone(&vfs.performed);
    let options = options(release_two_schema()).with_durability(
        zeppelin_embed::lifecycle::durability::DurabilityMode::Durable,
        zeppelin_embed::lifecycle::durability::CommitTier::Durable,
    );
    let opened = Store::open_with_test_dependencies(
        path,
        options,
        StoreTestDependencies::new(Arc::new(vfs), Arc::new(SystemMonotonicClock)),
    );
    let succeeded = opened.is_ok();
    // A crashed process never closes; drop the handle without closing it.
    drop(opened);
    (succeeded, performed.load(Ordering::SeqCst))
}

#[test]
fn a_crash_at_any_step_of_the_evolving_open_leaves_the_old_or_the_new_schema() {
    let base = tempdir().expect("base store");
    build_release_one_store(base.path(), 4, 6);

    let probe = tempdir().expect("probe store");
    copy_store(base.path(), probe.path());
    let (succeeded, mutations) = open_crashing_at(probe.path(), usize::MAX);
    assert!(succeeded, "the uncrashed evolving open succeeds");
    assert!(mutations >= 3, "manifest commit writes, renames and syncs");

    let mut saw_old = false;
    let mut saw_new = false;
    for crash_at in 0..=mutations {
        let scratch = tempdir().expect("crash scratch");
        copy_store(base.path(), scratch.path());
        let (succeeded, _) = open_crashing_at(scratch.path(), crash_at);
        assert_eq!(succeeded, crash_at == mutations, "crash at {crash_at}");

        let adopted = Store::open(
            scratch.path(),
            OpenOptions::default().with_epoch(fixture_epoch()),
        )
        .unwrap_or_else(|error| panic!("recover after crash at {crash_at}: {error}"));
        let schema = adopted.schema().clone();
        if schema == release_one_schema() {
            saw_old = true;
        } else if schema == release_two_schema() {
            saw_new = true;
        } else {
            panic!("crash at {crash_at} left schema {schema:?}");
        }
        adopted.close().expect("close adopted");

        let evolved = Store::open(scratch.path(), options(release_two_schema()))
            .unwrap_or_else(|error| panic!("evolve after crash at {crash_at}: {error}"));
        assert_eq!(matching_ids(&evolved, &Predicate::IsNull(LANG)), ids(1..=6));
        assert_eq!(matching_ids(&evolved, &Predicate::Exists(RANK)), ids(1..=6));
        evolved.close().expect("close evolved");
    }
    assert!(saw_old && saw_new, "the matrix spans the commit point");
}

/// The checked-in fixture: written by the v0.4.2 release core with the
/// emitter below (`emit_schema_evolution_fixture`), run at tag `v0.4.2`.
/// Docs 1..=8 are sealed, docs 9..=12 are the unsealed WAL tail, and every
/// document carries only `rank = id`.
fn v0_4_2_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema-v0.4.2")
}

#[test]
fn a_store_written_by_v0_4_2_opens_with_an_added_attribute_and_filters_on_it() {
    let directory = tempdir().expect("fixture copy");
    copy_store(&v0_4_2_fixture(), directory.path());

    let store = Store::open(directory.path(), options(release_two_schema()))
        .expect("open the v0.4.2 store with added attributes");
    assert_eq!(store.schema(), &release_two_schema());
    assert_eq!(matching_ids(&store, &Predicate::IsNull(LANG)), ids(1..=12));
    ingest_release_two(
        &store,
        (13..=16)
            .map(|doc| {
                release_two_document(doc, Some(if doc % 2 == 0 { "en" } else { "de" }), None)
            })
            .collect(),
    );
    store.seal().expect("seal release-two rows");
    ingest_release_two(&store, vec![release_two_document(17, Some("en"), Some(5))]);

    let check = |store: &Store| {
        assert_eq!(matching_ids(store, &lang_is("en")), ids([14, 16, 17]));
        assert_eq!(matching_ids(store, &Predicate::IsNull(LANG)), ids(1..=12));
        assert_eq!(matching_ids(store, &Predicate::Exists(SCORE)), ids([17]));
        assert_eq!(
            matching_ids(
                store,
                &Predicate::Range(RangePredicate {
                    column: RANK,
                    lower: Some(RangeBound {
                        value: PredicateValue::U64(7),
                        inclusive: true,
                    }),
                    upper: Some(RangeBound {
                        value: PredicateValue::U64(9),
                        inclusive: true,
                    }),
                }),
            ),
            ids([7, 8, 9])
        );
    };
    check(&store);
    store.seal().expect("seal before consolidation");
    consolidate(&store, 17);
    assert_eq!(published_segments(&store), 1);
    check(&store);
    store.close().expect("close");

    let reopened = Store::open(directory.path(), options(release_two_schema()))
        .expect("reopen the evolved fixture");
    check(&reopened);
    reopened.close().expect("close reopened fixture");
}

/// Regenerates the fixture. Run at tag `v0.4.2` (where this file does not
/// exist, so copy this function there) with `ZE_SCHEMA_FIXTURE_OUT` naming
/// an empty directory, then copy `manifest.ze`, `wal.ze`, and the segment.
#[test]
#[ignore = "fixture emitter; run deliberately at the release tag"]
fn emit_schema_evolution_fixture() {
    let out = std::env::var("ZE_SCHEMA_FIXTURE_OUT").expect("ZE_SCHEMA_FIXTURE_OUT");
    build_release_one_store(Path::new(&out), 8, 12);
}
