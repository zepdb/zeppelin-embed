//! ZE-47 / ZE-172 directed native storage fault classes 1-6. Each body drives a real
//! native store through a faulty VFS and returns only actual observations;
//! every expected answer and comparator lives in the independent adversarial
//! family.

use super::publication::{
    FaultPoint, RecordingVfs, record_verified_fault, reset_verified_faults, take_verified_faults,
};
use super::tempfile;
use crate::graph_read_view_test_support::{ObservedRelationship, PathReceipt};
use crate::graph_storage_fault_test_support::{
    ObservedKeyedNode, StorageFaultProbeReport, StorageFaultSchedule, StorageFaultState,
};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::artifact::{BlockKind, HEADER_BYTES, PhysicalRef};
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
use crate::property_graph::storage::tree::{TreeKind, decode_page};
use crate::property_graph::storage::{
    GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphRevision, NodeId,
    NodeRef, RelId,
};
use crate::vfs::{StdVfs, SyncKind, Vfs, VfsFile};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use xxhash_rust::xxh3::xxh3_64;

const ARTIFACT_REF_FIRE: &str = "property-graph.storage-faults.artifact-ref.fire";
const ARTIFACT_REF_CLEAN: &str = "property-graph.storage-faults.artifact-ref.clean";
const SPLIT_FIRE: &str = "property-graph.storage-faults.split.fire";
const SPLIT_CLEAN: &str = "property-graph.storage-faults.split.clean";
const OUT_IN_FIRE: &str = "property-graph.storage-faults.out-in.fire";
const OUT_IN_CLEAN: &str = "property-graph.storage-faults.out-in.clean";
const ROOT_FIRE: &str = "property-graph.storage-faults.root-replacement.fire";
const ROOT_CLEAN: &str = "property-graph.storage-faults.root-replacement.clean";

/// Byte offset of the root selector's graph-generation field. The selector is
/// a fixed 120-byte record, not an artifact with a 96-byte header.
const ROOT_SELECTOR_GENERATION: usize = 48;

/// Keyed nodes per class-2 commit. Class 2 discovers which chunk splits the
/// node directory at this granularity, so the chunk is small enough to name
/// one commit and large enough to keep discovery bounded. The adversarial
/// family knows the same constant and checks the reported pre-split
/// population against it.
const SPLIT_CHUNK_KEYS: usize = 16;

fn native_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn clock() -> Arc<crate::lifecycle::SystemMonotonicClock> {
    Arc::new(crate::lifecycle::SystemMonotonicClock)
}

/// Returns every `.zgraph` artifact file under one native store directory.
fn artifact_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(directory)
        .expect("read native store directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|value| value == "zgraph"))
        .collect();
    paths.sort();
    paths
}

fn file_snapshot(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(directory)
        .expect("read native store directory")
        .map(|entry| {
            let path = entry.expect("directory entry").path();
            let bytes = std::fs::read(&path).expect("read native store file");
            (path, bytes)
        })
        .collect()
}

fn root_bundle_path(directory: &Path) -> PathBuf {
    directory.join("graph-root.ze")
}

/// Chooses a damage offset strictly inside the artifact payload.
fn payload_offset(bytes: &[u8], seeded: u64) -> usize {
    let span = bytes.len().saturating_sub(HEADER_BYTES).max(1);
    HEADER_BYTES + (seeded as usize % span)
}

/// A VFS that returns a different file's handle from one `open_for_map` call.
/// This is the wrong-object read fault of classes 1 and 3.
struct MisdirectVfs {
    substitute: Mutex<Option<PathBuf>>,
    fires: AtomicU64,
    recording: RecordingVfs,
    target: Mutex<Option<(PathBuf, Option<PathBuf>)>>,
    opens: Mutex<Vec<PathBuf>>,
}

impl MisdirectVfs {
    fn new() -> Self {
        Self {
            substitute: Mutex::new(None),
            fires: AtomicU64::new(0),
            recording: RecordingVfs::default(),
            target: Mutex::new(None),
            opens: Mutex::new(Vec::new()),
        }
    }

    fn arm(&self, substitute: PathBuf) {
        *self.substitute.lock().expect("misdirect target") = Some(substitute);
    }

    fn fires(&self) -> u64 {
        self.fires.load(Ordering::Relaxed)
    }
}

impl Vfs for MisdirectVfs {
    fn segment_data_read_counter(&self) -> Option<Arc<AtomicU64>> {
        self.recording.segment_data_read_counter()
    }

    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.recording.ensure_directory(path, create)
    }

    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        self.recording.create_directory(path)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.recording.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        self.opens
            .lock()
            .expect("open log")
            .push(path.to_path_buf());
        let mut target = self.target.lock().expect("exact target");
        if target
            .as_ref()
            .is_some_and(|(expected, _)| expected == path)
        {
            let (_, substitute) = target.take().expect("targeted open");
            drop(target);
            self.fires.fetch_add(1, Ordering::Relaxed);
            return match substitute {
                Some(other) => self.recording.open_for_map(&other),
                None => {
                    drop(self.recording.open_for_map(path)?);
                    Err(std::io::Error::other("ZE-172 PostCommitError"))
                }
            };
        }
        drop(target);
        let mut armed = self.substitute.lock().expect("misdirect target");
        if armed.as_deref().is_some_and(|other| other != path) {
            let other = armed.take().expect("armed substitute");
            drop(armed);
            self.fires.fetch_add(1, Ordering::Relaxed);
            return self.recording.open_for_map(&other);
        }
        drop(armed);
        self.recording.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.recording.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.recording.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.recording.write(path, bytes)
    }

    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.recording.create_new(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.recording.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.recording.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.recording.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.recording.list(directory)
    }

    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.recording.for_each_direct_child(directory, visitor)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.recording.delete(path)
    }
}

fn create_store(path: &Path, vfs: &Arc<dyn Vfs>) -> Store {
    Store::create_native_graph_with_infrastructure(
        path,
        native_options(),
        None,
        Arc::clone(vfs),
        clock(),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store")
}

fn reopen_store(path: &Path, vfs: &Arc<dyn Vfs>) -> Store {
    Store::open_native_graph_with_infrastructure(
        path,
        native_options(),
        None,
        Arc::clone(vfs),
        clock(),
    )
    .expect("clean reopen")
}

/// Commits one keyed node and returns its allocated identity.
fn commit_node(store: &Store, key: &str, text: &str) -> NodeId {
    commit_keyed_nodes(store, std::slice::from_ref(&key.to_owned()), Some(text))
        .pop()
        .expect("keyed node identity")
}

/// Commits a batch of keyed nodes and returns their allocated identities in
/// request order, or the store's own typed refusal rendered for reporting.
fn try_commit_keyed_nodes(
    store: &Store,
    keys: &[String],
    text: Option<&str>,
) -> Result<Vec<NodeId>, String> {
    let images: Vec<_> = keys
        .iter()
        .map(|_| CanonicalContents::node(&mut [], &mut [], text, None).expect("node contents"))
        .collect();
    let requests: Vec<_> = keys
        .iter()
        .zip(images.iter())
        .map(|(key, image)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "ze47", key).expect("node key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        })
        .collect();
    let receipts = store
        .apply_native_graph(&requests, &control())
        .map_err(|error| format!("{error:?}"))?;
    Ok(receipts
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => panic!("node receipt domain"),
        })
        .collect())
}

/// Commits a batch of keyed nodes and returns their allocated identities in
/// request order.
fn commit_keyed_nodes(store: &Store, keys: &[String], text: Option<&str>) -> Vec<NodeId> {
    try_commit_keyed_nodes(store, keys, text)
        .unwrap_or_else(|error| panic!("keyed node commit {:?}: {error}", keys.first()))
}

/// Commits one relationship between two fresh nodes in a single batch, so one
/// candidate carries both the OUT and IN directions.
fn commit_linked_pair(store: &Store, tag: &str) -> (NodeId, NodeId, RelId) {
    let source_key = format!("{tag}-a");
    let target_key = format!("{tag}-b");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], Some("out"), None).expect("out node");
        let second = CanonicalContents::node(&mut [], &mut [], Some("in"), None).expect("in node");
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze47", &source_key)
                    .expect("source key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze47", &target_key)
                    .expect("target key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze47", tag)
                    .expect("relationship key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).expect("local source")),
                    target: NodeRef::Local(refs.node(1).expect("local target")),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &control())
            .expect("linked pair commit")
    });
    let source = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("source receipt domain"),
    };
    let target = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("target receipt domain"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt domain"),
    };
    (source, target, relationship)
}

