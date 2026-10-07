//! Independent visibility model; exact successful VFS prefixes model process stops,
//! not torn sectors or physical power loss (covered by the existing crash matrix).
use super::*;
use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::lifecycle::{AccessMode, DocumentFields};
use crate::vfs::crash::{CrashOperation, MemoryVfs, RecordingVfs as ByteRecorder, replay_prefix};
use rand::Rng;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Model {
    documents: BTreeSet<u128>,
    nodes: BTreeSet<NodeId>,
    // Keep every acknowledgement, including no-op generations.
    generations: Vec<u64>,
}

impl Model {
    fn acknowledge(&mut self, generation: u64) {
        self.generations.push(generation);
    }

    fn generation(&self) -> u64 {
        self.generations.iter().copied().max().unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Ingest,
    Batch,
    Seal,
    DeleteSealed,
    Purge,
    Reindex,
    Schema,
    Retention,
    Graph,
    Mixed,
    Checkpoint,
    Maintenance,
    Namespace,
}

const OPERATIONS: [Operation; 13] = [
    Operation::Ingest,
    Operation::Batch,
    Operation::Seal,
    Operation::DeleteSealed,
    Operation::Purge,
    Operation::Reindex,
    Operation::Schema,
    Operation::Retention,
    Operation::Graph,
    Operation::Mixed,
    Operation::Checkpoint,
    Operation::Maintenance,
    Operation::Namespace,
];

struct Step {
    start: usize,
    end: usize,
    before: Model,
    after: Model,
}

fn document(id: u128) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(1)),
        vec![1.0, 0.0],
    )
    .with_timestamp(id as i64)
    .with_text("restart model document")
}

fn write_node(store: &Store, key: &str) -> (NodeId, u64) {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "model", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    match receipts[0].entity {
        EntityId::Node(node) => (node, receipts[0].generation.get()),
        EntityId::Relationship(_) => panic!("expected model node"),
    }
}

fn open(path: &Path, vfs: &Arc<SequenceVfs>, options: OpenOptions) -> Store {
    let store = Store::open_with_infrastructure(
        path,
        options,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    if store.admit_native_read().is_ok() {
        disable_generation_fixture_maintenance(&store);
    }
    store
}

// The mixed writer API is not present at this HEAD. This one-shot adapter uses
// its existing op-11 wire seam: replace one real, valid graph envelope append
// with a document/document/graph run. Close immediately after the graph receipt;
// only recovery consumes the changed WAL sequence count. The byte recorder is
// below the adapter, so every crash cut contains exactly the bytes written.
struct SequenceVfs {
    recorded: ByteRecorder<StdVfs>,
    mixed: Arc<std::sync::Mutex<Option<Vec<u128>>>>,
}

impl SequenceVfs {
    fn new() -> Self {
        Self {
            recorded: ByteRecorder::new(StdVfs),
            mixed: Arc::new(std::sync::Mutex::new(None)),
        }
    }
    fn operations(&self) -> std::io::Result<Vec<CrashOperation>> {
        self.recorded.operations()
    }
}

impl Vfs for SequenceVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.recorded.ensure_directory(path, create)
    }
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.recorded.open(path)
    }
    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.recorded.open_for_map(path)
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.recorded.read(path)
    }
    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.recorded.read_range(path, offset, length)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.recorded.write(path, bytes)
    }
    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.recorded.create_new(path, bytes)
    }
    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn crate::vfs::VfsFile>> {
        let file = self.recorded.open_append(path)?;
        if path.file_name().is_some_and(|name| name == "wal.ze") {
            Ok(Box::new(MixedFile {
                file,
                mixed: self.mixed.clone(),
            }))
        } else {
            Ok(file)
        }
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.recorded.rename(from, to)
    }
    fn sync(&self, path: &Path, kind: crate::vfs::SyncKind) -> std::io::Result<()> {
        self.recorded.sync(path, kind)
    }
    fn list(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.recorded.list(path)
    }
    fn for_each_direct_child(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.recorded.for_each_direct_child(path, visitor)
    }
    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.recorded.delete(path)
    }
}

struct MixedFile {
    file: Box<dyn crate::vfs::VfsFile>,
    mixed: Arc<std::sync::Mutex<Option<Vec<u128>>>>,
}

