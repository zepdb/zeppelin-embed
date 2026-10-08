//! Independent visibility model; exact successful VFS prefixes model process stops,
//! not torn sectors or physical power loss (covered by the existing crash matrix).
use super::*;
use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::lifecycle::{AccessMode, DocumentFields};
use crate::vfs::crash::{CrashOperation, MemoryVfs, RecordingVfs as ByteRecorder, replay_prefix};
use rand::Rng;
use rand::seq::SliceRandom;
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
    Upsert,
    Seal,
    DeleteSealed,
    Purge,
    PurgeRetryFault,
    Reindex,
    Schema,
    Retention,
    Graph,
    Mixed,
    MixedAppendFault,
    Checkpoint,
    Maintenance,
    Namespace,
    EpochSwitch,
    Snapshot,
    Oversized,
}

const OPERATIONS: [Operation; 19] = [
    Operation::Ingest,
    Operation::Batch,
    Operation::Upsert,
    Operation::Seal,
    Operation::DeleteSealed,
    Operation::Purge,
    Operation::PurgeRetryFault,
    Operation::Reindex,
    Operation::Schema,
    Operation::Retention,
    Operation::Graph,
    Operation::Mixed,
    Operation::MixedAppendFault,
    Operation::Checkpoint,
    Operation::Maintenance,
    Operation::Namespace,
    Operation::EpochSwitch,
    Operation::Snapshot,
    Operation::Oversized,
];

struct Step {
    start: usize,
    end: usize,
    before: Model,
    after: Model,
    purge: bool,
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
    try_write_node(store, key).unwrap()
}

fn try_write_node(
    store: &Store,
    key: &str,
) -> Result<(NodeId, u64), crate::lifecycle::native_graph::NativeGraphError> {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let receipts = store.apply_native_graph(
        &[StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "model", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }],
        &QueryControl::Cancel(CancelToken::new()),
    )?;
    match receipts[0].entity {
        EntityId::Node(node) => Ok((node, receipts[0].generation.get())),
        EntityId::Relationship(_) => panic!("expected model node"),
    }
}

fn try_write_mixed(
    store: &Store,
    ids: &[u128],
    key: &str,
) -> Result<(NodeId, u64), crate::lifecycle::native_graph::NativeGraphError> {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let result = store.apply_native_mixed(
        &IngestBatch::new(ids.iter().map(|id| document(*id)).collect()),
        &[StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "model", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }],
        &QueryControl::Cancel(CancelToken::new()),
    )?;
    let EntityId::Node(node) = result[0].entity else {
        panic!("expected mixed node")
    };
    Ok((node, result.changed_generation().unwrap().get()))
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

pub(crate) struct SequenceVfs {
    recorded: ByteRecorder<RecordingVfs>,
}