/// Actual lookup of one node through a live store admission. `Err` is a loud
/// typed refusal; `Ok(None)` is an absent record.
fn lookup(store: &Store, node: NodeId) -> Result<Option<u64>, String> {
    struct Consumer {
        node: NodeId,
    }
    impl super::super::NativeReadConsumer<Option<u64>> for Consumer {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<Option<u64>, TreeError> {
            let mut resources = TreeResources::for_query(runtime)?;
            Ok(view
                .lookup_node(self.node, &mut resources)?
                .map(|record| record.record().revision().get()))
        }
    }
    store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            Consumer { node },
        )
        .map_err(|error| error.to_string())
}

/// Observes a keyed-node population plus the actual node-directory root level
/// and an xxh3-64 over that root page's bytes, through one retained lease, so
/// a retained pre-split root can still be read after later commits have been
/// attempted and its bytes compared across them.
fn observe_with_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    keys: &[(Vec<u8>, NodeId)],
) -> Result<(Vec<ObservedKeyedNode>, u16, u64), TreeError> {
    let shared = GraphResources::from_store(store).expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 16 * 1024 * 1024).expect("query memory");
    let guard = control();
    let mut runtime = RuntimeContext::new(lease, &guard, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained resources");
    let source = NativeQuerySource::new(capability, &resources, 64).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained graph view");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained lookup resources");
    let root = lease.bundle().roots().directory(TreeKind::Nodes)?;
    let (level, digest) = match root.reference() {
        Some(reference) => {
            let block = source.resolve(reference, &mut resources)?;
            let level = decode_page(TreeKind::Nodes, block.payload())
                .map_err(|_| TreeError::Invalid("node directory root page"))?
                .header()
                .level;
            (level, xxh3_64(block.payload()))
        }
        None => (0, 0),
    };
    let mut observed = Vec::new();
    for (key, node) in keys {
        let record = view
            .lookup_node(*node, &mut resources)?
            .ok_or(TreeError::Missing)?;
        observed.push(ObservedKeyedNode {
            key: key.clone(),
            node: node.get(),
            revision: record.record().revision().get(),
        });
    }
    Ok((observed, level, digest))
}

fn classify_tree_error(error: &TreeError) -> &'static str {
    match error {
        TreeError::Invalid(_) => "Invalid",
        TreeError::Missing => "Missing",
        TreeError::Format(_) => "Format",
        TreeError::Io(_) => "Io",
        TreeError::Memory => "Memory",
        TreeError::Work => "Work",
        TreeError::Control(_) => "Control",
        TreeError::Runtime(_) => "Runtime",
        TreeError::WalMetadata(_) => "WalMetadata",
    }
}

/// Reopens a damaged store and classifies the loud refusal. A store that still
/// opens must refuse the referenced lookup instead of answering silently.
fn classify_damaged_open(path: &Path, vfs: &Arc<dyn Vfs>, node: NodeId) -> String {
    match Store::open_native_graph_with_infrastructure(
        path,
        native_options(),
        None,
        Arc::clone(vfs),
        clock(),
    ) {
        Err(_) => "refused-open".to_owned(),
        Ok(store) => {
            let classification = match lookup(&store, node) {
                Err(_) => "refused-lookup".to_owned(),
                Ok(None) => "silent-absence".to_owned(),
                Ok(Some(_)) => "silent-answer".to_owned(),
            };
            let _ = store.close();
            classification
        }
    }
}

/// Resolves the published node-directory root through a substituted block
/// kind. The extent is byte-identical; only the named kind changes.
fn substituted_kind_refusal(store: &Store, lease: &super::super::NativeReadLease) -> String {
    let shared = GraphResources::from_store(store).expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let guard = control();
    let mut runtime = RuntimeContext::new(lease, &guard, &memory, RuntimeLimits::default())
        .expect("substitution runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("substitution capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("substitution resources");
    let source = NativeQuerySource::new(capability, &resources, 32).expect("substitution source");
    let root = lease
        .bundle()
        .roots()
        .directory(TreeKind::Nodes)
        .expect("node directory root")
        .reference()
        .expect("published node root");
    let substituted = PhysicalRef {
        kind: BlockKind::NodeRecord,
        ..root
    };
    match source.resolve(substituted, &mut resources) {
        Ok(_) => "kind-substitution:resolved".to_owned(),
        Err(error) => format!("kind-substitution:{}", classify_tree_error(&error)),
    }
}

struct ArtifactRefOutcome {
    refusals: Vec<String>,
    surviving: Vec<ObservedKeyedNode>,
}

/// Class 1: byte damage, a misdirected mapping and a substituted block kind
/// applied to artifacts the current root references.
fn run_artifact_ref_class(schedule: StorageFaultSchedule) -> ArtifactRefOutcome {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(MisdirectVfs::new());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = create_store(&path, &infrastructure);
    let mut keys = Vec::new();
    for index in 0..4_u32 {
        let name = format!("ref-{index}");
        let node = commit_node(&store, &name, "artifact reference payload");
        keys.push((name.into_bytes(), node));
    }
    let untouched = keys.last().expect("committed key").1;

    let lease = store.admit_native_read().expect("kind substitution lease");
    let mut refusals = vec![substituted_kind_refusal(&store, &lease)];
    drop(lease);
    store.close().expect("close native store");
    drop(store);

    let baseline = file_snapshot(&path);
    let artifacts = artifact_files(&path);
    assert!(!artifacts.is_empty(), "published store must own artifacts");
    let victim = artifacts.first().expect("victim artifact").clone();

    for variant in ["bit-flip", "truncate"] {
        let original = std::fs::read(&victim).expect("read victim artifact");
        let mut damaged = original.clone();
        if variant == "bit-flip" {
            let offset = payload_offset(&original, schedule.artifact_ref_offset);
            damaged[offset] ^= 0x40;
        } else {
            damaged.truncate(original.len() / 2);
        }
        std::fs::write(&victim, &damaged).expect("write damaged artifact");
        refusals.push(format!(
            "{variant}:{}",
            classify_damaged_open(&path, &infrastructure, untouched)
        ));
        std::fs::write(&victim, &original).expect("restore damaged artifact");
        assert_eq!(file_snapshot(&path), baseline, "byte-exact restore");
    }

    let substitute = artifacts
        .get(1)
        .cloned()
        .unwrap_or_else(|| root_bundle_path(&path));
    vfs.arm(substitute);
    refusals.push(format!(
        "wrong-object:{}",
        classify_damaged_open(&path, &infrastructure, untouched)
    ));
    assert!(vfs.fires() >= 1, "wrong-object fault never fired");
    assert_eq!(file_snapshot(&path), baseline, "refusal mutated no byte");

    let reopened = reopen_store(&path, &infrastructure);
    let mut surviving = Vec::new();
    for (key, node) in &keys {
        let revision = lookup(&reopened, *node)
            .expect("clean lookup")
            .expect("committed node");
        surviving.push(ObservedKeyedNode {
            key: key.clone(),
            node: node.get(),
            revision,
        });
    }
    reopened.close().expect("close reopened store");
    for _ in 0..refusals.len() {
        record_verified_fault();
    }
    ArtifactRefOutcome {
        refusals,
        surviving,
    }
}

struct SplitOutcome {
    level: u16,
    retained_level: u16,
    pre_split_keys: u32,
    committed: Vec<ObservedKeyedNode>,
    old_root: Vec<ObservedKeyedNode>,
    generations: (u64, u64),
    root_digests: (u64, u64),
    unsplit_generations: (u64, u64),
    unsplit_reopen_level: u16,
    refusals: Vec<String>,
}

/// One store carrying every keyed chunk before the splitting one, with the
/// pre-split lease that is this class's oracle already retained. `parent` is
/// declared last so the temporary directory outlives the mapped store.
struct PreSplitStore {
    store: Store,
    retained: super::super::NativeReadLease,
    keys: Vec<(Vec<u8>, NodeId)>,
    digest: u64,
    generation: u64,
    path: PathBuf,
    infrastructure: Arc<dyn Vfs>,
    vfs: Arc<RecordingVfs>,
    parent: tempfile::TempDir,
}

/// Builds a store holding exactly `names`, then retains a lease over that
/// provably unsplit root. Every later observation through `retained` reads the
/// tree as it was before any split was attempted.
fn build_pre_split_store(names: &[String]) -> PreSplitStore {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = create_store(&path, &infrastructure);
    let mut keys = Vec::new();
    for batch in names.chunks(SPLIT_CHUNK_KEYS) {
        for (name, node) in batch.iter().zip(commit_keyed_nodes(&store, batch, None)) {
            keys.push((name.clone().into_bytes(), node));
        }
    }
    let retained = store.admit_native_read().expect("pre-split lease");
    let (_, level, digest) =
        observe_with_lease(&store, &retained, &[]).expect("pre-split root observation");
    assert_eq!(
        level, 0,
        "the retained lease must pin a pre-split node directory root"
    );
    let generation = retained.bundle().roots().generation().get();
    PreSplitStore {
        store,
        retained,
        keys,
        digest,
        generation,
        path,
        infrastructure,
        vfs,
        parent,
    }
}

/// The ascending class-2 key names, which the adversarial family derives
/// independently from the same format.
fn split_key_names(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("split-{index:05}"))
        .collect()
}