impl crate::vfs::VfsFile for MixedFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(ids) = self.mixed.lock().unwrap().take() else {
            return self.file.append(bytes);
        };
        use crate::ingest::wal_payload::{
            GRAPH_COMMIT_V1, MIXED_BATCH_MEMBER_V1, UPSERT_V2, encode_mixed_batch_member,
            encode_upsert_v2,
        };
        use crate::wal::record::{WalRecord, decode_record, encode_record};
        let graph = decode_record(bytes).unwrap();
        assert_eq!(graph.record.op, GRAPH_COMMIT_V1);
        assert_eq!(graph.encoded_len, bytes.len());
        let mut members: Vec<_> = ids
            .iter()
            .map(|id| (UPSERT_V2, encode_upsert_v2(&document(*id)).unwrap()))
            .collect();
        members.push((GRAPH_COMMIT_V1, graph.record.payload.to_vec()));
        let mut run = Vec::new();
        for (index, (op, payload)) in members.iter().enumerate() {
            let payload =
                encode_mixed_batch_member(index as u32, members.len() as u32, *op, payload)
                    .unwrap();
            run.extend(
                encode_record(WalRecord {
                    seq: crate::wal::LogSeq::new(graph.record.seq.get() + index as u64),
                    op: MIXED_BATCH_MEMBER_V1,
                    payload: &payload,
                })
                .unwrap(),
            );
        }
        self.file.append(&run)
    }
    fn append_vectored(&mut self, buffers: &mut [std::io::IoSlice<'_>]) -> std::io::Result<()> {
        let bytes: Vec<_> = buffers
            .iter()
            .flat_map(|buffer| buffer.iter().copied())
            .collect();
        self.append(&bytes)
    }
    fn sync(&self, kind: crate::vfs::SyncKind) -> std::io::Result<()> {
        self.file.sync(kind)
    }
}

fn materialize(image: &MemoryVfs, root: &Path) {
    for (file, contents) in image.files().unwrap() {
        assert!(file.starts_with(root));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, contents).unwrap();
    }
}

fn clear_files(directory: &Path) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            clear_files(&entry.path());
        } else if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "lock")
        {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
}

struct ObserveNodes;

impl super::super::super::NativeReadConsumer<BTreeSet<NodeId>> for ObserveNodes {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<BTreeSet<NodeId>, TreeError> {
        let mut cursor =
            view.node_cursor(crate::property_graph::storage::LabelSelection::All, runtime)?;
        let mut output = [NodeId::new(1).unwrap(); 16];
        let mut nodes = BTreeSet::new();
        loop {
            let (count, state) = view.scan_nodes(&mut cursor, &mut output, runtime)?;
            nodes.extend(output[..count].iter().copied());
            if state == CursorState::Done {
                break;
            }
        }
        Ok(nodes)
    }
}

fn visible(store: &Store, documents: &BTreeSet<u128>) -> Model {
    let ids: Vec<_> = documents.iter().map(|id| DocId::new(*id)).collect();
    let found = store.get_documents(&ids, DocumentFields::NONE).unwrap();
    let live = ids
        .iter()
        .zip(found)
        .filter_map(|(id, row)| row.map(|_| id.get()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        store.count_documents(None, None).unwrap().count,
        live.len() as u64
    );
    let live_nodes = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveNodes,
        )
        .unwrap();
    Model {
        documents: live,
        nodes: live_nodes,
        generations: Vec::new(),
    }
}

fn same_visibility(actual: &Model, expected: &Model) -> bool {
    actual.documents == expected.documents && actual.nodes == expected.nodes
}