impl SequenceVfs {
    pub(crate) fn new() -> Self {
        Self {
            recorded: ByteRecorder::new(RecordingVfs::default()),
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
    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        self.recorded.create_directory(path)
    }
    fn remove_directory(&self, path: &Path) -> std::io::Result<()> {
        self.recorded.remove_directory(path)
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
        self.recorded.open_append(path)
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
        // StoreLock creates this OS lock file outside Vfs. Its staging cleanup
        // cannot enter the byte-image model as a delete without a recorded
        // creation. Engine files and their deletions remain fully recorded.
        if path.file_name().is_some_and(|name| name == "writer.lock") {
            self.recorded.inner().delete(path)
        } else {
            self.recorded.delete(path)
        }
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
    // A retained reader produces repeated references to the same artifacts.
    // Stop after intent publication, then validate RO and resume RW. The
    // >160k-reference regression is separate; keep the seeded shape small.
    let mut reclaim_rng =
        crate::test_support::seeded_rng("recovery::duplicate_reader_references_reopen");
    duplicate_reader_references_reopen(reclaim_rng.random_range(64..=1_000), 1);
    let sequences = std::env::var("ZE_RESTART_SEQUENCES")
        .map(|n| n.parse::<usize>().expect("positive ZE_RESTART_SEQUENCES"))
        .unwrap_or(8);
    assert!(sequences > 0);
    let started = std::time::Instant::now();
    let mut cuts_run = 0;
    let mut operation_counts = [0_usize; OPERATIONS.len()];
    let mut fault_publishers = [
        "seal",
        "purge",
        "checkpoint",
        "reindex",
        "retention",
        "merge",
        "snapshot",
        "schema",
        "alias",
        "epoch-drop",
        "promotion",
        "consolidation",
        "seal-rotation",
        "upsert-manifest-write",
        "schema-admission",
        "graph-noop",
        "namespace-noop",
    ];
    fault_publishers.shuffle(&mut rng);
    let mut fault_counts = [0_usize; 17];
    for sequence in 0..sequences {
        let length = rng.random_range(6..=12);
        let mut operations = Vec::new();
        // Catch only to add reproducibility context, then fail the entire run.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Manifest-absent graph debris: List is a definite no-write
            // refusal; failed first/later unlinks fence and reopen resumes.
            super::super::orphans::sweep_faults(rng.random());
            // Shuffle the fault histories with the same replayable seed. Every
            // publisher is exercised at the default eight sequences; failures
            // must refuse all subsequent acknowledgements until reopen.
            crate::lifecycle::tests::enable_graph_retry_completes(FaultPoint::PostManifestRename);
            crate::lifecycle::tests::enable_graph_retry_completes(FaultPoint::SelectorSync);
            enable_graph_retry_after_manifest_temp_sync_failure_reopens_writable();
            interrupted_enable_catalog_creation_fences_until_reopen();
            enable_graph_catalog_collision_is_a_definite_refusal();
            snapshot_after_completed_active_purge_reopens_without_sealing();
            crate::lifecycle::tests::replacement_snapshot_backup_reopens_with_nonzero_watermarks();
            for fault_index in [
                Some(sequence % 8),
                (sequence < 8).then_some(sequence + 8),
                (sequence == 0).then_some(16),
            ]
            .into_iter()
            .flatten()
            {
                let publisher = fault_publishers[fault_index];
                match publisher {
                    "seal-rotation" => ze390_seal_rotation_fault(),
                    "upsert-manifest-write" => ze390_upsert_manifest_fault(),
                    "schema-admission" => {
                        a_rejected_graph_admission_leaves_the_schema_and_generation_unchanged()
                    }
                    "graph-noop" => a_graph_noop_after_a_fenced_publication_is_refused(),
                    "namespace-noop" => {
                        an_all_noop_namespace_batch_after_a_fenced_publication_is_refused()
                    }
                    "schema" => schema_manifest_rename_failure(),
                    "alias" => epoch_manifest_rename_failure(false),
                    "epoch-drop" => epoch_manifest_rename_failure(true),
                    "promotion" => tier_manifest_rename_failure(false),
                    "consolidation" => tier_manifest_rename_failure(true),
                    _ => manifest_rename_publisher_failure(publisher),
                }
                fault_counts[fault_index] += 1;
            }
            super::super::seal::rotation_fault(
                if sequence % 2 == 0 {
                    FaultPoint::Rename
                } else {
                    FaultPoint::DirectorySync
                },
                2,
            );
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
            vfs.recorded.inner().arm_fault(FaultPoint::Enumeration);
            assert!(store.as_ref().unwrap().enable_graph().is_err());
            vfs.recorded.inner().assert_fired_once();
            assert_shared_writer_stopped(store.as_ref().unwrap());
            model.acknowledge(store.as_ref().unwrap().enable_graph().unwrap());
            assert!(store.as_ref().unwrap().admit_native_read().is_ok());
            disable_generation_fixture_maintenance(store.as_ref().unwrap());
            let (node, generation) = write_node(store.as_ref().unwrap(), "initial");
            model.nodes.insert(node);
            model.acknowledge(generation);
            let mut steps = Vec::new();
            let mut graph_unfolded = true;
            for index in 0..length {
                let active = model.documents.difference(&sealed).next().is_some();
                // Seal folds the current graph tail. Purge orders include both
                // unabsorbed graph tails and checkpoints with active documents.
                // Physical purge folds any graph tail before rewriting.
                let eligible: Vec<_> = OPERATIONS
                    .iter()
                    .copied()
                    .filter(|operation| match operation {
                        Operation::Reindex => !active || !graph_unfolded,
                        Operation::Purge | Operation::Upsert => !model.documents.is_empty(),
                        Operation::PurgeRetryFault => !graph_unfolded && !sealed.is_empty(),
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
                let mut start = vfs.operations().unwrap().len();
                let mut before = model.clone();
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
                    Operation::Upsert => {
                        let id = *model
                            .documents
                            .iter()
                            .nth(rng.random_range(0..model.documents.len()))
                            .unwrap();
                        let updated = IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(2 + index as u64)),
                            vec![1.0, 0.0],
                        )
                        .with_timestamp(id as i64)
                        .with_text("restart model updated document");
                        let generation = s
                            .ingest(IngestBatch::new(vec![updated]))
                            .unwrap()
                            .generation();
                        // A replacement of a sealed row now also has an active copy.
                        sealed.remove(&id);
                        generation
                    }
                    Operation::Seal => {
                        let generation = s.seal().unwrap();
                        sealed = model.documents.clone();
                        if active {
                            graph_unfolded = false;
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
                        let id = *model
                            .documents
                            .iter()
                            .nth(rng.random_range(0..model.documents.len()))
                            .unwrap();
                        let token = s.purge(&[DocId::new(id)]).unwrap();
                        let generation = s.await_physical_purge(token).unwrap().generation();
                        graph_unfolded = false;
                        model.documents.remove(&id);
                        sealed.remove(&id);
                        generation
                    }
                    Operation::PurgeRetryFault => {
                        let id = *sealed
                            .iter()
                            .nth(rng.random_range(0..sealed.len()))
                            .unwrap();
                        let token = s.purge(&[DocId::new(id)]).unwrap();
                        // The first directory sync publishes the replacement
                        // segment; the second follows the manifest rename.
                        vfs.recorded
                            .inner()
                            .arm_fault_after(FaultPoint::DirectorySync, 1);
                        assert!(s.await_physical_purge(token.clone()).is_err());
                        vfs.recorded.inner().assert_fired_once();
                        assert_eq!(s.snapshot().unwrap().generation(), model.generation());
                        assert!(
                            s.await_physical_purge(token.clone()).is_err(),
                            "purge retry cleared the publication fence"
                        );
                        assert!(
                            try_write_node(s, &format!("fenced-{index}")).is_err(),
                            "graph write crossed the publication fence"
                        );
                        drop(store.take());
                        store = Some(open(&path, &vfs, native_options()));
                        let recovered = store.as_ref().unwrap();
                        // Recovery can append a reclaim record while adopting
                        // the rejected graph preparation's orphan objects. Fold
                        // that tail, then finish any explicitly waiting intent.
                        recovered
                            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                            .unwrap();
                        match vfs.open(&path.join(crate::ingest::PURGE_INTENT_FILE)) {
                            Ok(_) => {
                                recovered.await_physical_purge(token).unwrap();
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => panic!("pending purge inspection: {error}"),
                        }
                        model.documents.remove(&id);
                        sealed.remove(&id);
                        recovered.snapshot().unwrap().generation()
                    }
                    Operation::Reindex => {
                        let generation = s.reindex_text().unwrap();
                        sealed = model.documents.clone();
                        if active {
                            graph_unfolded = false;
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
                        generation
                    }
                    Operation::Mixed | Operation::MixedAppendFault => {
                        let ids = [next_document, next_document + 1];
                        next_document += 2;
                        let failed = matches!(operation, Operation::MixedAppendFault);
                        if failed {
                            let fault = if rng.random_bool(0.5) {
                                FaultPoint::Append
                            } else {
                                FaultPoint::PartialAppend
                            };
                            vfs.recorded.inner().arm_fault(fault);
                        }
                        let result = try_write_mixed(s, &ids, &format!("mixed-{index}"));
                        all_documents.extend(ids);
                        if failed {
                            assert!(result.is_err());
                            vfs.recorded.inner().assert_fired_once();
                            assert_shared_writer_stopped(s);
                            drop(store.take());
                            store = Some(open(&path, &vfs, native_options()));
                            assert!(same_visibility(
                                &visible(store.as_ref().unwrap(), &all_documents),
                                &model
                            ));
                            assert_eq!(
                                store.as_ref().unwrap().snapshot().unwrap().generation(),
                                model.generation()
                            );
                            model.generation()
                        } else {
                            let (node, generation) = result.unwrap();
                            assert_eq!(s.snapshot().unwrap().generation(), model.generation() + 1);
                            model.nodes.insert(node);
                            graph_unfolded = true;
                            model.documents.extend(ids);
                            generation
                        }
                    }
                    Operation::Checkpoint => {
                        s.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                            .unwrap();
                        graph_unfolded = false;
                        s.snapshot().unwrap().generation()
                    }
                    Operation::Maintenance => {
                        let report = s
                            .maintain_native_graph_step(&QueryControl::Cancel(CancelToken::new()))
                            .unwrap();
                        model.acknowledge(report.generation.get());
                        graph_unfolded = true;
                        s.snapshot().unwrap().generation()
                    }
                    Operation::EpochSwitch => {
                        // The two complete epochs isolate interpretation transitions
                        // from the document history in the main visibility model.
                        let (epochs, a, b) = two_epoch_fixture();
                        let options = native_options().with_epoch(a.clone());
                        let epoch_store = Store::open(epochs.path(), options.clone()).unwrap();
                        for target in [b.identity(), a.identity()] {
                            epoch_store
                                .switch_epoch_alias(target)
                                .expect("graph-free switch");
                        }
                        epoch_store.enable_graph().unwrap();
                        disable_generation_fixture_maintenance(&epoch_store);
                        let before = file_snapshot(epochs.path());
                        assert!(matches!(
                            epoch_store.switch_epoch_alias(b.identity()),
                            Err(crate::epoch::EpochTransitionError::GraphEpochTransition)
                        ));
                        assert_eq!(file_snapshot(epochs.path()), before);
                        let (node, _) = write_node(&epoch_store, "after-epoch-refusal");
                        drop(epoch_store);
                        let reopened = Store::open(epochs.path(), options).unwrap();
                        assert!(observe_node(&reopened, node).is_some());
                        s.snapshot().unwrap().generation()
                    }
                    Operation::Snapshot => {
                        // Force an unabsorbed graph commit even after a prior fold.
                        let (node, generation) = write_node(s, &format!("before-snapshot-{index}"));
                        model.nodes.insert(node);
                        model.acknowledge(generation);
                        let end = vfs.operations().unwrap().len();
                        steps.push(Step {
                            start,
                            end,
                            before,
                            after: model.clone(),
                            purge: false,
                        });
                        start = end;
                        before = model.clone();
                        let copy = root.as_path().join(format!("snapshot-{index}"));
                        let captured = match s.write_snapshot(&copy) {
                            Ok(generation) => generation,
                            Err(crate::lifecycle::StoreError::SnapshotPin { detail }) => {
                                assert!(
                                    matches!(
                                        detail,
                                        "seal namespace document writes before exporting a graph snapshot"
                                    ),
                                    "unexpected snapshot refusal: {detail}"
                                );
                                assert!(same_visibility(&visible(s, &all_documents), &model));
                                assert!(!copy.exists());
                                model.acknowledge(s.snapshot().unwrap().generation());
                                s.checkpoint_native_graph(
                                    &QueryControl::Cancel(CancelToken::new()),
                                )
                                .unwrap();
                                model.acknowledge(s.snapshot().unwrap().generation());
                                model.acknowledge(s.seal().unwrap());
                                sealed = model.documents.clone();
                                let (node, generation) =
                                    write_node(s, &format!("before-retry-snapshot-{index}"));
                                model.nodes.insert(node);
                                model.acknowledge(generation);
                                s.write_snapshot(&copy).unwrap()
                            }
                            Err(error) => panic!("snapshot failed: {error}"),
                        };
                        for access in [AccessMode::ReadOnly, AccessMode::ReadWrite] {
                            let backup =
                                Store::open(&copy, native_options().with_access_mode(access))
                                    .unwrap();
                            assert_eq!(backup.snapshot().unwrap().generation(), captured);
                            assert!(same_visibility(&visible(&backup, &all_documents), &model));
                        }
                        model.acknowledge(captured);
                        // A post-pin export refusal creates no source mutation
                        // and must leave its writer usable. The existing VFS
                        // seam fires after the graph checkpoint has completed.
                        let fault_vfs = vfs.clone();
                        vfs.recorded.inner().after_next_directory_create(move || {
                            fault_vfs.recorded.inner().arm_fault(FaultPoint::OpenAppend);
                        });
                        let failed_copy = root.as_path().join(format!("snapshot-failed-{index}"));
                        assert!(matches!(
                            s.write_snapshot(&failed_copy),
                            Err(crate::lifecycle::StoreError::Io { .. })
                        ));
                        vfs.recorded.inner().assert_fired_once();
                        assert!(!failed_copy.exists());
                        // The snapshot retry can acknowledge a graph write of
                        // its own. Finish that model step before the subsequent
                        // source mutation, so crash cuts retain each real receipt.
                        steps.push(Step {
                            start,
                            end: vfs.operations().unwrap().len(),
                            before: before.clone(),
                            after: model.clone(),
                            purge: false,
                        });
                        start = vfs.operations().unwrap().len();
                        before = model.clone();
                        let view = s.open_snapshot().unwrap();
                        let pinned_model = model.clone();
                        let (later, generation) =
                            write_node(s, &format!("after-copy-refusal-{index}"));
                        model.nodes.insert(later);
                        assert!(same_visibility(
                            &visible(&view, &all_documents),
                            &pinned_model
                        ));
                        view.close().unwrap();
                        model.acknowledge(generation);
                        // Preserve Snapshot's folded-tail postcondition so the
                        // existing purge-retry fault remains eligible for seeded
                        // coverage after this additional acknowledged mutation.
                        s.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                            .unwrap();
                        graph_unfolded = false;
                        s.snapshot().unwrap().generation()
                    }
                    Operation::Oversized => {
                        use crate::lifecycle::StoreError;
                        let large = document(next_document)
                            .with_text("x".repeat(crate::wal::DEFAULT_MAX_GROUP_BYTES_DURABLE));
                        let error = s.ingest(IngestBatch::new(vec![large])).unwrap_err();
                        assert!(
                            matches!(
                                error,
                                crate::ingest::IngestError::Store(StoreError::WalWrite(
                                    crate::wal::WalWriteError::GroupTooLarge { .. }
                                ))
                            ),
                            "{error:?}"
                        );
                        let id = next_document;
                        next_document += 1;
                        model.acknowledge(
                            s.ingest(IngestBatch::new(vec![document(id)]))
                                .expect("document after definite refusal")
                                .generation(),
                        );
                        model.documents.insert(id);
                        all_documents.insert(id);
                        let end = vfs.operations().unwrap().len();
                        steps.push(Step {
                            start,
                            end,
                            before,
                            after: model.clone(),
                            purge: false,
                        });
                        start = end;
                        before = model.clone();
                        let (node, generation) = write_node(s, &format!("after-oversized-{index}"));
                        model.nodes.insert(node);
                        graph_unfolded = true;
                        generation
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
                    purge: matches!(operation, Operation::Purge | Operation::PurgeRetryFault),
                });
            }
            let bytes = vfs.operations().unwrap();
            drop(store);
            let mut cuts = BTreeSet::from([bytes.len()]);
            for step in steps.iter().enumerate().filter_map(|(index, step)| {
                (step.purge || index + 2 >= steps.len()).then_some(step)
            }) {
                cuts.insert(step.start);
                cuts.insert(step.end);
                for (index, operation) in bytes.iter().enumerate().take(step.end).skip(step.start) {
                    let path = match operation {
                        CrashOperation::Append { path, .. } | CrashOperation::Sync { path, .. } => {
                            Some(path)
                        }
                        CrashOperation::Rename { from, .. } => Some(from),
                        _ => None,
                    };
                    // These cuts model the source. Copy output does not mutate
                    // it, and the complete backup has its own RO/RW oracle.
                    if let Some(path) = path {
                        let snapshot_output = path
                            .strip_prefix(root.as_path())
                            .ok()
                            .and_then(|relative| relative.components().next())
                            .is_some_and(|part| {
                                let name = part.as_os_str().to_string_lossy();
                                name.starts_with("snapshot-") || name.starts_with(".snapshot-")
                            });
                        if !snapshot_output {
                            cuts.insert(index + 1);
                        }
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
            fault_counts.iter().all(|count| *count > 0),
            "uncovered manifest fault: {fault_publishers:?} {fault_counts:?}"
        );
        assert!(
            operation_counts.iter().all(|count| *count > 0),
            "uncovered operation kind: {operation_counts:?}"
        );
    }
    eprintln!(
        "restart model: seed={seed:?}, sequences={sequences}, cuts={cuts_run}, operation_counts={operation_counts:?} in OPERATIONS order, fault_publishers={fault_publishers:?}, fault_counts={fault_counts:?}, install_faults={sequences}, elapsed={:?}",
        started.elapsed()
    );
}