/// Commits ascending keyed chunks into a scratch store until the node
/// directory root first reaches level >= 1, and returns the one-based number
/// of the chunk whose commit caused that split. The real class-2 store then
/// stops one chunk short of it, so the faulted commit is the splitting commit
/// rather than an ordinary one after the tree has already grown a branch.
fn discover_splitting_chunk(cap: usize) -> usize {
    let parent = tempfile::tempdir().expect("discovery parent");
    let path = parent.path().join("native");
    let infrastructure: Arc<dyn Vfs> = Arc::new(StdVfs);
    let store = create_store(&path, &infrastructure);
    let names = split_key_names(cap);
    let mut discovered = None;
    for (index, batch) in names.chunks(SPLIT_CHUNK_KEYS).enumerate() {
        let _ = commit_keyed_nodes(&store, batch, None);
        let lease = store.admit_native_read().expect("discovery lease");
        let (_, level, _) = observe_with_lease(&store, &lease, &[]).expect("discovery observation");
        drop(lease);
        if level >= 1 {
            discovered = Some(index + 1);
            break;
        }
    }
    store.close().expect("close discovery store");
    discovered.expect("the scheduled population must split the node directory")
}

/// Class 2: faults scheduled against the commit that actually splits the node
/// directory, proved against a lease retained from strictly before the split,
/// plus a byte fault on the separator bytes that split produced.
///
/// Two stores are needed because the fault points end in two different
/// product states. `PartialCreate` and `ObjectSync` are recoverable refusals,
/// so one store can take both and then publish the same split cleanly.
/// `Append` fails the WAL envelope and returns `CommitIndeterminate`, which
/// permanently stops writes and read admissions; that arm therefore runs on
/// its own store and is proved through a reopen instead.
fn run_split_class(schedule: StorageFaultSchedule) -> SplitOutcome {
    let chunk = discover_splitting_chunk(schedule.split_keys as usize);
    assert!(chunk >= 2, "the first chunk must not already split");
    let pre_split = (chunk - 1) * SPLIT_CHUNK_KEYS;
    let names = split_key_names(chunk * SPLIT_CHUNK_KEYS);
    let pre_split_names = names.get(..pre_split).expect("pre-split names").to_vec();
    let splitting = names.get(pre_split..).expect("splitting chunk").to_vec();

    let recoverable = build_pre_split_store(&pre_split_names);
    let mut refusals = Vec::new();
    for point in [FaultPoint::PartialCreate, FaultPoint::ObjectSync] {
        recoverable
            .vfs
            .arm_fault_after(point, u64::from(schedule.split_skip) % 2);
        let result = try_commit_keyed_nodes(&recoverable.store, &splitting, None);
        refusals.push(format!(
            "{point:?}:{}",
            if result.is_err() {
                "refused"
            } else {
                "published"
            }
        ));
        assert!(
            result.is_err(),
            "a fault on the splitting commit did not refuse"
        );
        recoverable.vfs.assert_fired_once();
        let (_, level, digest) = observe_with_lease(&recoverable.store, &recoverable.retained, &[])
            .expect("retained root observation");
        assert_eq!(level, 0, "a refused split moved the retained root level");
        assert_eq!(
            digest, recoverable.digest,
            "a refused split rewrote the retained root page"
        );
        let fresh = recoverable
            .store
            .admit_native_read()
            .expect("post-refusal lease");
        assert_eq!(
            fresh.bundle().roots().generation().get(),
            recoverable.generation,
            "a refused split advanced the published generation"
        );
        drop(fresh);
    }
    let fresh = recoverable
        .store
        .admit_native_read()
        .expect("post-fault lease");
    let after = fresh.bundle().roots().generation().get();
    drop(fresh);

    // The same chunk, now unfaulted: the split the faults were aimed at.
    let _ = commit_keyed_nodes(&recoverable.store, &splitting, None);
    let current = recoverable
        .store
        .admit_native_read()
        .expect("post-split lease");
    let (committed, level, _) = observe_with_lease(&recoverable.store, &current, &recoverable.keys)
        .expect("committed key observation");
    assert!(
        level >= 1,
        "the splitting chunk did not split the node directory"
    );
    let (old_root, retained_level, retained_digest) =
        observe_with_lease(&recoverable.store, &recoverable.retained, &recoverable.keys)
            .expect("old root observation");
    assert_eq!(
        retained_level, 0,
        "the retained root is not the pre-split root"
    );
    let probe = (schedule.split_probe as usize) % recoverable.keys.len();
    assert_eq!(
        old_root.get(probe),
        committed.get(probe),
        "the split moved a pre-split key"
    );
    drop(current);

    // The indeterminate arm, on its own store.
    let (unsplit_generations, unsplit_reopen_level) =
        run_indeterminate_split(&pre_split_names, &splitting, &mut refusals);

    let PreSplitStore {
        store,
        retained,
        keys,
        digest: pre_digest,
        generation: before,
        path,
        infrastructure,
        parent,
        vfs: _,
    } = recoverable;
    drop(retained);
    store.close().expect("close split store");
    drop(store);

    if let Some(victim) = separator_artifact(&path) {
        let original = std::fs::read(&victim).expect("read separator artifact");
        let mut damaged = original.clone();
        let offset = payload_offset(&original, schedule.artifact_ref_offset.wrapping_add(7));
        damaged[offset] ^= 0x01;
        std::fs::write(&victim, &damaged).expect("write damaged separator");
        refusals.push(format!(
            "separator:{}",
            classify_damaged_open(&path, &infrastructure, keys[probe].1)
        ));
        std::fs::write(&victim, &original).expect("restore separator artifact");
    }
    drop(parent);
    for _ in 0..refusals.len() {
        record_verified_fault();
    }
    SplitOutcome {
        level,
        retained_level,
        pre_split_keys: u32::try_from(pre_split).expect("pre-split population fits u32"),
        committed,
        old_root,
        generations: (before, after),
        root_digests: (pre_digest, retained_digest),
        unsplit_generations,
        unsplit_reopen_level,
        refusals,
    }
}

/// Fails the WAL envelope of the splitting commit on a store of its own, then
/// proves the split never reached a reader: the retained pre-split root is
/// byte-unchanged, admissions are stopped, and a reopen exposes the same
/// generation over a root that is still level zero.
///
/// Returns the pre-fault and reopened generations and the reopened root level.
fn run_indeterminate_split(
    pre_split_names: &[String],
    splitting: &[String],
    refusals: &mut Vec<String>,
) -> ((u64, u64), u16) {
    let indeterminate = build_pre_split_store(pre_split_names);
    indeterminate.vfs.arm_fault(FaultPoint::Append);
    let result = try_commit_keyed_nodes(&indeterminate.store, splitting, None);
    refusals.push(format!(
        "Append:{}",
        if result.is_err() {
            "refused"
        } else {
            "published"
        }
    ));
    assert!(
        result.is_err(),
        "a WAL envelope fault on the splitting commit did not refuse"
    );
    indeterminate.vfs.assert_fired_once();
    let (_, level, digest) = observe_with_lease(&indeterminate.store, &indeterminate.retained, &[])
        .expect("retained root observation");
    assert_eq!(
        level, 0,
        "an indeterminate split moved the retained root level"
    );
    assert_eq!(
        digest, indeterminate.digest,
        "an indeterminate split rewrote the retained root page"
    );
    refusals.push(format!(
        "stopped-admission:{}",
        if indeterminate.store.admit_native_read().is_err() {
            "refused"
        } else {
            "published"
        }
    ));

    let PreSplitStore {
        store,
        retained,
        keys,
        generation,
        path,
        infrastructure,
        parent,
        ..
    } = indeterminate;
    drop(retained);
    store.close().expect("close indeterminate store");
    drop(store);

    let reopened = reopen_store(&path, &infrastructure);
    let lease = reopened.admit_native_read().expect("reopened lease");
    let (survivors, reopened_level, _) =
        observe_with_lease(&reopened, &lease, &keys).expect("reopened observation");
    let reopened_generation = lease.bundle().roots().generation().get();
    assert_eq!(
        survivors.len(),
        keys.len(),
        "the reopen lost a pre-split key"
    );
    drop(lease);
    reopened
        .close()
        .expect("close reopened indeterminate store");
    drop(parent);
    ((generation, reopened_generation), reopened_level)
}