pub(super) fn run() {
    let seed = std::env::var("ZE_TEST_SEED").unwrap_or_else(|_| "0".into());
    let mut rng = crate::test_support::seeded_rng(
        "recovery::random_operation_sequences_reopen_to_the_model_state",
    );
    let sequences = std::env::var("ZE_RESTART_SEQUENCES")
        .map(|n| n.parse::<usize>().expect("positive ZE_RESTART_SEQUENCES"))
        .unwrap_or(8);
    assert!(sequences > 0);
    let started = std::time::Instant::now();
    let mut cuts_run = 0;
    let mut operation_counts = [0_usize; OPERATIONS.len()];
    for sequence in 0..sequences {
        let length = rng.random_range(6..=12);
        let mut operations = Vec::new();
        // Catch only to add reproducibility context, then fail the entire run.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let directory = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(directory.path()).unwrap();
            let path = root.as_path().join("alpha");
            let vfs = Arc::new(SequenceVfs::new());
            let mut store = Some(open(&path, &vfs, native_options()));
            let mut model = Model::default();
            let mut sealed = BTreeSet::new();
            let mut all_documents = BTreeSet::new();
            let mut next_document = 91_u128;
            // Two sealed rows guarantee real sealed deletion/retention targets.
            for _ in 0..2 {
                let id = next_document;
                next_document += 1;
                let ack = store
                    .as_ref()
                    .unwrap()
                    .ingest(IngestBatch::new(vec![document(id)]))
                    .unwrap();
                model.documents.insert(id);
                all_documents.insert(id);
                model.acknowledge(ack.generation());
                model.acknowledge(store.as_ref().unwrap().seal().unwrap());
                sealed.insert(id);
            }
            model.acknowledge(store.as_ref().unwrap().enable_graph().unwrap());
            disable_generation_fixture_maintenance(store.as_ref().unwrap());
            let (node, generation) = write_node(store.as_ref().unwrap(), "initial");
            model.nodes.insert(node);
            model.acknowledge(generation);
            let mut steps = Vec::new();
            let mut graph_unfolded = true;
            let mut graph_in_document_wal = true;
            for index in 0..length {
                let active = model.documents.difference(&sealed).next().is_some();
                // Generate admitted operation orders, respecting HEAD's explicit
                // T5/physical-purge guards. No executed failure is discarded or
                // retried: any unexpected rejection fails with the entire history.
                let eligible: Vec<_> = OPERATIONS
                    .iter()
                    .copied()
                    .filter(|operation| match operation {
                        Operation::Seal | Operation::Reindex => !active || !graph_unfolded,
                        Operation::Purge => !graph_in_document_wal && !model.documents.is_empty(),
                        Operation::DeleteSealed | Operation::Retention => !sealed.is_empty(),
                        _ => true,
                    })
                    .collect();
                // Cover each requested kind before unrestricted sampling. This
                // changes selection weights, never a failing executed history.
                let unseen: Vec<_> = eligible
                    .iter()
                    .copied()
                    .filter(|operation| operation_counts[*operation as usize] == 0)
                    .collect();
                let choices = if unseen.is_empty() {
                    &eligible
                } else {
                    &unseen
                };
                let operation = choices[rng.random_range(0..choices.len())];
                operations.push(operation);
                operation_counts[operation as usize] += 1;
                let operation = &operation;
                let start = vfs.operations().unwrap().len();
                let before = model.clone();
                let s = store.as_ref().unwrap();
                let generation = match operation {
                    Operation::Ingest | Operation::Batch => {
                        let count = if matches!(operation, Operation::Batch) {
                            rng.random_range(2..=4)
                        } else {
                            1
                        };
                        let ids: Vec<_> = (next_document..next_document + count).collect();
                        next_document += count;
                        let ack = s
                            .ingest(IngestBatch::new(
                                ids.iter().map(|id| document(*id)).collect(),
                            ))
                            .unwrap();
                        model.documents.extend(&ids);
                        all_documents.extend(ids);
                        ack.generation()
                    }
                    Operation::Seal => {
                        let generation = s.seal().unwrap();
                        sealed = model.documents.clone();
                        if active {
                            graph_unfolded = false;
                            graph_in_document_wal = false;
                        }
                        generation
                    }
                    Operation::DeleteSealed => {
                        let id = *sealed.iter().next().unwrap();
                        let ack = s.delete(DeleteBatch::new(vec![DocId::new(id)])).unwrap();
                        model.documents.remove(&id);
                        sealed.remove(&id);
                        ack.generation()
                    }
                    Operation::Purge => {
                        let id = model.documents.iter().next().copied().unwrap_or(91);
                        let token = s.purge(&[DocId::new(id)]).unwrap();
                        let generation = s.await_physical_purge(token).unwrap().generation();
                        model.documents.remove(&id);
                        sealed.remove(&id);
                        generation
                    }
                    Operation::Reindex => {
                        let generation = s.reindex_text().unwrap();
                        sealed = model.documents.clone();
                        if active {
                            graph_unfolded = false;
                            graph_in_document_wal = false;
                        }
                        generation
                    }
                    Operation::Schema => {
                        use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
                        let mut columns = s
                            .schema()
                            .columns()
                            .iter()
                            .filter(|column| column.id() != crate::meta::TIMESTAMP_COLUMN)
                            .cloned()
                            .collect::<Vec<_>>();
                        columns.push(ColumnDefinition::new(
                            ColumnId::new(71 + index as u32),
                            format!("extra-{index}"),
                            ColumnType::U64,
                            true,
                        ));
                        let schema = Schema::new(columns).unwrap();
                        drop(store.take());
                        store = Some(open(&path, &vfs, native_options().with_schema(schema)));
                        store.as_ref().unwrap().snapshot().unwrap().generation()
                    }
                    Operation::Retention => {
                        // Retention removes whole sealed segments, not individual
                        // timestamps. This range contains every sealed segment;
                        // active documents remain visible.
                        let ack = s.drop_partition(0..next_document as i64).unwrap();
                        model.documents.retain(|id| !sealed.contains(id));
                        sealed.clear();
                        ack.generation()
                    }
                    Operation::Graph => {
                        let (node, generation) = write_node(s, &format!("sequence-{index}"));
                        model.nodes.insert(node);
                        graph_unfolded = true;
                        graph_in_document_wal = true;
                        generation
                    }
                    Operation::Mixed => {
                        let ids = [next_document, next_document + 1];
                        next_document += 2;
                        assert!(vfs.mixed.lock().unwrap().replace(ids.to_vec()).is_none());
                        let (node, generation) = write_node(s, &format!("mixed-{index}"));
                        assert!(vfs.mixed.lock().unwrap().is_none());
                        drop(store.take());
                        model.nodes.insert(node);
                        graph_unfolded = true;
                        graph_in_document_wal = true;
                        model.documents.extend(ids);
                        all_documents.extend(ids);
                        store = Some(open(&path, &vfs, native_options()));
                        assert!(
                            store.as_ref().unwrap().snapshot().unwrap().generation() >= generation
                        );
                        generation
                    }
                    Operation::Checkpoint => {
                        s.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                            .unwrap();
                        graph_unfolded = false;
                        if !active {
                            graph_in_document_wal = false;
                        }
                        s.snapshot().unwrap().generation()
                    }
                    Operation::Maintenance => {
                        let report = s
                            .maintain_native_graph_step(&QueryControl::Cancel(CancelToken::new()))
                            .unwrap();
                        model.acknowledge(report.generation.get());
                        graph_unfolded = true;
                        graph_in_document_wal = true;
                        s.snapshot().unwrap().generation()
                    }
                    Operation::Namespace => {
                        use crate::lifecycle::{LiveNamespaceMutation, NamespaceMutation};
                        let other = open(&root.as_path().join("beta"), &vfs, native_options());
                        let id = next_document;
                        next_document += 1;
                        let generations = crate::lifecycle::namespace_batch_live_on_vfs(
                            root.as_path(),
                            vec![
                                LiveNamespaceMutation {
                                    store: s,
                                    mutation: NamespaceMutation {
                                        name: "alpha".into(),
                                        options: native_options(),
                                        upserts: vec![document(id)],
                                        deletes: Vec::new(),
                                        delete_where: None,
                                    },
                                },
                                LiveNamespaceMutation {
                                    store: &other,
                                    mutation: NamespaceMutation {
                                        name: "beta".into(),
                                        options: native_options(),
                                        upserts: Vec::new(),
                                        deletes: Vec::new(),
                                        delete_where: None,
                                    },
                                },
                            ],
                            vfs.as_ref(),
                        )
                        .unwrap();
                        model.documents.insert(id);
                        all_documents.insert(id);
                        generations[0]
                    }
                };
                model.acknowledge(generation);
                let actual = visible(store.as_ref().unwrap(), &all_documents);
                assert!(
                    same_visibility(&actual, &model),
                    "operation {index} {operation:?}: actual={actual:?}; expected={model:?}"
                );
                steps.push(Step {
                    start,
                    end: vfs.operations().unwrap().len(),
                    before,
                    after: model.clone(),
                });
            }
            let bytes = vfs.operations().unwrap();
            drop(store);
            let mut cuts = BTreeSet::from([bytes.len()]);
            for step in steps.iter().rev().take(2) {
                cuts.insert(step.start);
                cuts.insert(step.end);
                for (index, operation) in bytes.iter().enumerate().take(step.end).skip(step.start) {
                    if matches!(
                        operation,
                        CrashOperation::Append { .. }
                            | CrashOperation::Sync { .. }
                            | CrashOperation::Rename { .. }
                    ) {
                        cuts.insert(index + 1);
                    }
                }
            }
            for cut in cuts {
                let step = steps.iter().rev().find(|step| step.start <= cut).unwrap();
                let acknowledged = if cut >= step.end {
                    &step.after
                } else {
                    &step.before
                };
                let image = replay_prefix(&MemoryVfs::new(), &bytes, cut).unwrap();
                // Namespace bindings use directory identities at this HEAD.
                // Restore files in place, preserving directory/lock identities.
                clear_files(root.as_path());
                materialize(&image, root.as_path());
                let crash_path = root.as_path().join("alpha");
                let mut generation = acknowledged.generation();
                let mut recovered_visibility = None;
                let mut next_node = None;
                for (phase, access) in [
                    AccessMode::ReadWrite,
                    AccessMode::ReadOnly,
                    AccessMode::ReadWrite,
                ]
                .into_iter()
                .enumerate()
                {
                    let recovered =
                        Store::open(&crash_path, native_options().with_access_mode(access))
                            .unwrap_or_else(|error| {
                                panic!("cut={cut}, access={access:?}: {error}")
                            });
                    let current = recovered.snapshot().unwrap().generation();
                    assert!(
                        current >= generation,
                        "cut={cut}: generation {generation} -> {current}"
                    );
                    generation = current;
                    let actual = visible(&recovered, &all_documents);
                    if cut >= step.end {
                        assert!(
                            same_visibility(&actual, acknowledged),
                            "cut={cut}: {actual:?} != {acknowledged:?}"
                        );
                    } else {
                        // The interrupted operation was never acknowledged. Its
                        // complete old or new state is allowed, never a partial batch.
                        assert!(
                            same_visibility(&actual, &step.before)
                                || same_visibility(&actual, &step.after),
                            "cut={cut}: partial operation state {actual:?}; before={:?}; after={:?}",
                            step.before,
                            step.after
                        );
                    }
                    if let Some(previous) = &recovered_visibility {
                        assert!(
                            same_visibility(&actual, previous),
                            "cut={cut}: restart changed visibility"
                        );
                    }
                    recovered_visibility = Some(actual);
                    if phase == 2 {
                        disable_generation_fixture_maintenance(&recovered);
                        let (node, acknowledged_generation) =
                            write_node(&recovered, "after-restarts");
                        assert!(acknowledged_generation > generation);
                        generation = acknowledged_generation;
                        next_node = Some(node);
                    }
                    drop(recovered);
                }
                let next = next_node.unwrap();
                let final_open = Store::open(&crash_path, native_options()).unwrap();
                assert!(final_open.snapshot().unwrap().generation() >= generation);
                let mut expected = recovered_visibility.unwrap();
                expected.nodes.insert(next);
                assert!(same_visibility(
                    &visible(&final_open, &all_documents),
                    &expected
                ));
                cuts_run += 1;
            }
        }));
        if let Err(error) = result {
            eprintln!(
                "restart model failure: ZE_TEST_SEED={seed:?} sequence={sequence} operations={operations:?}"
            );
            std::panic::resume_unwind(error);
        }
    }
    if sequences >= 8 {
        assert!(
            operation_counts.iter().all(|count| *count > 0),
            "uncovered operation kind: {operation_counts:?}"
        );
    }
    eprintln!(
        "restart model: seed={seed:?}, sequences={sequences}, cuts={cuts_run}, operation_counts={operation_counts:?} in OPERATIONS order, elapsed={:?}",
        started.elapsed()
    );
}