/// Returns the largest artifact file, which holds the split directory pages.
fn separator_artifact(directory: &Path) -> Option<PathBuf> {
    artifact_files(directory).into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .map(|meta| meta.len())
            .unwrap_or_default()
    })
}

struct OutInOutcome {
    out_rows: Vec<ObservedRelationship>,
    in_rows: Vec<ObservedRelationship>,
    refusals: Vec<String>,
}

/// Class 3: a half-prepared OUT/IN candidate and a wrong-object read of the
/// adjacency pack. The candidate must never publish and both directions must
/// still agree after a reopen.
fn run_out_in_class(schedule: StorageFaultSchedule) -> OutInOutcome {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let recording = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = recording.clone();
    let store = create_store(&path, &infrastructure);
    let (source, target, relationship) = commit_linked_pair(&store, "out-in");
    let lease = store.admit_native_read().expect("published lease");
    let published = lease.bundle().roots().generation().get();
    drop(lease);

    // The second batch writes both directions of a new relationship. The
    // fault fires after an earlier object of the same candidate succeeded, so
    // the candidate is half prepared when it refuses.
    let mut refusals = Vec::new();
    // The skip must never be zero: a fault on the candidate's first `create`
    // leaves nothing prepared, so the half-prepared claim would be false. The
    // schedule supplies 1..=3, which maps here onto a skip of 1 or 2.
    recording.arm_fault_after(
        FaultPoint::Create,
        u64::from(schedule.out_in_append % 2 + 1),
    );
    let result = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], Some("half"), None).expect("node");
        let second =
            CanonicalContents::node(&mut [], &mut [], Some("prepared"), None).expect("node");
        store.apply_native_graph(
            &[
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze47", "half-a").expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&first)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze47", "half-b").expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&second)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "ze47", "half")
                        .expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Local(refs.node(0).expect("local source")),
                        target: NodeRef::Local(refs.node(1).expect("local target")),
                        relationship_type: GraphName::new("LINKS").expect("type"),
                        properties: &[],
                    }),
                },
            ],
            &control(),
        )
    });
    refusals.push(format!(
        "half-prepared:{}",
        if result.is_err() {
            "refused"
        } else {
            "published"
        }
    ));
    assert!(result.is_err(), "half-prepared candidate did not refuse");
    recording.assert_fired_once();
    let lease = store.admit_native_read().expect("post-fault lease");
    assert_eq!(
        lease.bundle().roots().generation().get(),
        published,
        "a failed OUT/IN candidate published a root"
    );
    drop(lease);
    store.close().expect("close out/in store");
    drop(store);

    // A misdirected mapping while the reader opens an adjacency pack.
    let misdirect = Arc::new(MisdirectVfs::new());
    let misdirecting: Arc<dyn Vfs> = misdirect.clone();
    let artifacts = artifact_files(&path);
    if artifacts.len() >= 2 {
        misdirect.arm(artifacts[1].clone());
        refusals.push(format!(
            "wrong-object:{}",
            classify_damaged_open(&path, &misdirecting, source)
        ));
        assert!(misdirect.fires() >= 1, "wrong-object fault never fired");
    }

    let reopened = reopen_store(&path, &infrastructure);
    let (out_rows, in_rows) = observe_directions(&reopened, source, target);
    assert!(
        lookup(&reopened, source).expect("source lookup").is_some()
            && lookup(&reopened, target).expect("target lookup").is_some(),
        "reopen lost a committed endpoint"
    );
    assert_eq!(
        out_rows.len(),
        1,
        "the committed relationship must survive as exactly one OUT row"
    );
    assert_eq!(out_rows[0].rel, relationship.get());
    reopened.close().expect("close reopened out/in store");
    for _ in 0..refusals.len() {
        record_verified_fault();
    }
    OutInOutcome {
        out_rows,
        in_rows,
        refusals,
    }
}

fn observe_directions(
    store: &Store,
    source: NodeId,
    target: NodeId,
) -> (Vec<ObservedRelationship>, Vec<ObservedRelationship>) {
    use crate::property_graph::storage::adjacency::RelationshipRow;
    use crate::property_graph::storage::{
        CursorState, DirectionSelection, RelationshipTypeSelection,
    };
    struct Consumer {
        source: NodeId,
        target: NodeId,
    }
    impl super::super::NativeReadConsumer<(Vec<RelationshipRow>, Vec<RelationshipRow>)> for Consumer {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<(Vec<RelationshipRow>, Vec<RelationshipRow>), TreeError> {
            let filler = RelationshipRow {
                rel: RelId::new(1).expect("filler relationship"),
                source: NodeId::new(1).expect("filler source"),
                target: NodeId::new(1).expect("filler target"),
                relationship_type: crate::property_graph::catalog::RelTypeId::new(1)
                    .expect("filler type"),
            };
            let mut directions = Vec::new();
            for (node, direction) in [
                (self.source, DirectionSelection::Out),
                (self.target, DirectionSelection::In),
            ] {
                let mut cursor = view.expansion_cursor(
                    node,
                    direction,
                    RelationshipTypeSelection::All,
                    runtime,
                )?;
                let mut rows = Vec::new();
                loop {
                    let mut output = [filler; 8];
                    let (count, state) = view.expand(&mut cursor, &mut output, runtime)?;
                    rows.extend_from_slice(&output[..count]);
                    if state == CursorState::Done {
                        break;
                    }
                }
                directions.push(rows);
            }
            let incoming = directions.pop().unwrap_or_default();
            let outgoing = directions.pop().unwrap_or_default();
            Ok((outgoing, incoming))
        }
    }
    let (out_rows, in_rows) = store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            Consumer { source, target },
        )
        .expect("direction observation");
    let convert = |rows: Vec<RelationshipRow>| {
        rows.into_iter()
            .map(|row| ObservedRelationship {
                rel: row.rel.get(),
                source: row.source.get(),
                target: row.target.get(),
                relationship_type: row.relationship_type.get(),
            })
            .collect::<Vec<_>>()
    };
    (convert(out_rows), convert(in_rows))
}

struct RootOutcome {
    refusals: Vec<String>,
    reopened_generation: u64,
    previous_generation: u64,
}

/// Class 4: root replacement faults applied to a published store.
fn run_root_replacement_class(schedule: StorageFaultSchedule) -> RootOutcome {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = create_store(&path, &infrastructure);
    let mut keys = Vec::new();
    for index in 0..3_u32 {
        let name = format!("root-{index}");
        let node = commit_node(&store, &name, "root replacement payload");
        keys.push((name.into_bytes(), node));
    }
    let lease = store.admit_native_read().expect("published lease");
    let previous_generation = lease.bundle().roots().generation().get();
    drop(lease);

    // A crash between the directory sync and the WAL envelope must leave the
    // acknowledged generation exactly where it was.
    let mut refusals = Vec::new();
    vfs.arm_fault_after(FaultPoint::Append, u64::from(schedule.root_variant % 2));
    let image = CanonicalContents::node(&mut [], &mut [], Some("stale"), None).expect("contents");
    let result = store.apply_native_graph(
        &[StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "ze47", "root-stale").expect("stale key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }],
        &control(),
    );
    refusals.push(format!(
        "pre-envelope:{}",
        if result.is_err() {
            "refused"
        } else {
            "published"
        }
    ));
    assert!(result.is_err(), "pre-envelope fault did not refuse");
    vfs.assert_fired_once();
    store.close().expect("close root store");
    drop(store);

    let baseline = file_snapshot(&path);
    let bundle = root_bundle_path(&path);
    let original = std::fs::read(&bundle).expect("read root bundle");

    let mut torn = original.clone();
    torn.truncate(original.len().saturating_sub(16).max(1));
    std::fs::write(&bundle, &torn).expect("write torn root bundle");
    refusals.push(format!(
        "torn-tail:{}",
        classify_damaged_open(&path, &infrastructure, keys[0].1)
    ));
    std::fs::write(&bundle, &original).expect("restore root bundle");
    assert_eq!(file_snapshot(&path), baseline, "byte-exact restore");

    // The selector's generation field is bytes 48..56. Lowering it names a
    // root generation older than the checkpoint base without touching any
    // other field, so only the selector's own integrity can refuse it.
    let mut stale = original.clone();
    let generation = u64::from_le_bytes(
        original
            .get(ROOT_SELECTOR_GENERATION..ROOT_SELECTOR_GENERATION + 8)
            .and_then(|field| <[u8; 8]>::try_from(field).ok())
            .expect("selector generation field"),
    );
    stale
        .get_mut(ROOT_SELECTOR_GENERATION..ROOT_SELECTOR_GENERATION + 8)
        .expect("selector generation field")
        .copy_from_slice(&generation.checked_sub(1).unwrap_or(u64::MAX).to_le_bytes());
    assert_ne!(stale, original, "stale-generation fault damaged no byte");
    std::fs::write(&bundle, &stale).expect("write stale root bundle");
    refusals.push(format!(
        "stale-generation:{}",
        classify_damaged_open(&path, &infrastructure, keys[0].1)
    ));
    std::fs::write(&bundle, &original).expect("restore root bundle");
    assert_eq!(file_snapshot(&path), baseline, "byte-exact restore");

    let reopened = reopen_store(&path, &infrastructure);
    let lease = reopened.admit_native_read().expect("reopened lease");
    let reopened_generation = lease.bundle().roots().generation().get();
    drop(lease);
    for (_, node) in &keys {
        assert!(
            lookup(&reopened, *node).expect("reopened lookup").is_some(),
            "reopen lost a previously acknowledged node"
        );
    }
    assert!(
        lookup(&reopened, NodeId::new(u128::MAX).expect("absent node"))
            .expect("absent lookup")
            .is_none(),
        "reopen invented an unpublished node"
    );
    reopened.close().expect("close reopened root store");
    for _ in 0..refusals.len() {
        record_verified_fault();
    }
    RootOutcome {
        refusals,
        reopened_generation,
        previous_generation,
    }
}

#[cfg(feature = "test-seams")]
pub(super) fn run_actual_probe(
    _seed: u64,
    schedule: StorageFaultSchedule,
) -> StorageFaultProbeReport {
    let mut receipts = Vec::new();
    let mut push = |fire: &'static str, clean: &'static str, fires: u64| {
        receipts.push(PathReceipt {
            key: fire,
            fires,
            clean_controls: 1,
        });
        receipts.push(PathReceipt {
            key: clean,
            fires: 0,
            clean_controls: 1,
        });
    };

    reset_verified_faults();
    let artifact = run_artifact_ref_class(schedule);
    push(
        ARTIFACT_REF_FIRE,
        ARTIFACT_REF_CLEAN,
        take_verified_faults(),
    );

    reset_verified_faults();
    let split = run_split_class(schedule);
    push(SPLIT_FIRE, SPLIT_CLEAN, take_verified_faults());

    reset_verified_faults();
    let out_in = run_out_in_class(schedule);
    push(OUT_IN_FIRE, OUT_IN_CLEAN, take_verified_faults());

    reset_verified_faults();
    let root = run_root_replacement_class(schedule);
    push(ROOT_FIRE, ROOT_CLEAN, take_verified_faults());

    let mut qualification = run_qualification_probe(schedule.qualification_seed, false);
    let mapping = run_qualification_probe(schedule.qualification_seed, true);
    qualification.cells.extend(mapping.cells);
    qualification.commits = mapping.commits;
    for cell in &qualification.cells {
        let (fire, clean) = match cell.name.as_str() {
            "spill" => (
                "property-graph.storage-faults.spill.fire",
                "property-graph.storage-faults.spill.clean",
            ),
            "proof" => (
                "property-graph.storage-faults.proof.fire",
                "property-graph.storage-faults.proof.clean",
            ),
            "delete" => (
                "property-graph.storage-faults.delete.fire",
                "property-graph.storage-faults.delete.clean",
            ),
            "fold" => (
                "property-graph.storage-faults.fold.fire",
                "property-graph.storage-faults.fold.clean",
            ),
            "mapping-bit-flip" => (
                "property-graph.storage-faults.mapping-bit-flip.fire",
                "property-graph.storage-faults.mapping-bit-flip.clean",
            ),
            "mapping-WrongObject" => (
                "property-graph.storage-faults.mapping-WrongObject.fire",
                "property-graph.storage-faults.mapping-WrongObject.clean",
            ),
            "mapping-PostCommitError" => (
                "property-graph.storage-faults.mapping-PostCommitError.fire",
                "property-graph.storage-faults.mapping-PostCommitError.clean",
            ),
            other => panic!("unknown qualification receipt {other}"),
        };
        receipts.push(PathReceipt {
            key: fire,
            fires: cell.fires,
            clean_controls: cell.controls,
        });
        receipts.push(PathReceipt {
            key: clean,
            fires: 0,
            clean_controls: cell.controls,
        });
    }
    StorageFaultProbeReport {
        receipts,
        state: StorageFaultState {
            artifact_refusals: artifact.refusals,
            surviving_nodes: artifact.surviving,
            split_level: split.level,
            retained_split_level: split.retained_level,
            pre_split_keys: split.pre_split_keys,
            committed_keys: split.committed,
            old_root_keys: split.old_root,
            split_generations: split.generations,
            retained_root_digests: split.root_digests,
            unsplit_generations: split.unsplit_generations,
            unsplit_reopen_level: split.unsplit_reopen_level,
            split_refusals: split.refusals,
            out_rows: out_in.out_rows,
            in_rows: out_in.in_rows,
            out_in_refusals: out_in.refusals,
            root_refusals: root.refusals,
            reopened_generation: root.reopened_generation,
            previous_generation: root.previous_generation,
            qualification,
        },
    }
}

fn targeted_damage(vfs: &MisdirectVfs, target: &Path, substitute: Option<PathBuf>) {
    *vfs.target.lock().expect("target") = Some((target.to_path_buf(), substitute));
}

fn deletes(vfs: &MisdirectVfs) -> Vec<PathBuf> {
    vfs.recording
        .take()
        .into_iter()
        .filter_map(|event| match event {
            super::publication::DurabilityEvent::Delete(path) => Some(path),
            _ => None,
        })
        .collect()
}

fn protected_changes(before: &BTreeMap<PathBuf, Vec<u8>>, candidates: &[PathBuf]) -> u64 {
    before
        .iter()
        .filter(|(path, bytes)| {
            path.extension().is_some_and(|ext| ext == "zgraph")
                && !candidates.contains(path)
                && std::fs::read(path).ok().as_ref() != Some(*bytes)
        })
        .count() as u64
}

fn run_reclaim_cell(
    seed: u64,
    name: &str,
    fault: bool,
) -> crate::graph_storage_fault_test_support::QualificationCell {
    use super::consolidation::{
        commit_maintenance, pending_reclaim_proof_for_lease, reclaim_candidate_path,
        seed_reclaimable_manifest,
    };
    use crate::graph_storage_fault_test_support::QualificationCell;
    let parent = tempfile::tempdir().expect("reclaim qualification parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(MisdirectVfs::new());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = create_store(&path, &infrastructure);
    let mut cell = QualificationCell {
        name: name.into(),
        intent_preserved: true,
        ..Default::default()
    };
    if name == "spill" {
        for index in 0..3 {
            commit_node(&store, &format!("spill-{index}"), "mark input");
        }
        store
            .checkpoint_native_graph(&control())
            .expect("cut spill history");
        let before = file_snapshot(&path);
        let before_authority = store
            .admit_native_read()
            .expect("before failed mark")
            .bundle()
            .base();
        deletes(&vfs);
        let damaged = parent.path().join("damaged");
        let hook_vfs = vfs.clone();
        let hook_path = path.clone();
        let counts = Arc::new(Mutex::new((0, 0)));
        let observed = counts.clone();
        let mut armed = fault;
        let _guard = super::super::maintenance::spill::qualification::start(move |reference| {
            let merge = crate::property_graph::storage::reclaim::QUALIFICATION_MERGE
                .with(std::cell::Cell::get);
            if armed && merge.0 >= 2 && merge.1 > 0 {
                let target = crate::property_graph::storage::allocation::artifact_path(
                    &hook_path,
                    reference.object.artifact,
                );
                let mut bytes = std::fs::read(&target).expect("merge input bytes");
                let frame = crate::property_graph::storage::artifact::decode(
                    crate::property_graph::storage::artifact::ContainerKind::Object,
                    None,
                    &bytes,
                )
                .expect("merge input frame");
                if frame
                    .framed_block(reference.block)
                    .expect("merge input block")
                    .payload()
                    .get(..8)
                    != Some(b"ZGCP\x04\0\x01\0".as_slice())
                {
                    return;
                }
                let payload_length = frame
                    .framed_block(reference.block)
                    .expect("merge input block")
                    .payload()
                    .len();
                let offset = reference.block.offset as usize + reference.block.length as usize
                    - payload_length
                    + seed as usize % payload_length;
                println!(
                    "ZE-172 spill merge input offset={offset} payload_bytes={payload_length} runs={} merges={}",
                    merge.0, merge.1
                );
                bytes[offset] ^= 1;
                std::fs::write(&damaged, bytes).expect("damaged private merge input");
                targeted_damage(&hook_vfs, &target, Some(damaged.clone()));
                *observed.lock().expect("merge observations") = merge;
                armed = false;
            }
        });
        crate::property_graph::storage::reclaim::QUALIFICATION_MERGE
            .with(|counts| counts.set((0, 0)));
        let result = commit_maintenance(&store);
        let first_deletes = deletes(&vfs);
        if fault {
            cell.refusal = format!(
                "bit-flip:{}",
                classify_native_error(&result.expect_err("real merge refuses byte flip"))
            );
            cell.fires = vfs.fires();
            (cell.runs, cell.merges) = *counts.lock().expect("merge observations");
            cell.intent_preserved = store
                .admit_native_read()
                .expect("after failed mark")
                .bundle()
                .base()
                == before_authority;
            cell.changed_live_files = protected_changes(&before, &[]);
            cell.unauthorized_unlinks = first_deletes.len() as u64;
            drop(_guard);
            commit_maintenance(&store).expect("spill preparation resumes after restoration");
        } else {
            result.expect("same-seed clean mark/merge");
            (cell.runs, cell.merges) = crate::property_graph::storage::reclaim::QUALIFICATION_MERGE
                .with(std::cell::Cell::get);
        }
        cell.resumed = true;
        store.close().expect("close spill cell");
        reopen_store(&path, &infrastructure)
            .close()
            .expect("close reopened spill");
        return cell;
    }
    if name == "fold" {
        seed_reclaimable_manifest(&store, &format!("fold-seed-{seed}"));
        for index in 0..7 {
            commit_node(&store, &format!("fold-{index}"), "inventory fold");
        }
        assert_eq!(
            store
                .admit_native_read()
                .expect("before fold")
                .bundle()
                .prepared_inventories()
                .len(),
            8
        );
        store
            .checkpoint_native_graph(&control())
            .expect("cut fold history");
    } else {
        seed_reclaimable_manifest(&store, &format!("seed-{seed}"));
    }
    let previous_inventory = store
        .admit_native_read()
        .expect("inventory before folding")
        .bundle()
        .roots()
        .directory(TreeKind::ObjectInventory)
        .expect("previous inventory")
        .reference();
    commit_maintenance(&store).expect("commit pending intent and genuine fold");
    let pending = store.admit_native_read().expect("pending reader");
    let pending_root = pending.bundle().reclaim();
    let (manifest, candidates) = pending_reclaim_proof_for_lease(&store, &pending);
    let targets: Vec<_> = candidates
        .iter()
        .map(|candidate| reclaim_candidate_path(&path, candidate))
        .collect();
    assert!(targets.len() >= 2, "fixture must reach second candidate");
    let inventory = pending
        .bundle()
        .roots()
        .directory(TreeKind::ObjectInventory)
        .expect("fold inventory")
        .reference()
        .expect("rewritten inventory page");
    if name == "fold" {
        assert_ne!(
            Some(inventory),
            previous_inventory,
            "fold rewrote the targeted inventory page"
        );
        cell.folded = 8 + 1 - pending.bundle().prepared_inventories().len();
        assert_eq!(cell.folded, 8);
    }
    drop(pending);
    let before = file_snapshot(&path);
    deletes(&vfs);
    if fault {
        match name {
            "proof" => {
                let target = crate::property_graph::storage::allocation::artifact_path(
                    &path,
                    manifest.mark.root.object.artifact,
                );
                let mut bytes = std::fs::read(&target).expect("completed mark bytes");
                bytes.truncate(HEADER_BYTES + (seed as usize % (bytes.len() - HEADER_BYTES)));
                let damaged = parent.path().join("truncated");
                std::fs::write(&damaged, bytes).expect("truncated private mark copy");
                targeted_damage(&vfs, &target, Some(damaged));
            }
            "fold" => {
                let target = crate::property_graph::storage::allocation::artifact_path(
                    &path,
                    inventory.artifact,
                );
                let mut bytes = std::fs::read(&target).expect("rewritten inventory bytes");
                let frame = crate::property_graph::storage::artifact::decode(
                    crate::property_graph::storage::artifact::ContainerKind::Object,
                    None,
                    &bytes,
                )
                .expect("folded inventory pack");
                let payload = frame
                    .framed_block(inventory)
                    .expect("inventory block")
                    .payload();
                decode_page(TreeKind::ObjectInventory, payload)
                    .expect("actual rewritten ObjectInventory page");
                let payload_length = payload.len();
                let offset = inventory.offset as usize + inventory.length as usize - payload_length
                    + seed as usize % payload_length;
                println!(
                    "ZE-172 rewritten inventory offset={offset} payload_bytes={payload_length} folded={}",
                    cell.folded
                );
                bytes[offset] ^= 1;
                let damaged = parent.path().join("inventory-damage");
                std::fs::write(&damaged, bytes).expect("private damaged inventory");
                targeted_damage(&vfs, &target, Some(damaged));
            }
            "delete" => vfs.recording.arm_fault_after(FaultPoint::Delete, 1),
            _ => panic!("unknown reclaim cell"),
        }
        let result = commit_maintenance(&store);
        println!(
            "ZE-172 reclaim {name} seed={seed} fault fires={} target={:?} result={result:?}",
            vfs.fires(),
            vfs.target.lock().expect("target")
        );
        let error = result.expect_err("targeted reclaim fault must refuse");
        let initial = deletes(&vfs);
        if name == "delete" {
            vfs.recording.assert_fired_once();
            cell.fires = 1;
            assert_eq!(initial.len(), 1);
        } else {
            cell.fires = vfs.fires();
            assert!(initial.is_empty(), "byte refusal precedes unlink");
        }
        cell.refusal = format!("{name}:{}", classify_native_error(&error));
        cell.unauthorized_unlinks = initial
            .iter()
            .filter(|target| !targets.contains(target))
            .count() as u64;
        cell.changed_live_files = protected_changes(&before, &targets);
        cell.intent_preserved = store
            .admit_native_read()
            .expect("authority after refusal")
            .bundle()
            .reclaim()
            == pending_root;
        let report = commit_maintenance(&store).expect("resume remaining targets");
        let remaining = deletes(&vfs);
        cell.removed_bytes = report.removed_bytes;
        cell.unlinked_bytes = remaining
            .iter()
            .map(|target| before[target].len() as u64)
            .sum();
        let mut all = initial;
        all.extend(remaining);
        all.sort();
        let mut expected = targets.clone();
        expected.sort();
        cell.resumed = all == expected && targets.iter().all(|path| !path.exists());
    } else {
        let report = commit_maintenance(&store).expect("same-seed clean reclaim");
        let clean = deletes(&vfs);
        cell.removed_bytes = report.removed_bytes;
        cell.unlinked_bytes = clean.iter().map(|target| before[target].len() as u64).sum();
        let mut clean = clean;
        clean.sort();
        let mut expected = targets;
        expected.sort();
        cell.resumed = clean == expected;
        assert_eq!(
            cell.removed_bytes, cell.unlinked_bytes,
            "clean physical byte accounting"
        );
        assert!(cell.resumed);
    }
    store.close().expect("close reclaim cell");
    reopen_store(&path, &infrastructure)
        .close()
        .expect("close reopened reclaimed store");
    cell
}

fn classify_native_error(error: &super::super::NativeGraphError) -> &'static str {
    match error {
        super::super::NativeGraphError::Io { .. } => "Io",
        super::super::NativeGraphError::Read(error) => classify_tree_error(error),
        super::super::NativeGraphError::Invalid(_) => "Invalid",
        other => panic!("unexpected fault outcome: {other:?}"),
    }
}

fn run_scoped_mapping_fault_class(
    seed: u64,
) -> crate::graph_storage_fault_test_support::QualificationReport {
    use crate::graph_storage_fault_test_support::{QualificationCell, QualificationReport};
    use crate::property_graph::staging::{WriteLimits, WriteMemory};
    use crate::property_graph::storage::NativePreparationSource;
    use crate::property_graph::storage::mapping_slot_capture::Capture;
    use crate::property_graph::storage::memory::StorageMemory;
    let capture = Capture::start();
    let parent = tempfile::tempdir().expect("mapping parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(MisdirectVfs::new());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = create_store(&path, &infrastructure);
    capture.take();
    vfs.opens.lock().expect("log").clear();
    let mut commits = Vec::new();
    let mut references = Vec::new();
    let mut keys = Vec::new();
    for index in 0..66 {
        let key = format!("window-{index}");
        let node = commit_node(&store, &key, "mapping qualification");
        keys.push((key.into_bytes(), node));
        references.push(
            store
                .admit_native_read()
                .expect("pack reader")
                .bundle()
                .catalog()
                .block,
        );
        let reports = capture.take();
        for observed in &reports {
            assert!(observed.generation <= index as u64 + 1);
            assert!(observed.filled <= observed.capacity);
            if observed.kind
                == crate::property_graph::storage::mapping_slot_capture::Kind::Preparation
                && observed.capacity > 4
            {
                assert!(observed.filled <= 60);
            }
        }
        let logged = std::mem::take(&mut *vfs.opens.lock().expect("commit open log"));
        let opens = reports.iter().map(|report| report.opens).sum::<usize>();
        assert_eq!(
            opens,
            logged.len(),
            "commit {index} independent open counter"
        );
        commits.push(
            crate::graph_storage_fault_test_support::QualificationCommit {
                generation: store
                    .admit_native_read()
                    .expect("commit generation")
                    .bundle()
                    .base()
                    .generation
                    .get(),
                opens,
                logged_opens: logged.len(),
                filled: reports
                    .iter()
                    .filter(|report| report.capacity == 64)
                    .map(|report| report.filled)
                    .max()
                    .unwrap_or(0),
                scoped_opens: reports.iter().map(|report| report.scoped_opens).sum(),
            },
        );
    }
    let lease = store.admit_native_read().expect("mapping read lease");
    let nodes: Vec<_> = keys.iter().map(|(_, node)| *node).collect();
    let before = mapping_logical_state(&store, &nodes);
    let foreign_path = parent.path().join("foreign");
    let foreign = create_store(&foreign_path, &infrastructure);
    commit_node(&foreign, "foreign", "checksum valid foreign store");
    let foreign_reference = foreign
        .admit_native_read()
        .expect("foreign reader")
        .bundle()
        .catalog()
        .block;
    let foreign_file = crate::property_graph::storage::allocation::artifact_path(
        &foreign_path,
        foreign_reference.artifact,
    );
    let target =
        crate::property_graph::storage::allocation::artifact_path(&path, references[65].artifact);
    let original = std::fs::read(&target).expect("scoped target bytes");
    let mut report = QualificationReport {
        commits,
        ..Default::default()
    };
    for name in [
        "mapping-bit-flip",
        "mapping-WrongObject",
        "mapping-PostCommitError",
    ] {
        let damaged = parent.path().join("damaged-pack");
        let mut bytes = original.clone();
        let offset = payload_offset(&bytes, seed);
        bytes[offset] ^= 1;
        std::fs::write(&damaged, bytes).expect("damaged temporary file");
        let replacement = match name {
            "mapping-bit-flip" => Some(damaged),
            "mapping-WrongObject" => Some(foreign_file.clone()),
            _ => None,
        };
        let mut cell = QualificationCell {
            name: name.into(),
            intent_preserved: true,
            ..Default::default()
        };
        for fault in [true, false] {
            let shared = GraphResources::from_store(&store).expect("shared");
            let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
            let guard = control();
            let memory =
                StorageMemory::new(&writer, &guard, 32 * 1024 * 1024).expect("storage memory");
            let source =
                NativePreparationSource::new(&lease, &memory, 64).expect("64 slot fixture");
            let mut resources = source.resources(u64::MAX).expect("resources");
            vfs.opens.lock().expect("log").clear();
            let fires = vfs.fires();
            source.with_scoped_reads(|| {
                for reference in &references[..60] {
                    assert!(
                        !source.scoped_blocks(),
                        "prefix still pins before threshold"
                    );
                    source
                        .with_block(*reference, &mut resources, |block, _| {
                            assert_eq!(block.reference(), *reference);
                            Ok(())
                        })
                        .expect("fill unique pack");
                }
                assert!(source.scoped_blocks(), "actual scoped transition");
                assert_eq!(source.mapping_slot_report().filled, 60);
                if fault {
                    targeted_damage(&vfs, &target, replacement.clone());
                }
                let result = source.with_block(references[65], &mut resources, |block, _| {
                    if fault {
                        cell.exposed_blocks += 1;
                    }
                    assert_eq!(block.reference(), references[65]);
                    assert_eq!(block.identity().store, lease.bundle().base().store);
                    Ok(())
                });
                if fault {
                    let error = result.expect_err("scoped path validates target and refuses");
                    cell.refusal = format!("{name}:{}", classify_tree_error(&error));
                    cell.fires = vfs.fires() - fires;
                    let observed = source.mapping_slot_report();
                    cell.filled = observed.filled;
                    cell.opens = observed.opens;
                    cell.scoped_opens = observed.scoped_opens;
                    cell.logged_opens = vfs.opens.lock().expect("log").len();
                } else {
                    result.expect("same-seed scoped clean control");
                    let observed = source.mapping_slot_report();
                    assert_eq!(observed.opens, vfs.opens.lock().expect("clean log").len());
                    assert_eq!(
                        (observed.filled, observed.opens, observed.scoped_opens),
                        (60, 61, 1)
                    );
                    cell.controls += 1;
                }
            });
        }
        cell.changed_live_files =
            u64::from(std::fs::read(&target).expect("original target") != original);
        cell.resumed = mapping_logical_state(&store, &nodes) == before;
        report.cells.push(cell);
    }
    drop(lease);
    foreign.close().expect("close foreign");
    store.close().expect("close mapping store");
    capture.restore_default_capacity();
    let reopened = reopen_store(&path, &infrastructure);
    let lease = reopened
        .admit_native_read()
        .expect("reopened logical lease");
    assert_eq!(before, mapping_logical_state(&reopened, &nodes));
    drop(lease);
    reopened.close().expect("close reopen mapping");
    report
}

pub(crate) fn run_qualification_probe(
    seed: u64,
    mapping: bool,
) -> crate::graph_storage_fault_test_support::QualificationReport {
    if mapping {
        return run_scoped_mapping_fault_class(seed);
    }
    let mut report = crate::graph_storage_fault_test_support::QualificationReport::default();
    let clean_selection = run_selection_probe(seed);
    let mut selection = run_selection_probe(seed);
    selection.clean_serials = clean_selection.selected_serials;
    selection.clean_ranges = clean_selection.selected_ranges;
    report.selection = Some(selection);
    for name in ["spill", "proof", "delete", "fold"] {
        let clean = run_reclaim_cell(seed, name, false);
        assert!(clean.resumed, "same-seed clean execution {name}");
        let mut cell = run_reclaim_cell(seed, name, true);
        cell.controls = 1;
        report.cells.push(cell);
    }
    report
}

fn run_selection_probe(
    seed: u64,
) -> crate::graph_storage_fault_test_support::QualificationSelection {
    use crate::graph_storage_fault_test_support::{QualificationPack, QualificationSelection};
    use crate::property_graph::storage::adjacency::QUALIFICATION_RANGES;
    use crate::property_graph::storage::consolidation::{
        QUALIFICATION_PACKS, QUALIFICATION_SELECTION,
    };
    struct Capture;
    impl Drop for Capture {
        fn drop(&mut self) {
            QUALIFICATION_SELECTION.with(|active| active.set(false));
        }
    }
    let parent = tempfile::tempdir().expect("selection fixture");
    let path = parent.path().join("native");
    let vfs: Arc<dyn Vfs> = Arc::new(RecordingVfs::default());
    let store = create_store(&path, &vfs);
    let mut groups = Vec::new();
    let mut pending_counts = Vec::new();
    // Each group has its own real base and independently sized pending deltas.
    // Rotate creation order so oldest selection is seed-dependent too.
    for order in 0..3 {
        let group = (order + seed as usize % 3) % 3;
        let (source, target, _) = commit_linked_pair(&store, &format!("selection-{group}"));
        let pending = 1 + ((group + seed as usize % 3) % 3);
        groups.push((group, source, target));
        pending_counts.push((group, pending));
        for index in 0..pending {
            let key = format!("selection-{group}-{index}");
            store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "ze47", &key)
                            .expect("selection rel key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Existing(source),
                            target: NodeRef::Existing(target),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    }],
                    &control(),
                )
                .expect("seed-sized pending delta");
        }
    }
    store
        .checkpoint_native_graph(&control())
        .expect("selection checkpoint");
    QUALIFICATION_PACKS.with(|packs| packs.borrow_mut().clear());
    QUALIFICATION_RANGES.with(|ranges| ranges.borrow_mut().clear());
    QUALIFICATION_SELECTION.with(|active| active.set(true));
    let capture = Capture;
    let admission = store
        .admit_native_graph_maintenance()
        .expect("selection maintenance");
    super::super::maintenance::commit_with_limits(
        &store,
        &admission,
        &control(),
        super::super::maintenance::MaintenanceLimits {
            relocation_bytes: 1,
            ..Default::default()
        },
    )
    .expect("oldest pack and largest pending range maintenance");
    drop(admission);
    drop(capture);
    let packs = QUALIFICATION_PACKS.with(|packs| std::mem::take(&mut *packs.borrow_mut()));
    assert_eq!(packs.len(), 1, "one actual pack selection");
    let (census, serials, byte_limit) = packs.into_iter().next().expect("pack observation");
    let mut observed = QualificationSelection {
        packs: census
            .into_iter()
            .map(|row| QualificationPack {
                serial: row.serial,
                bytes: row.bytes,
                live: row.live,
                pages: row.live_pages,
                records: row.live_records,
                graph_live: row.graph_live,
            })
            .collect(),
        selected_serials: serials,
        byte_limit,
        ..Default::default()
    };
    pending_counts.sort();
    observed.fixture_pending = pending_counts.into_iter().map(|(_, count)| count).collect();
    let range_group = |kind: TreeKind, key: [u8; 40]| {
        let node = u128::from_le_bytes(key[..16].try_into().expect("range node identity"));
        groups
            .iter()
            .find(|(_, source, target)| {
                if kind == TreeKind::OutRanges {
                    source.get() == node
                } else {
                    target.get() == node
                }
            })
            .expect("known fixture range")
            .0
    };
    for (kind, candidates, selected) in
        QUALIFICATION_RANGES.with(|ranges| std::mem::take(&mut *ranges.borrow_mut()))
    {
        for (key, count) in candidates {
            observed
                .ranges
                .push((kind as u8, range_group(kind, key), count));
        }
        if let Some(key) = selected {
            observed
                .selected_ranges
                .push((kind as u8, range_group(kind, key)));
        }
    }
    assert!(
        !observed.ranges.is_empty(),
        "fixture exercises pending selection after oldest pack drain"
    );
    store.close().expect("close selection store");
    observed
}

fn mapping_logical_state(store: &Store, nodes: &[NodeId]) -> Vec<super::mapping_slots::NodeState> {
    nodes
        .chunks(8)
        .flat_map(|chunk| super::mapping_slots::logical_state(store, chunk))
        .collect()
}

#[cfg(test)]
#[test]
fn ze177_reopen_phase_report_is_complete() {
    ze177_measure_reopen(false);
}

#[cfg(test)]
#[test]
#[ignore = "ZE-177 release measurement"]
fn ze177_profile_forty_artifact_reopen() {
    ze177_measure_reopen(true);
}

#[cfg(test)]
fn ze177_measure_reopen(large: bool) {
    let parent = tempfile::tempdir().expect("fixture parent");
    let path = parent.path().join("native");
    let infrastructure: Arc<dyn Vfs> = Arc::new(StdVfs);
    let write_start = std::time::Instant::now();
    let store = create_store(&path, &infrastructure);
    let names = split_key_names(if large { 4096 } else { 16 });
    let mut nodes = Vec::new();
    let inventory = || {
        let files: Vec<_> = std::fs::read_dir(&path)
            .expect("inventory")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| {
                path.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .starts_with("graph-")
                    && path.extension().is_some_and(|ext| ext == "zgraph")
            })
            .collect();
        let bytes: u64 = files
            .iter()
            .map(|path| std::fs::metadata(path).expect("metadata").len())
            .sum();
        (files.len(), bytes)
    };
    for batch in names.chunks(16) {
        nodes.extend(commit_keyed_nodes(&store, batch, None));
        if !large || inventory().0 >= 42 {
            break;
        }
    }
    let (artifacts, bytes) = inventory();
    if large {
        assert!(
            (40..=50).contains(&artifacts),
            "forty-artifact scale: {artifacts}"
        );
    }
    let lease = store.admit_native_read().expect("lease");
    let generation = lease.bundle().roots().generation().get();
    drop(lease);
    let write_time = write_start.elapsed();
    store.close().expect("close");
    drop(store);
    let wal_bytes: u64 = std::fs::read_dir(&path)
        .expect("files")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with("graph-wal-")
        })
        .map(|path| std::fs::metadata(path).expect("wal metadata").len())
        .sum();
    let start = std::time::Instant::now();
    let reopened = reopen_store(&path, &infrastructure);
    let total = start.elapsed();
    let report = super::super::recovery::reopen_profile_for_test();
    assert_eq!(
        report.phases.len(),
        6,
        "missing phase observations: {report:?}"
    );
    assert!(report.nested.iter().any(|(name, _, _)| *name == "mapping"));
    assert!(
        report
            .nested
            .iter()
            .any(|(name, _, _)| *name == "artifact_validation")
    );
    let lease = reopened.admit_native_read().expect("reopened lease");
    assert_eq!(lease.bundle().roots().generation().get(), generation);
    drop(lease);
    for node in &nodes {
        assert_eq!(lookup(&reopened, *node).expect("lookup"), Some(1));
    }
    println!(
        "ZE177 artifacts={artifacts} artifact_bytes={bytes} nodes={} wal_bytes={wal_bytes} generation={generation} write_ms={:.6} reopen_ms={:.6}",
        nodes.len(),
        write_time.as_secs_f64() * 1000.0,
        total.as_secs_f64() * 1000.0
    );
    for (name, elapsed) in &report.phases {
        println!(
            "ZE177 phase={name} ms={:.6} share={:.9}",
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() / total.as_secs_f64()
        );
    }
    for name in [
        "mapping",
        "artifact_validation",
        "validate_complete_reclaim_authority",
        "validate_captured_state_reachability",
    ] {
        let events: Vec<_> = report
            .nested
            .iter()
            .filter(|(kind, _, _)| *kind == name)
            .collect();
        let seconds: f64 = events
            .iter()
            .map(|(_, elapsed, _)| elapsed.as_secs_f64())
            .sum();
        let bytes: usize = events.iter().map(|(_, _, bytes)| bytes).sum();
        println!(
            "ZE177 nested={name} calls={} bytes={bytes} ms={:.6} share={:.9}",
            events.len(),
            seconds * 1000.0,
            seconds / total.as_secs_f64()
        );
    }
    println!("ZE177 correctness=passed");
    reopened.close().expect("close reopened");
}
