use super::tempfile;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::staging::{
    ResultLayout, ResultMaterializer, ResultRegistration, StageError, StructuredOperation,
    StructuredWrite, WriteImage, WritePhase,
};
use crate::property_graph::storage::adjacency::RelationshipRow;
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    CursorState, DirectionSelection, GraphReadView, NativeCatalog, NativeQuerySource,
    NativeReadCapability, RelationshipTypeSelection,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphName, GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
};
use crate::vfs::{StdVfs, SyncKind, Vfs, VfsFile};
use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};

thread_local! { static VERIFIED_FAULTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

pub(super) fn reset_verified_faults() {
    VERIFIED_FAULTS.with(|count| count.set(0));
}

pub(super) fn take_verified_faults() -> u64 {
    VERIFIED_FAULTS.with(|count| count.replace(0))
}

/// Count one fired refusal that no VFS fault point produced (a budget, a
/// cancellation, a planted omission), after its assertions passed.
pub(super) fn record_verified_fault() {
    VERIFIED_FAULTS.with(|count| count.set(count.get() + 1));
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DurabilityEvent {
    Create(PathBuf),
    Write(PathBuf),
    OpenAppend(PathBuf),
    Append(PathBuf),
    Sync(PathBuf, SyncKind),
    Rename(PathBuf, PathBuf),
    Delete(PathBuf),
    Published(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FaultPoint {
    Create,
    PartialCreate,
    ObjectSync,
    DirectorySync,
    Append,
    PartialAppend,
    WalSync,
    Rename,
    Publish,
    OpenAppend,
    SelectorSync,
    ManifestSync,
    Delete,
}

#[derive(Default)]
struct FaultSchedule {
    armed: Option<FaultPoint>,
    /// Matching path-level operations to let through before firing.
    skip: u64,
    fires: u64,
}

type SyncGate = Arc<Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>>;

#[derive(Default)]
pub(crate) struct RecordingVfs {
    events: Arc<Mutex<Vec<DurabilityEvent>>>,
    wal_sync_gate: SyncGate,
    faults: Arc<Mutex<FaultSchedule>>,
    after_create: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    after_manifest_version: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    list_calls: AtomicU64,
    child_calls: AtomicU64,
}

struct RecordingFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    events: Arc<Mutex<Vec<DurabilityEvent>>>,
    wal_sync_gate: SyncGate,
    faults: Arc<Mutex<FaultSchedule>>,
}

impl RecordingVfs {
    pub(super) fn after_manifest_version(&self, action: impl FnOnce() + Send + 'static) {
        *self.after_manifest_version.lock().unwrap() = Some(Box::new(action));
    }

    pub(super) fn take(&self) -> Vec<DurabilityEvent> {
        std::mem::take(&mut *self.events.lock().expect("recording VFS events"))
    }

    pub(crate) fn clear_events(&self) {
        self.events.lock().expect("recording VFS events").clear();
    }

    /// Actual Full-sync ordinal of the first directory protection or final
    /// checkpoint selector directory sync in a completed control.
    pub(crate) fn directory_sync_ordinal(&self, selector: bool) -> usize {
        let events = self.events.lock().expect("recording VFS events");
        let syncs: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                DurabilityEvent::Sync(path, crate::vfs::SyncKind::Full) => Some(path),
                _ => None,
            })
            .collect();
        let index = if selector {
            syncs.iter().rposition(|path| path.is_dir())
        } else {
            syncs.iter().position(|path| path.is_dir())
        };
        index.expect("control completed its directory boundary") + 1
    }

    fn record(&self, event: DurabilityEvent) {
        self.events
            .lock()
            .expect("recording VFS events")
            .push(event);
    }

    pub(crate) fn arm_wal_full_sync(&self) -> (Arc<Barrier>, Arc<Barrier>) {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *self.wal_sync_gate.lock().expect("WAL sync gate") =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        (entered, release)
    }

    pub(crate) fn arm_fault(&self, point: FaultPoint) {
        self.arm_fault_after(point, 0);
    }

    /// Fires on the matching operation after `skip` earlier ones succeed.
    /// Only path-level points (create, sync, rename, delete) honor `skip`.
    pub(super) fn arm_fault_after(&self, point: FaultPoint, skip: u64) {
        let mut faults = self.faults.lock().expect("fault schedule");
        assert!(faults.armed.replace(point).is_none());
        faults.skip = skip;
        faults.fires = 0;
    }

    fn fire(&self, point: FaultPoint) -> bool {
        let mut faults = self.faults.lock().expect("fault schedule");
        if faults.armed == Some(point) && faults.skip > 0 {
            faults.skip -= 1;
            false
        } else if faults.armed == Some(point) {
            faults.armed = None;
            faults.fires += 1;
            true
        } else {
            false
        }
    }

    /// Runs `action` once, on the creating thread, right after the next
    /// successful create: the caller's first private file is then on disk.
    pub(crate) fn after_next_create(&self, action: impl FnOnce() + Send + 'static) {
        let previous = self
            .after_create
            .lock()
            .expect("after-create hook")
            .replace(Box::new(action));
        assert!(previous.is_none());
    }

    pub(crate) fn after_create_is_armed(&self) -> bool {
        self.after_create
            .lock()
            .expect("after-create hook")
            .is_some()
    }

    pub(crate) fn assert_fired_once(&self) {
        let faults = self.faults.lock().expect("fault schedule");
        assert_eq!(faults.fires, 1);
        assert!(faults.armed.is_none());
        VERIFIED_FAULTS.with(|count| count.set(count.get() + faults.fires));
    }

    pub(super) fn enumeration_calls(&self) -> (u64, u64) {
        (
            self.list_calls.load(Ordering::Relaxed),
            self.child_calls.load(Ordering::Relaxed),
        )
    }
}

impl VfsFile for RecordingFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut faults = self.faults.lock().expect("fault schedule");
        if faults.armed == Some(FaultPoint::Append) {
            faults.armed = None;
            faults.fires += 1;
            return Err(std::io::Error::other("scheduled WAL append failure"));
        }
        if faults.armed == Some(FaultPoint::PartialAppend) {
            faults.armed = None;
            faults.fires += 1;
            let half = bytes.len() / 2;
            self.inner.append(&bytes[..half])?;
            return Err(std::io::Error::other(
                "scheduled partial WAL append failure",
            ));
        }
        drop(faults);
        self.inner.append(bytes)?;
        self.events
            .lock()
            .expect("recording VFS events")
            .push(DurabilityEvent::Append(self.path.clone()));
        Ok(())
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        if matches!(
            self.faults.lock().expect("fault schedule").armed,
            Some(FaultPoint::Append | FaultPoint::PartialAppend)
        ) {
            let bytes: Vec<_> = buffers
                .iter()
                .flat_map(|buffer| buffer.iter().copied())
                .collect();
            return self.append(&bytes);
        }
        self.inner.append_vectored(buffers)?;
        self.events
            .lock()
            .expect("recording VFS events")
            .push(DurabilityEvent::Append(self.path.clone()));
        Ok(())
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        if kind == SyncKind::Full && self.path.file_name().is_some_and(|name| name == "wal.ze") {
            let mut faults = self.faults.lock().expect("fault schedule");
            if faults.armed == Some(FaultPoint::WalSync) {
                faults.armed = None;
                faults.fires += 1;
                return Err(std::io::Error::other("scheduled WAL Full-sync failure"));
            }
        }
        self.inner.sync(kind)?;
        self.events
            .lock()
            .expect("recording VFS events")
            .push(DurabilityEvent::Sync(self.path.clone(), kind));
        if kind == SyncKind::Full && self.path.file_name().is_some_and(|name| name == "wal.ze") {
            let gate = self.wal_sync_gate.lock().expect("WAL sync gate").take();
            if let Some((entered, release)) = gate {
                entered.wait();
                release.wait();
            }
        }
        Ok(())
    }
}

impl Vfs for RecordingVfs {
    fn segment_data_read_counter(&self) -> Option<Arc<AtomicU64>> {
        StdVfs.segment_data_read_counter()
    }

    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
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
        let bytes = StdVfs.read_range(path, offset, length)?;
        if path.file_name().is_some_and(|name| name == "manifest.ze") && offset == 10 && length == 2
        {
            let action = self.after_manifest_version.lock().unwrap().take();
            if let Some(action) = action {
                action();
            }
        }
        Ok(bytes)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.write(path, bytes)?;
        self.record(DurabilityEvent::Write(path.to_path_buf()));
        Ok(())
    }

    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if self.fire(FaultPoint::Create) {
            StdVfs.create_new(path, b"foreign-owner")?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "scheduled create collision",
            ));
        }
        if self.fire(FaultPoint::PartialCreate) {
            StdVfs.create_new(path, &bytes[..bytes.len() / 2])?;
            return Err(std::io::Error::other("scheduled partial create failure"));
        }
        StdVfs.create_new(path, bytes)?;
        self.record(DurabilityEvent::Create(path.to_path_buf()));
        let action = self.after_create.lock().expect("after-create hook").take();
        if let Some(action) = action {
            action();
        }
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        if self.fire(FaultPoint::OpenAppend) {
            return Err(std::io::Error::other(
                "scheduled append-handle open failure",
            ));
        }
        let file = Box::new(RecordingFile {
            inner: StdVfs.open_append(path)?,
            path: path.to_path_buf(),
            events: Arc::clone(&self.events),
            wal_sync_gate: Arc::clone(&self.wal_sync_gate),
            faults: Arc::clone(&self.faults),
        });
        self.record(DurabilityEvent::OpenAppend(path.to_path_buf()));
        Ok(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if self.fire(FaultPoint::Rename) {
            return Err(std::io::Error::other("scheduled rename failure"));
        }
        StdVfs.rename(from, to)?;
        self.record(DurabilityEvent::Rename(
            from.to_path_buf(),
            to.to_path_buf(),
        ));
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        if kind == SyncKind::Full {
            let selected = path.is_dir() && self.events.lock().unwrap().last().is_some_and(|event|
                matches!(event, DurabilityEvent::Rename(_, to) if to.file_name().is_some_and(|name| name == "manifest.ze")));
            if selected && self.fire(FaultPoint::SelectorSync) {
                return Err(std::io::Error::other(
                    "scheduled selector directory sync failure",
                ));
            }
            let point = if path.is_dir() {
                Some(FaultPoint::DirectorySync)
            } else if path
                .extension()
                .is_some_and(|extension| extension == "zgraph")
            {
                Some(FaultPoint::ObjectSync)
            } else if path
                .file_name()
                .is_some_and(|name| name == ".manifest.ze.tmp")
            {
                Some(FaultPoint::ManifestSync)
            } else {
                None
            };
            if point.is_some_and(|point| self.fire(point)) {
                return Err(std::io::Error::other("scheduled path Full-sync failure"));
            }
        }
        StdVfs.sync(path, kind)?;
        self.record(DurabilityEvent::Sync(path.to_path_buf(), kind));
        Ok(())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.list_calls.fetch_add(1, Ordering::Relaxed);
        StdVfs.list(directory)
    }

    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.child_calls.fetch_add(1, Ordering::Relaxed);
        StdVfs.for_each_direct_child(directory, visitor)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        if self.fire(FaultPoint::Delete) {
            return Err(std::io::Error::other("scheduled delete failure"));
        }
        StdVfs.delete(path)?;
        self.record(DurabilityEvent::Delete(path.to_path_buf()));
        Ok(())
    }
}

struct ObserveMixed {
    first: NodeId,
    second: NodeId,
    relationship: RelId,
    vector_bits: [u32; 2],
}

impl super::super::NativeReadConsumer<Vec<RelationshipRow>> for ObserveMixed {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<RelationshipRow>, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        let first = view
            .lookup_node(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing first committed node"))?;
        let second = view
            .lookup_node(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing second committed node"))?;
        assert_eq!(first.record().revision().get(), 1);
        assert_eq!(second.record().revision().get(), 1);
        let text = view
            .stored_text(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing committed text"))?;
        let mut text_bytes = [0_u8; 10];
        assert_eq!(text.len(), 10);
        assert_eq!(text.read_at(0, &mut text_bytes, &mut resources)?, 10);
        assert_eq!(&text_bytes, b"first text");
        assert!(view.stored_text(self.second, &mut resources)?.is_none());
        assert!(view.vector_payload(self.first, &mut resources)?.is_none());
        let vector = view
            .vector_payload(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing committed vector"))?;
        assert_eq!(vector.dimensions(), 2);
        assert_eq!(
            vector.coordinate(0, &mut resources)?.to_bits(),
            self.vector_bits[0]
        );
        assert_eq!(
            vector.coordinate(1, &mut resources)?.to_bits(),
            self.vector_bits[1]
        );
        let relationship = view
            .lookup_relationship(self.relationship, &mut resources)?
            .ok_or(TreeError::Invalid("missing committed relationship"))?;
        assert_eq!(relationship.row().source, self.first);
        assert_eq!(relationship.row().target, self.second);
        let relationship_row = relationship.row();
        drop(resources);

        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        assert_eq!((sparse.text_count(), sparse.vector_count()), (1, 1));
        assert!(
            sparse
                .lookup(Modality::Text, self.first, &mut resources)?
                .is_some()
        );
        assert!(
            sparse
                .lookup(Modality::Text, self.second, &mut resources)?
                .is_none()
        );
        assert!(
            sparse
                .lookup(Modality::Vector, self.first, &mut resources)?
                .is_none()
        );
        let vector_member = sparse
            .lookup(Modality::Vector, self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing exact vector membership"))?;
        let stored = vector_member
            .vector
            .ok_or(TreeError::Invalid("missing sparse vector payload"))?;
        assert_eq!(
            stored.coordinate(0, &mut resources)?.to_bits(),
            self.vector_bits[0]
        );
        assert_eq!(
            stored.coordinate(1, &mut resources)?.to_bits(),
            self.vector_bits[1]
        );
        drop(resources);
        drop(sparse);

        let mut observed = Vec::new();
        for (node, direction) in [
            (self.first, DirectionSelection::Out),
            (self.second, DirectionSelection::In),
        ] {
            let mut cursor =
                view.expansion_cursor(node, direction, RelationshipTypeSelection::All, runtime)?;
            let mut output = [relationship_row];
            let (count, state) = view.expand(&mut cursor, &mut output, runtime)?;
            assert_eq!(count, 1);
            assert_eq!(output[0], relationship_row);
            if node == self.first {
                observed.push(output[0]);
            }
            if state == CursorState::More {
                let (count, state) = view.expand(&mut cursor, &mut output, runtime)?;
                assert_eq!((count, state), (0, CursorState::Done));
            }
        }
        Ok(observed)
    }
}

fn run_ze39_fresh_mixed_commit_is_durable_and_coherent() -> Vec<RelationshipRow> {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze39-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x39, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    assert!(
        path.join("wal.ze").exists(),
        "graph and documents share the Store WAL"
    );
    let creation = vfs.take();
    assert!(creation.iter().any(|event| {
        matches!(event, DurabilityEvent::Sync(target, SyncKind::Full) if target == parent.path())
    }));

    let coordinates = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let label = GraphName::new("Document").expect("label");
        let mut labels = [label];
        let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
        let mut properties = property_fixture();
        let first = CanonicalContents::node(&mut labels, &mut properties, Some("first text"), None)
            .expect("first node");
        let second =
            CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("second node");
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "ab")
                    .expect("relationship key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).expect("node slot")),
                    target: NodeRef::Local(refs.node(1).expect("node slot")),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("mixed graph commit must publish")
    });
    assert_eq!(receipts.len(), 3);
    let first = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("first receipt changed identity domain"),
    };
    let second = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("second receipt changed identity domain"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt changed identity domain"),
    };

    let observed = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveMixed {
                first,
                second,
                relationship,
                vector_bits: coordinates.map(f32::to_bits),
            },
        )
        .expect("one coherent mixed GraphReadView");
    check_native_property_adapter(&store, first);
    vfs.record(DurabilityEvent::Published(1));

    let events = vfs.take();
    let directory_sync = events
        .iter()
        .position(|event| {
            matches!(event, DurabilityEvent::Sync(target, SyncKind::Full) if target == &path)
        })
        .expect("graph directory Full sync");
    let append = events
        .iter()
        .position(|event| matches!(event, DurabilityEvent::Append(_)))
        .expect("complete WAL append");
    let wal_sync = events
        .iter()
        .position(|event| {
            matches!(event, DurabilityEvent::Sync(target, SyncKind::Full) if target.file_name().is_some_and(|name| name == "wal.ze"))
        })
        .expect("WAL Full sync");
    let publication = events
        .iter()
        .position(|event| *event == DurabilityEvent::Published(1))
        .expect("publication observation");
    assert!(directory_sync < append && append < wal_sync && wal_sync < publication);
    for (index, event) in events.iter().enumerate() {
        if let DurabilityEvent::Create(path) = event {
            assert_eq!(
                events.get(index + 1),
                Some(&DurabilityEvent::Sync(path.clone(), SyncKind::Full)),
                "each immutable file is Full-synced before later commit work"
            );
        }
    }
    store.close().expect("close native store");
    observed
}

pub(super) fn property_fixture() -> Vec<GraphProperty<'static>> {
    [
        ("s", PropertyData::String("property\0text")),
        ("b", PropertyData::Bool(true)),
        ("i", PropertyData::I64(i64::MIN)),
        (
            "f",
            PropertyData::F64(f64::from_bits(0x7ff8_0000_0000_0039)),
        ),
        ("e", PropertyData::EmptyList { count: 0 }),
        ("ss", PropertyData::Strings(&["a", "", "z\0q"])),
        ("bb", PropertyData::Bools(&[true, false])),
        ("ii", PropertyData::Integers(&[i64::MIN, 39, i64::MAX])),
        ("ff", PropertyData::Floats(&[-0.0, f64::INFINITY])),
    ]
    .into_iter()
    .map(|(name, data)| {
        GraphProperty::new(
            GraphName::new(name).unwrap(),
            PropertyValue::new(data).unwrap(),
        )
    })
    .collect()
}

fn check_native_property_adapter(store: &Store, node: NodeId) {
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::staging::{AdmittedBase, WriteLimits, WriteMemory};
    use crate::property_graph::storage::{NativePreparationSource, memory::StorageMemory};
    let lease = store.admit_native_read().unwrap();
    let shared = GraphResources::from_store(store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(&lease, &storage, 32).unwrap();
    let mut resources = source.resources(64 * 1024 * 1024).unwrap();
    let cell = std::cell::RefCell::new(&mut resources);
    let error = std::cell::Cell::new(None);
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
        revision: GraphRevision::new(2).unwrap(),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: None,
    }];
    let base = super::super::base::NativeAdmittedBase::new(
        &lease, &source, &storage, &request, &cell, &error,
    )
    .unwrap();
    for property in property_fixture() {
        let actual = base
            .property(EntityId::Node(node), property.name(), &mut |_| Ok(()))
            .unwrap()
            .unwrap()
            .data();
        match (actual, property.value().data()) {
            (PropertyData::String(a), PropertyData::String(b)) => assert_eq!(a, b),
            (PropertyData::Bool(a), PropertyData::Bool(b)) => assert_eq!(a, b),
            (PropertyData::I64(a), PropertyData::I64(b)) => assert_eq!(a, b),
            (PropertyData::F64(a), PropertyData::F64(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (PropertyData::EmptyList { count: a }, PropertyData::EmptyList { count: b }) => {
                assert_eq!(a, b)
            }
            (PropertyData::Strings(a), PropertyData::Strings(b)) => assert_eq!(a, b),
            (PropertyData::Bools(a), PropertyData::Bools(b)) => assert_eq!(a, b),
            (PropertyData::Integers(a), PropertyData::Integers(b)) => assert_eq!(a, b),
            (PropertyData::Floats(a), PropertyData::Floats(b)) => assert_eq!(
                a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            ),
            _ => panic!("property tag changed"),
        }
    }
    assert!(
        base.property(
            EntityId::Node(node),
            GraphName::new("missing").unwrap(),
            &mut |_| Ok(())
        )
        .unwrap()
        .is_none()
    );
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct GenerationSnapshot {
    pub(super) generation: u64,
    pub(super) revision: u64,
    pub(super) original_generation: u64,
    pub(super) canonical: Vec<u8>,
    pub(super) text: Vec<u8>,
    pub(super) vector: Vec<u32>,
    pub(super) old_relationship: bool,
    pub(super) new_relationship: bool,
    pub(super) out: Vec<RelId>,
    pub(super) incoming: Vec<RelId>,
    pub(super) sparse_text: bool,
    pub(super) sparse_vector: bool,
}

pub(super) fn snapshot_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: NodeId,
    peer: NodeId,
    old_relationship: RelId,
    new_relationship: Option<RelId>,
) -> GenerationSnapshot {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("initial resources");
    let source = NativeQuerySource::new(capability, &resources, 16).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained view");

    let mut resources = TreeResources::for_query(&mut runtime).expect("record resources");
    let record = view
        .lookup_node(node, &mut resources)
        .expect("node lookup")
        .expect("live node");
    let mut canonical = vec![0_u8; record.record().canonical_bytes().len() as usize];
    let canonical_length = canonical.len();
    assert_eq!(
        record
            .record()
            .canonical_bytes()
            .read_at(0, &mut canonical, &mut resources)
            .expect("canonical read"),
        canonical_length
    );
    let text_reader = view
        .stored_text(node, &mut resources)
        .expect("text lookup")
        .expect("stored text");
    let mut text = vec![0_u8; text_reader.len() as usize];
    let text_length = text.len();
    assert_eq!(
        text_reader
            .read_at(0, &mut text, &mut resources)
            .expect("text read"),
        text_length
    );
    let vector_reader = view
        .vector_payload(node, &mut resources)
        .expect("vector lookup")
        .expect("stored vector");
    let mut vector = Vec::new();
    for index in 0..vector_reader.dimensions() {
        vector.push(
            vector_reader
                .coordinate(index, &mut resources)
                .expect("vector coordinate")
                .to_bits(),
        );
    }
    let old_present = view
        .lookup_relationship(old_relationship, &mut resources)
        .expect("old relationship lookup")
        .is_some();
    let new_present = new_relationship.is_some_and(|relationship| {
        view.lookup_relationship(relationship, &mut resources)
            .expect("new relationship lookup")
            .is_some()
    });
    drop(resources);

    let mut out_cursor = view
        .expansion_cursor(
            node,
            DirectionSelection::Out,
            RelationshipTypeSelection::All,
            &mut runtime,
        )
        .expect("OUT cursor");
    let sentinel = RelationshipRow {
        rel: RelId::new(u128::MAX).expect("sentinel relationship"),
        source: node,
        target: peer,
        relationship_type: crate::property_graph::catalog::RelTypeId::new(u64::MAX)
            .expect("sentinel type"),
    };
    let mut rows = [sentinel; 4];
    let (count, state) = view
        .expand(&mut out_cursor, &mut rows, &mut runtime)
        .expect("OUT expansion");
    assert_eq!(state, CursorState::Done);
    let out = rows[..count].iter().map(|row| row.rel).collect();
    drop(out_cursor);

    let mut in_cursor = view
        .expansion_cursor(
            peer,
            DirectionSelection::In,
            RelationshipTypeSelection::All,
            &mut runtime,
        )
        .expect("IN cursor");
    let (count, state) = view
        .expand(&mut in_cursor, &mut rows, &mut runtime)
        .expect("IN expansion");
    assert_eq!(state, CursorState::Done);
    let incoming = rows[..count].iter().map(|row| row.rel).collect();
    drop(in_cursor);

    let sparse = view.sparse_view(&mut runtime).expect("sparse view");
    let mut resources = TreeResources::for_query(&mut runtime).expect("sparse resources");
    let sparse_text = sparse
        .lookup(Modality::Text, node, &mut resources)
        .expect("text membership")
        .is_some();
    let sparse_vector = sparse
        .lookup(Modality::Vector, node, &mut resources)
        .expect("vector membership")
        .is_some();
    GenerationSnapshot {
        generation: lease.bundle().base().generation.get(),
        revision: record.record().revision().get(),
        original_generation: record.record().provenance().original_generation().get(),
        canonical,
        text,
        vector,
        old_relationship: old_present,
        new_relationship: new_present,
        out,
        incoming,
        sparse_text,
        sparse_vector,
    }
}

/// Actual OUT expansion rows of `node`, through the public read view.
pub(super) fn out_rows_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: NodeId,
    peer: NodeId,
) -> Vec<RelationshipRow> {
    rows_for_lease(store, lease, node, peer, DirectionSelection::Out)
}

/// Actual expansion rows of `node` in one direction, through the public view.
pub(super) fn rows_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: NodeId,
    peer: NodeId,
    direction: DirectionSelection,
) -> Vec<RelationshipRow> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("initial resources");
    let source = NativeQuerySource::new(capability, &resources, 16).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained view");
    let mut cursor = view
        .expansion_cursor(
            node,
            direction,
            RelationshipTypeSelection::All,
            &mut runtime,
        )
        .expect("expansion cursor");
    let sentinel = RelationshipRow {
        rel: RelId::new(u128::MAX).expect("sentinel relationship"),
        source: node,
        target: peer,
        relationship_type: crate::property_graph::catalog::RelTypeId::new(u64::MAX)
            .expect("sentinel type"),
    };
    let mut rows = [sentinel; 4];
    let (count, state) = view
        .expand(&mut cursor, &mut rows, &mut runtime)
        .expect("OUT expansion");
    assert_eq!(state, CursorState::Done);
    rows[..count].to_vec()
}

pub(super) fn sparse_physical_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: NodeId,
    modality: Modality,
) -> crate::property_graph::storage::search::SparsePhysicalSnapshot {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("initial resources");
    let source = NativeQuerySource::new(capability, &resources, 16).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained view");
    let sparse = view.sparse_view(&mut runtime).expect("sparse view");
    let mut resources = TreeResources::for_query(&mut runtime).expect("sparse resources");
    sparse
        .physical_snapshot(modality, node, &mut resources)
        .expect("physical sparse lookup")
        .expect("physical sparse member")
}

fn run_ze39_retained_reader_and_new_admission_observe_whole_generations() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze39-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x39, 0xb5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Arc::new(
        Store::create_native_graph_with_infrastructure(
            &path,
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            Some(document.clone()),
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .expect("fresh native store"),
    );
    vfs.take();

    let old_coordinates = [f32::from_bits(0x3f00_0001), f32::from_bits(0x3f80_0001)];
    let mut old_properties = [GraphProperty::new(
        GraphName::new("rank").expect("property name"),
        PropertyValue::new(PropertyData::I64(1)).expect("property value"),
    )];
    let old_embedding = CanonicalEmbedding::new(&document, &old_coordinates).expect("embedding");
    let old_node = CanonicalContents::node(
        &mut [],
        &mut old_properties,
        Some("old text"),
        Some(old_embedding),
    )
    .expect("old node");
    let peer_node = CanonicalContents::node(&mut [], &mut [], None, None).expect("peer node");
    let initial = crate::property_graph::with_local_refs(|refs| {
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&old_node)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("peer key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&peer_node)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "old-edge")
                    .expect("relationship key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).expect("source")),
                    target: NodeRef::Local(refs.node(1).expect("target")),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("initial generation")
    });
    let node = match initial[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node identity domain"),
    };
    let peer = match initial[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("peer identity domain"),
    };
    let old_relationship = match initial[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship identity domain"),
    };
    let retained = store.admit_native_read().expect("retained old admission");
    vfs.take();

    let new_coordinates = [f32::from_bits(0x4000_0001), f32::from_bits(0x4040_0001)];
    let mut new_properties = [GraphProperty::new(
        GraphName::new("rank").expect("property name"),
        PropertyValue::new(PropertyData::I64(2)).expect("property value"),
    )];
    let new_embedding = CanonicalEmbedding::new(&document, &new_coordinates).expect("embedding");
    let new_node = CanonicalContents::node(
        &mut [],
        &mut new_properties,
        Some("new text"),
        Some(new_embedding),
    )
    .expect("new node");
    let changed = [
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Put(EntityId::Node(node)),
            image: Some(WriteImage::Node(&new_node)),
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "old-edge")
                .expect("old relationship key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(
                EntityId::Relationship(old_relationship),
                GraphDeleteMode::Restrict,
            ),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "new-edge")
                .expect("new relationship key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(node),
                target: NodeRef::Existing(peer),
                relationship_type: GraphName::new("LINKS").expect("relationship type"),
                properties: &[],
            }),
        },
    ];
    let (entered, release) = vfs.arm_wal_full_sync();
    let new_relationship = std::thread::scope(|scope| {
        let writer_store = Arc::clone(&store);
        let commit = scope.spawn(move || {
            writer_store.apply_native_graph(&changed, &QueryControl::Cancel(CancelToken::new()))
        });
        entered.wait();
        let held_generation = store
            .admit_native_read()
            .expect("admission while WAL Full sync is held")
            .bundle()
            .base()
            .generation
            .get();
        release.wait();
        assert_eq!(
            held_generation, 2,
            "publication must remain old until WAL Full sync returns"
        );
        let receipts = commit
            .join()
            .expect("commit thread")
            .expect("changed generation");
        match receipts[2].entity {
            EntityId::Relationship(relationship) => relationship,
            EntityId::Node(_) => panic!("new relationship identity domain"),
        }
    });

    let old = snapshot_for_lease(
        &store,
        &retained,
        node,
        peer,
        old_relationship,
        Some(new_relationship),
    );
    let current = store.admit_native_read().expect("new admission");
    let new = snapshot_for_lease(
        &store,
        &current,
        node,
        peer,
        old_relationship,
        Some(new_relationship),
    );
    assert_eq!(
        old,
        GenerationSnapshot {
            generation: 2,
            revision: 1,
            original_generation: 2,
            canonical: {
                let mut bytes = Vec::new();
                old_node
                    .write_to(&mut bytes, &mut || Ok(()))
                    .expect("old canonical");
                bytes
            },
            text: b"old text".to_vec(),
            vector: old_coordinates.map(f32::to_bits).to_vec(),
            old_relationship: true,
            new_relationship: false,
            out: vec![old_relationship],
            incoming: vec![old_relationship],
            sparse_text: true,
            sparse_vector: true,
        }
    );
    assert_eq!(
        new,
        GenerationSnapshot {
            generation: 3,
            revision: 2,
            original_generation: 3,
            canonical: {
                let mut bytes = Vec::new();
                new_node
                    .write_to(&mut bytes, &mut || Ok(()))
                    .expect("new canonical");
                bytes
            },
            text: b"new text".to_vec(),
            vector: new_coordinates.map(f32::to_bits).to_vec(),
            old_relationship: false,
            new_relationship: true,
            out: vec![new_relationship],
            incoming: vec![new_relationship],
            sparse_text: true,
            sparse_vector: true,
        }
    );
    drop(current);
    drop(retained);
    store.close().expect("close native store");
}

fn run_ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    vfs.take();

    let first = CanonicalContents::node(&mut [], &mut [], Some("alpha"), None).expect("first node");
    let create_first = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("first key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first)),
    }];
    let initial = store
        .apply_native_graph(&create_first, &QueryControl::Cancel(CancelToken::new()))
        .expect("initial commit");
    assert_eq!(initial.len(), 1);
    vfs.take();
    let sequence = store
        .admit_native_read()
        .expect("admit generation one")
        .bundle()
        .sequence();

    assert!(
        store
            .apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new()))
            .expect("empty no-op")
            .is_empty()
    );
    assert!(vfs.take().is_empty());
    let replay = store
        .apply_native_graph(&create_first, &QueryControl::Cancel(CancelToken::new()))
        .expect("exact replay");
    assert_eq!(replay.len(), 1);
    assert!(replay[0].replayed);
    assert_eq!(replay[0].generation.get(), 2);
    assert!(vfs.take().is_empty());
    assert_eq!(
        store
            .admit_native_read()
            .expect("unchanged replay view")
            .bundle()
            .sequence(),
        sequence
    );

    let second = CanonicalContents::node(&mut [], &mut [], None, None).expect("second node");
    let mixed = [
        create_first[0],
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("second key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&second)),
        },
    ];
    let mixed_receipts = store
        .apply_native_graph(&mixed, &QueryControl::Cancel(CancelToken::new()))
        .expect("mixed replay and changed commit");
    assert_eq!(mixed_receipts.len(), 2);
    assert!(mixed_receipts[0].replayed);
    assert_eq!(mixed_receipts[0].generation.get(), 2);
    assert!(!mixed_receipts[1].replayed);
    assert_eq!(mixed_receipts[1].generation.get(), 3);
    assert_eq!(
        vfs.take()
            .iter()
            .filter(|event| matches!(event, DurabilityEvent::Append(_)))
            .count(),
        1
    );

    let duplicate = [mixed[1], mixed[1]];
    assert!(
        store
            .apply_native_graph(&duplicate, &QueryControl::Cancel(CancelToken::new()))
            .is_err()
    );
    assert!(vfs.take().is_empty());
    store.close().expect("close native store");
}

fn run_ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    // This fixture pins the 64-envelope checkpoint boundary independently of
    // foreground reclaim publications (covered by ZE-316's count fixture).
    store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .expect("isolate checkpoint cadence");
    vfs.take();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let create = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "checkpoint").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let first = store
        .apply_native_graph(&create, &QueryControl::Cancel(CancelToken::new()))
        .expect("first complete envelope");
    let node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node identity domain"),
    };
    for revision in 2..=64 {
        let request = [StructuredWrite {
            key: create[0].key,
            revision: GraphRevision::new(revision).expect("revision"),
            operation: StructuredOperation::Put(EntityId::Node(node)),
            image: Some(WriteImage::Node(&image)),
        }];
        let receipt = store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .expect("complete envelope");
        assert_eq!(receipt[0].generation.get(), revision + 1);
    }
    let before = store.admit_native_read().expect("generation 64 admission");
    assert_eq!(
        before.bundle().prepared_inventories().len(),
        64,
        "each complete allocation inventory remains protected until consolidation"
    );
    let checkpoint_before = before.bundle().base().fold;
    drop(before);
    vfs.take();

    assert!(
        store
            .apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new()))
            .expect("threshold no-op")
            .is_empty()
    );
    let replay = [StructuredWrite {
        key: create[0].key,
        revision: GraphRevision::new(64).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: Some(WriteImage::Node(&image)),
    }];
    assert!(
        store
            .apply_native_graph(&replay, &QueryControl::Cancel(CancelToken::new()))
            .expect("threshold replay")[0]
            .replayed
    );
    assert!(
        vfs.take().is_empty(),
        "no-change work cannot trigger a checkpoint"
    );

    let changed = [StructuredWrite {
        key: create[0].key,
        revision: GraphRevision::new(65).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: Some(WriteImage::Node(&image)),
    }];
    let receipt = store
        .apply_native_graph(&changed, &QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint then changed commit");
    assert_eq!(receipt[0].generation.get(), 67);
    let current = store.admit_native_read().expect("generation 65 admission");
    assert_ne!(current.bundle().base().fold, checkpoint_before);
    assert_eq!(current.bundle().base().fold.envelope_sequence, 64);
    assert_eq!(current.bundle().base().generation.get(), 67);
    let manifest =
        crate::manifest::io::load_manifest(vfs.as_ref(), &path.join("manifest.ze"), 65).unwrap();
    assert_eq!(
        manifest.graph.as_ref().unwrap().state().unwrap().sequence,
        64
    );
    drop(current);

    let events = vfs.take();
    let rotated_wal = events
        .iter()
        .position(|event| {
            matches!(event, DurabilityEvent::Rename(_, path) if path.file_name().is_some_and(|name| name == "wal.ze"))
        })
        .expect("rotated WAL replacement");
    let selected_root = events
        .iter()
        .position(|event| {
            matches!(event, DurabilityEvent::Rename(_, target) if target.file_name().is_some_and(|name| name == "manifest.ze"))
        })
        .expect("checkpoint selector rename");
    let changed_append = events
        .iter()
        .rposition(|event| matches!(event, DurabilityEvent::Append(_)))
        .expect("post-checkpoint append");
    assert!(selected_root < rotated_wal && rotated_wal < changed_append);

    let blocked = [StructuredWrite {
        key: create[0].key,
        revision: GraphRevision::new(66).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: Some(WriteImage::Node(&image)),
    }];
    for point in [
        FaultPoint::ManifestSync,
        FaultPoint::DirectorySync,
        FaultPoint::OpenAppend,
        FaultPoint::Rename,
        FaultPoint::SelectorSync,
    ] {
        vfs.arm_fault(point);
        assert!(matches!(
            store.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new())),
            Err(super::super::NativeGraphError::Io { .. }
                | super::super::NativeGraphError::Store(_))
        ));
        vfs.assert_fired_once();
        assert_eq!(
            store
                .admit_native_read()
                .expect("acknowledged generation remains readable")
                .bundle()
                .base()
                .generation
                .get(),
            67
        );
        assert!(matches!(
            store.apply_native_graph(&blocked, &QueryControl::Cancel(CancelToken::new())),
            Err(super::super::NativeGraphError::CheckpointRequired)
        ));
        assert!(
            store
                .apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new()))
                .expect("no-op remains available after checkpoint failure")
                .is_empty()
        );
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("clean explicit checkpoint retry");
    }
    let store_generation = store.snapshot().unwrap().generation();
    assert_eq!(
        store
            .apply_native_graph(&blocked, &QueryControl::Cancel(CancelToken::new()))
            .expect("changed admission resumes")[0]
            .generation
            .get(),
        store_generation + 1
    );
    store.close().expect("close native store");
    checkpoint_pending_byte_boundary();
}

fn checkpoint_pending_byte_boundary() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("bytes");
    let store = Store::create_native_graph(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .unwrap();
    let envelope_bytes_on_disk = || {
        let bytes = std::fs::read(store.directory.join("wal.ze")).unwrap();
        let replay = crate::wal::replay::replay(&bytes);
        replay
            .records
            .iter()
            .map(|record| {
                assert_eq!(record.op, crate::ingest::wal_payload::GRAPH_COMMIT_V1);
                record.payload.len()
            })
            .sum::<usize>()
    };
    let key = "k".repeat(512 * 1024);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut rotated = false;
    for revision in 1..=40 {
        let (old_identity, old_bytes, old_count) = {
            let guard = store.native_graph.writer.lock().unwrap();
            let writer = guard.as_ref().unwrap();
            assert_eq!(envelope_bytes_on_disk(), writer.envelope_bytes);
            (
                writer.last_graph_seq,
                writer.envelope_bytes,
                writer.complete_envelopes,
            )
        };
        let request = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", &key).unwrap(),
            revision: GraphRevision::new(revision).unwrap(),
            operation: if revision == 1 {
                StructuredOperation::Create
            } else {
                StructuredOperation::Put(EntityId::Node(NodeId::new(1).unwrap()))
            },
            image: Some(WriteImage::Node(&image)),
        }];
        let before_generation = store.snapshot().unwrap().generation();
        let generation = store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .unwrap()[0]
            .generation
            .get();
        let guard = store.native_graph.writer.lock().unwrap();
        let writer = guard.as_ref().unwrap();
        assert_eq!(envelope_bytes_on_disk(), writer.envelope_bytes);
        let tail = writer.envelope_bytes;
        assert!(tail <= crate::property_graph::wal::MAX_ENVELOPE_BYTES);
        assert!(writer.last_graph_seq > old_identity);
        let folded = old_count != 0 && writer.complete_envelopes == 1;
        assert_eq!(generation, before_generation + 1 + u64::from(folded));
        if folded {
            assert!(
                old_count < 64,
                "byte trigger precedes envelope-count trigger"
            );
            assert_eq!(writer.complete_envelopes, 1);
            assert!(
                old_bytes + tail > crate::property_graph::wal::MAX_ENVELOPE_BYTES,
                "actual encoded pending envelope crosses the exact 16 MiB bound"
            );
            rotated = true;
            break;
        }
    }
    assert!(
        rotated,
        "pending bytes must rotate before an oversized tail is appended"
    );
    store.close().unwrap();
}

fn run_ze39_protection_capture_and_maintenance_recheck_are_atomic() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Arc::new(
        Store::create_native_graph_with_infrastructure(
            &path,
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .expect("fresh native store"),
    );
    let first_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("first node");
    let first = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "first").expect("first key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    store
        .apply_native_graph(&first, &QueryControl::Cancel(CancelToken::new()))
        .expect("first commit");
    let admitted = store.admit_native_read().expect("first admission");
    let old_root = admitted.bundle().catalog();
    let old_fold = admitted.bundle().base().fold;
    drop(admitted);
    let before = store
        .capture_native_read_roots()
        .expect("initial protected capture");
    assert!(before.contains(old_root));
    assert!(before.wal().is_some_and(|wal| wal.bytes > 0));
    assert!(!before.prepared.is_empty());
    let before_fence = before.serial_fence();

    let stale = store
        .admit_native_graph_maintenance()
        .expect("retained maintenance base");
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    store
        .native_graph
        .state
        .lock()
        .expect("publication state")
        .admission_hook = Some((Arc::clone(&entered), Arc::clone(&release)));
    let second_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("second node");
    let second = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "second").expect("second key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&second_image)),
    }];
    std::thread::scope(|scope| {
        let read_store = Arc::clone(&store);
        let retained = scope.spawn(move || read_store.admit_native_read().expect("racing read"));
        entered.wait();
        let write_store = Arc::clone(&store);
        let commit = scope.spawn(move || {
            write_store
                .apply_native_graph(&second, &QueryControl::Cancel(CancelToken::new()))
                .expect("racing commit")
        });
        // The commit owns the writer while its admission waits behind the
        // paused read. A capture that won the writer first would observe
        // only the old bundle.
        while store.native_graph.writer.try_lock().is_ok() {
            std::thread::yield_now();
        }
        let capture_store = Arc::clone(&store);
        let captured = scope.spawn(move || {
            capture_store
                .capture_native_read_roots()
                .expect("racing capture")
        });
        release.wait();
        let old_lease = retained.join().expect("read thread");
        let receipt = commit.join().expect("write thread");
        assert_eq!(receipt[0].generation.get(), 3);
        let protected = captured.join().expect("capture thread");
        assert!(protected.contains(old_root));
        assert!(protected.bundle_count() >= 2);
        assert!(protected.serial_fence() > before_fence);
        assert!(protected.wal().is_some());
        assert!(!protected.prepared.is_empty());
        drop(old_lease);
    });

    assert!(matches!(
        store.commit_native_graph_maintenance(&stale, &QueryControl::Cancel(CancelToken::new()),),
        Err(super::super::NativeGraphError::StalePreparation)
    ));
    let current = store.admit_native_read().expect("current admission");
    assert_eq!(current.bundle().base().fold, old_fold);
    let checkpoint_catalog = current.bundle().catalog();
    let raw_lagged = super::super::NativeGraphBundleInput {
        base: crate::property_graph::staging::BaseIdentity {
            store: current.bundle().base().store,
            generation: crate::property_graph::GraphGeneration::new(
                current.bundle().base().generation.get() + 1,
            ),
            fold: current.bundle().base().fold,
            roots: current.bundle().base().roots,
        },
        root_envelope: current.bundle().root_envelope(),
        roots: current.bundle().roots(),
        wal_roots: current.bundle().wal_roots(),
        sequence: current.bundle().sequence() + 1,
        catalog: current.bundle().catalog(),
        vector: current.bundle().vector(),
        text: current.bundle().text(),
        reclaim: current.bundle().reclaim(),
        high_waters: current.bundle().high_waters(),
        prepared_inventories: current.bundle().prepared_inventories().to_vec(),
        lexical: current.bundle().lexical(),
        document: current.bundle().document().cloned(),
    };
    drop(current);
    assert!(matches!(
        store.install_native_graph_for_test(raw_lagged),
        Err(super::super::NativeGraphError::Invalid(_))
    ));

    let maintenance = store
        .admit_native_graph_maintenance()
        .expect("fresh maintenance admission");
    // ZE-46: maintenance publishes a Maintenance transition rather than a
    // checkpoint. The cutoff guarantee this control pinned holds at the
    // checkpoint that follows it.
    let report = store
        .commit_native_graph_maintenance(&maintenance, &QueryControl::Cancel(CancelToken::new()))
        .expect("bounded replacement maintenance");
    assert_eq!(report.generation.get(), 5);
    assert!(report.replaced_physical_refs > 0);
    drop(maintenance);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint after maintenance");
    let after = store
        .capture_native_read_roots()
        .expect("post-checkpoint capture");
    assert!(after.serial_fence() > before_fence);
    assert!(after.wal().is_some_and(|wal| wal.bytes == 0));
    assert!(after.prepared.is_empty());
    let selected = store
        .admit_native_read()
        .expect("selected maintenance root");
    assert_ne!(selected.bundle().base().fold, old_fold);
    assert_eq!(selected.bundle().base().generation.get(), 5);
    assert_eq!(selected.bundle().catalog(), checkpoint_catalog);
    drop(selected);
    drop(stale);
    drop(before);
    drop(after);
    // A correctly tagged target cannot substitute the old OUT root. The real
    // constructor must reject before any immutable write or WAL append.
    let relationship = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Relationship, "app", "proof").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Relationship {
            source: NodeRef::Existing(NodeId::new(1).unwrap()),
            target: NodeRef::Existing(NodeId::new(2).unwrap()),
            relationship_type: GraphName::new("PROOF").unwrap(),
            properties: &[],
        }),
    }];
    vfs.take();
    store
        .native_graph
        .substitute_old_out
        .store(true, Ordering::Release);
    assert!(matches!(
        store.apply_native_graph(&relationship, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::Invalid(
            "prepared committed transition facts"
        ))
    ));
    assert!(
        !store
            .native_graph
            .substitute_old_out
            .load(Ordering::Acquire)
    );
    assert!(vfs.take().is_empty());
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        5
    );
    assert_eq!(
        store
            .apply_native_graph(&relationship, &QueryControl::Cancel(CancelToken::new()))
            .unwrap()[0]
            .generation
            .get(),
        7
    );
    let store = Arc::into_inner(store).expect("sole store owner");
    store.close().expect("close native store");
}

struct ObserveTextMembership {
    node: NodeId,
    expected_text: &'static [u8],
    expected_member: bool,
}

impl super::super::NativeReadConsumer<()> for ObserveTextMembership {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        let text = view
            .stored_text(self.node, &mut resources)?
            .ok_or(TreeError::Invalid("missing text"))?;
        let mut bytes = vec![0_u8; text.len() as usize];
        let expected = bytes.len();
        assert_eq!(text.read_at(0, &mut bytes, &mut resources)?, expected);
        assert_eq!(bytes, self.expected_text);
        drop(resources);
        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        assert_eq!(
            sparse
                .lookup(Modality::Text, self.node, &mut resources)?
                .is_some(),
            self.expected_member
        );
        Ok(())
    }
}

fn run_ze39_precommit_failure_keeps_graph_and_path_ownership_private() {
    for point in [
        FaultPoint::Create,
        FaultPoint::PartialCreate,
        FaultPoint::ObjectSync,
        FaultPoint::DirectorySync,
    ] {
        let parent = tempfile::tempdir().expect("temporary parent");
        let path = parent.path().join("native");
        let vfs = Arc::new(RecordingVfs::default());
        let infrastructure: Arc<dyn Vfs> = vfs.clone();
        let store = Store::create_native_graph_with_infrastructure(
            &path,
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .expect("fresh native store");
        let original =
            CanonicalContents::node(&mut [], &mut [], Some("indexed"), None).expect("node");
        let create = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "faulted").expect("key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&original)),
        }];
        let receipt = store
            .apply_native_graph(&create, &QueryControl::Cancel(CancelToken::new()))
            .expect("initial commit");
        let node = match receipt[0].entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => panic!("node identity domain"),
        };
        vfs.take();
        let analyzed_empty =
            CanonicalContents::node(&mut [], &mut [], Some("!!!"), None).expect("replacement");
        let changed = [StructuredWrite {
            key: create[0].key,
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Put(EntityId::Node(node)),
            image: Some(WriteImage::Node(&analyzed_empty)),
        }];
        if point == FaultPoint::Create {
            let shared =
                crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
            let baseline = shared.reserved_bytes().unwrap();
            let pressure = shared
                .reserve((256 * 1024 * 1024 - baseline - 2 * 1024 * 1024) as usize)
                .unwrap();
            assert!(matches!(
                store.apply_native_graph(&changed, &QueryControl::Cancel(CancelToken::new())),
                Err(super::super::NativeGraphError::Read(TreeError::Memory))
            ));
            assert!(
                vfs.take().is_empty(),
                "pack reservation fails before any file creation"
            );
            drop(pressure);
            assert_eq!(
                shared.reserved_bytes().unwrap(),
                baseline,
                "failed preparation releases its owners"
            );
        }
        vfs.arm_fault(point);
        assert!(matches!(
            store.apply_native_graph(&changed, &QueryControl::Cancel(CancelToken::new())),
            Err(super::super::NativeGraphError::Io { .. })
        ));
        vfs.assert_fired_once();
        assert_eq!(
            store
                .admit_native_read()
                .expect("old generation remains admissible")
                .bundle()
                .base()
                .generation
                .get(),
            2
        );
        if point == FaultPoint::Create {
            let foreign = vfs
                .list(&path)
                .expect("artifact listing")
                .into_iter()
                .find(|candidate| vfs.read(candidate).ok().as_deref() == Some(b"foreign-owner"))
                .expect("colliding foreign file");
            assert_eq!(vfs.read(&foreign).expect("foreign bytes"), b"foreign-owner");
        }
        let retry = store
            .apply_native_graph(&changed, &QueryControl::Cancel(CancelToken::new()))
            .expect("clean same-input retry");
        assert_eq!(retry[0].generation.get(), 3);
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                16,
                ObserveTextMembership {
                    node,
                    expected_text: b"!!!",
                    expected_member: false,
                },
            )
            .expect("replacement removal is visible only after retry");
        store.close().expect("close native store");
    }
}

fn run_ze39_commit_attempt_errors_are_indeterminate_and_stop_admission() {
    for point in [
        FaultPoint::Append,
        FaultPoint::PartialAppend,
        FaultPoint::WalSync,
        FaultPoint::Publish,
    ] {
        let parent = tempfile::tempdir().expect("temporary parent");
        let path = parent.path().join("native");
        let vfs = Arc::new(RecordingVfs::default());
        let infrastructure: Arc<dyn Vfs> = vfs.clone();
        let store = Store::create_native_graph_with_infrastructure(
            &path,
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .expect("fresh native store");
        let old = store.admit_native_read().expect("old retained lease");
        let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node");
        let request = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "uncertain").expect("key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }];
        if point == FaultPoint::Publish {
            store
                .native_graph
                .fail_next_publication
                .store(true, Ordering::Release);
        } else {
            vfs.arm_fault(point);
        }
        assert!(matches!(
            store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
            Err(super::super::NativeGraphError::CommitIndeterminate { .. })
        ));
        if point == FaultPoint::Publish {
            assert!(
                !store
                    .native_graph
                    .fail_next_publication
                    .load(Ordering::Acquire)
            );
            VERIFIED_FAULTS.with(|count| count.set(count.get() + 1));
            assert!(vfs.take().iter().any(|event| matches!(event, DurabilityEvent::Sync(path, SyncKind::Full) if path.file_name().unwrap() == "wal.ze")));
        } else {
            vfs.assert_fired_once();
        }
        assert_eq!(old.bundle().base().generation.get(), 1);
        assert!(matches!(
            store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
            Err(super::super::NativeGraphError::WritesStopped)
        ));
        assert!(matches!(
            store.admit_native_read(),
            Err(super::super::NativeGraphError::ReadAdmissionsStopped)
        ));
        drop(old);
        store.close().expect("close stopped native store");
        let clean = Store::create_native_graph_with_infrastructure(
            parent.path().join("clean"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .unwrap();
        assert_eq!(
            clean
                .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
                .unwrap()[0]
                .generation
                .get(),
            2
        );
        clean.close().unwrap();
    }
}

struct ProbeRegistration {
    bytes: usize,
}

impl ResultRegistration for ProbeRegistration {
    fn capacity_bytes(&self) -> usize {
        self.bytes
    }
}

struct ProbeMaterializer {
    calls: Arc<AtomicU64>,
    fail_materialize: bool,
    excessive_layout: bool,
}

impl ResultMaterializer for ProbeMaterializer {
    type Registration = ProbeRegistration;

    fn layout(
        &mut self,
        receipt_count: usize,
        control: &mut crate::property_graph::staging::WriteControl<'_>,
    ) -> Result<ResultLayout, StageError> {
        control(WritePhase::CoreResult)?;
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ResultLayout {
            rows: receipt_count,
            core_bytes: receipt_count,
            abi_bytes: receipt_count,
            registry_bytes: if self.excessive_layout {
                usize::MAX
            } else {
                receipt_count
            },
        })
    }

    fn materialize(
        &mut self,
        receipts: &[crate::property_graph::staging::ItemReceipt],
        core: &mut [u8],
        abi: &mut [u8],
        control: &mut crate::property_graph::staging::WriteControl<'_>,
    ) -> Result<Self::Registration, StageError> {
        control(WritePhase::AbiResult)?;
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_materialize {
            return Err(StageError::Cancelled);
        }
        assert_eq!(core.len(), receipts.len());
        assert_eq!(abi.len(), receipts.len());
        core.fill(0x39);
        abi.fill(0x68);
        Ok(ProbeRegistration {
            bytes: receipts.len(),
        })
    }
}

fn run_ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Arc::new(
        Store::create_native_graph_with_infrastructure(
            &path,
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .expect("fresh native store"),
    );
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "result").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    vfs.take();
    let failed_calls = Arc::new(AtomicU64::new(0));
    let mut failing = ProbeMaterializer {
        calls: Arc::clone(&failed_calls),
        fail_materialize: true,
        excessive_layout: false,
    };
    assert!(matches!(
        store.apply_native_graph_with_materializer(
            &request,
            &QueryControl::Cancel(CancelToken::new()),
            &mut failing,
        ),
        Err(super::super::NativeGraphError::Stage(StageError::Cancelled))
    ));
    assert_eq!(failed_calls.load(Ordering::Relaxed), 2);
    assert!(vfs.take().is_empty());
    assert_eq!(
        store
            .admit_native_read()
            .expect("precommit failure leaves reads healthy")
            .bundle()
            .base()
            .generation
            .get(),
        1
    );

    let limit_calls = Arc::new(AtomicU64::new(0));
    let mut limited = ProbeMaterializer {
        calls: Arc::clone(&limit_calls),
        fail_materialize: false,
        excessive_layout: true,
    };
    assert!(matches!(
        store.apply_native_graph_with_materializer(
            &request,
            &QueryControl::Cancel(CancelToken::new()),
            &mut limited
        ),
        Err(super::super::NativeGraphError::Stage(StageError::Limit))
    ));
    assert_eq!(limit_calls.load(Ordering::Relaxed), 1);
    assert!(vfs.take().is_empty());
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(
        store
            .apply_native_graph(&request, &QueryControl::Cancel(cancelled))
            .is_err()
    );
    assert!(vfs.take().is_empty());
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let successful_calls = Arc::new(AtomicU64::new(0));
    let (entered, release) = vfs.arm_wal_full_sync();
    let (close_started_tx, close_started_rx) = std::sync::mpsc::channel();
    let (close_done_tx, close_done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let commit_store = Arc::clone(&store);
        let calls = Arc::clone(&successful_calls);
        let commit = scope.spawn(move || {
            let mut materializer = ProbeMaterializer {
                calls,
                fail_materialize: false,
                excessive_layout: false,
            };
            commit_store.apply_native_graph_with_materializer(&request, &control, &mut materializer)
        });
        entered.wait();
        token.cancel();
        let close_store = Arc::clone(&store);
        let close = scope.spawn(move || {
            close_started_tx.send(()).expect("close start signal");
            let result = close_store.close();
            close_done_tx.send(()).expect("close completion signal");
            result
        });
        close_started_rx.recv().expect("close started");
        assert!(matches!(
            close_done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release.wait();
        let registration = commit
            .join()
            .expect("commit thread")
            .expect("post-attempt cancellation completes commit");
        assert_eq!(registration.capacity_bytes(), 1);
        assert_eq!(registration.core_bytes(), &[0x39]);
        assert_eq!(registration.abi_bytes(), &[0x68]);
        #[cfg(feature = "allocation-audit")]
        {
            assert_eq!(
                store
                    .native_graph
                    .commit_allocations
                    .load(Ordering::Acquire),
                0
            );
            assert_eq!(
                store
                    .native_graph
                    .commit_allocation_denials
                    .load(Ordering::Acquire),
                0
            );
        }
        close
            .join()
            .expect("close thread")
            .expect("close after commit");
    });
    assert_eq!(successful_calls.load(Ordering::Relaxed), 2);
    assert!(close_done_rx.try_recv().is_ok());
}

struct FailingEntropy;

impl crate::property_graph::storage::allocation::EntropyProvider for FailingEntropy {
    fn fill_nonce(&mut self, _: &mut [u8; 16]) -> std::io::Result<()> {
        Err(std::io::Error::other("scheduled entropy failure"))
    }
}

fn run_ze39_fresh_create_failures_never_adopt_or_replace_identity() {
    let options = || {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024)
    };
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("valid");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("valid fresh store");
    let selected_before = vfs.read(&path.join("manifest.ze")).expect("selected root");
    let repeated = Store::create_native_graph_with_infrastructure(
        &path,
        options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    );
    assert!(matches!(
        repeated,
        Err(super::super::NativeGraphError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(
        vfs.read(&path.join("manifest.ze"))
            .expect("unchanged selected root"),
        selected_before
    );
    let manifest =
        crate::manifest::io::load_manifest(vfs.as_ref(), &path.join("manifest.ze"), 0).unwrap();
    assert!(manifest.graph.is_some());
    store.close().expect("close valid store");

    let incomplete = parent.path().join("incomplete");
    std::fs::create_dir(&incomplete).expect("incomplete directory");
    assert_eq!(
        super::super::persistence::classify_native_graph(vfs.as_ref(), &incomplete),
        super::super::persistence::NativeStoreClassification::Incomplete
    );
    assert!(matches!(
        Store::create_native_graph_with_infrastructure(
            &incomplete,
            options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        ),
        Err(super::super::NativeGraphError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists
    ));

    let incompatible = parent.path().join("incompatible");
    std::fs::create_dir(&incompatible).expect("incompatible directory");
    let mut incompatible_bytes = [0_u8; 120];
    incompatible_bytes[..8].copy_from_slice(b"ZGROOT01");
    incompatible_bytes[8..10].copy_from_slice(&2_u16.to_le_bytes());
    std::fs::write(incompatible.join("graph-root.ze"), incompatible_bytes)
        .expect("incompatible selector");
    assert_eq!(
        super::super::persistence::classify_native_graph(vfs.as_ref(), &incompatible),
        super::super::persistence::NativeStoreClassification::Incompatible
    );
    assert!(matches!(
        Store::create_native_graph_with_infrastructure(
            &incompatible,
            options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        ),
        Err(super::super::NativeGraphError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists
    ));

    let corrupt = parent.path().join("corrupt");
    std::fs::create_dir(&corrupt).expect("corrupt directory");
    let mut corrupt_bytes = [0_u8; 120];
    corrupt_bytes[..8].copy_from_slice(b"ZGROOT01");
    corrupt_bytes[8..10].copy_from_slice(&1_u16.to_le_bytes());
    corrupt_bytes[10..12].copy_from_slice(
        &crate::format::FormatFamily::NativeGraphRoot
            .id()
            .to_le_bytes(),
    );
    corrupt_bytes[12..16].copy_from_slice(&120_u32.to_le_bytes());
    std::fs::write(corrupt.join("graph-root.ze"), corrupt_bytes).expect("corrupt selector");
    assert_eq!(
        super::super::persistence::classify_native_graph(vfs.as_ref(), &corrupt),
        super::super::persistence::NativeStoreClassification::Corrupt
    );
    assert!(matches!(
        Store::create_native_graph_with_infrastructure(
            &corrupt,
            options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        ),
        Err(super::super::NativeGraphError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists
    ));

    let entropy_path = parent.path().join("entropy-failure");
    assert!(matches!(
        Store::create_native_graph_with_infrastructure(
            &entropy_path,
            options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut FailingEntropy,
        ),
        Err(super::super::NativeGraphError::Io { .. })
    ));
    assert_eq!(
        super::super::persistence::classify_native_graph(vfs.as_ref(), &entropy_path),
        super::super::persistence::NativeStoreClassification::Incomplete
    );

    for point in [
        FaultPoint::Create,
        FaultPoint::ObjectSync,
        FaultPoint::DirectorySync,
    ] {
        let failed_path = parent.path().join(format!("constructor-{point:?}"));
        vfs.arm_fault(point);
        assert!(
            Store::create_native_graph_with_infrastructure(
                &failed_path,
                options(),
                None,
                Arc::clone(&infrastructure),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
                &mut crate::property_graph::storage::allocation::OsEntropy,
            )
            .is_err()
        );
        vfs.assert_fired_once();
        assert_eq!(
            super::super::persistence::classify_native_graph(vfs.as_ref(), &failed_path),
            super::super::persistence::NativeStoreClassification::Incomplete
        );
    }
}

#[cfg_attr(test, test)]
fn ze39_fresh_mixed_commit_is_durable_and_coherent() {
    let _ = run_ze39_fresh_mixed_commit_is_durable_and_coherent();
}
#[cfg_attr(test, test)]
fn ze39_retained_reader_and_new_admission_observe_whole_generations() {
    run_ze39_retained_reader_and_new_admission_observe_whole_generations();
}
#[cfg_attr(test, test)]
fn ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work() {
    run_ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work();
}
#[cfg_attr(test, test)]
fn ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state() {
    run_ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state();
}
#[cfg_attr(test, test)]
fn ze39_protection_capture_and_maintenance_recheck_are_atomic() {
    run_ze39_protection_capture_and_maintenance_recheck_are_atomic();
}
#[cfg_attr(test, test)]
fn ze39_precommit_failure_keeps_graph_and_path_ownership_private() {
    run_ze39_precommit_failure_keeps_graph_and_path_ownership_private();
}
#[cfg_attr(test, test)]
fn ze39_commit_attempt_errors_are_indeterminate_and_stop_admission() {
    run_ze39_commit_attempt_errors_are_indeterminate_and_stop_admission();
}
#[cfg_attr(test, test)]
fn ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary() {
    run_ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary();
}
#[cfg_attr(test, test)]
fn ze39_fresh_create_failures_never_adopt_or_replace_identity() {
    run_ze39_fresh_create_failures_never_adopt_or_replace_identity();
}

#[cfg(feature = "test-seams")]
pub(crate) fn run_actual_probe(
    _seed: u64,
) -> crate::graph_read_view_test_support::ActualProbeReport {
    use crate::graph_read_view_test_support::{
        ActualProbeReport, ObservedRelationship, PathReceipt,
    };
    let rows = run_ze39_fresh_mixed_commit_is_durable_and_coherent();
    let mut receipts = vec![PathReceipt {
        key: "property-graph.publication.coherent",
        fires: 0,
        clean_controls: 1,
    }];
    let paths: [(&str, fn()); 8] = [
        (
            "retained",
            run_ze39_retained_reader_and_new_admission_observe_whole_generations,
        ),
        (
            "noop",
            run_ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work,
        ),
        (
            "precommit",
            run_ze39_precommit_failure_keeps_graph_and_path_ownership_private,
        ),
        (
            "uncertain",
            run_ze39_commit_attempt_errors_are_indeterminate_and_stop_admission,
        ),
        (
            "result",
            run_ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary,
        ),
        (
            "checkpoint",
            run_ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state,
        ),
        (
            "capture",
            run_ze39_protection_capture_and_maintenance_recheck_are_atomic,
        ),
        (
            "creation",
            run_ze39_fresh_create_failures_never_adopt_or_replace_identity,
        ),
    ];
    for (name, probe) in paths {
        reset_verified_faults();
        probe();
        let fires = take_verified_faults();
        let key = match name {
            "retained" => "property-graph.publication.retained",
            "noop" => "property-graph.publication.noop",
            "precommit" => "property-graph.publication.precommit",
            "uncertain" => "property-graph.publication.uncertain",
            "result" => "property-graph.publication.result",
            "checkpoint" => "property-graph.publication.checkpoint",
            "capture" => "property-graph.publication.capture",
            "creation" => "property-graph.publication.creation",
            _ => unreachable!(),
        };
        receipts.push(PathReceipt {
            key,
            fires,
            clean_controls: 1,
        });
    }
    ActualProbeReport {
        receipts,
        relationships: rows
            .into_iter()
            .map(|row| ObservedRelationship {
                rel: row.rel.get(),
                source: row.source.get(),
                target: row.target.get(),
                relationship_type: row.relationship_type.get(),
            })
            .collect(),
    }
}

/// Query-path probe access to the existing publication fault.
pub(crate) fn arm_query_publication_fault(store: &Store) {
    store
        .native_graph
        .fail_next_publication
        .store(true, Ordering::Release);
}

pub(crate) fn query_publication_fault_fired(store: &Store) -> bool {
    !store
        .native_graph
        .fail_next_publication
        .load(Ordering::Acquire)
}

#[test]
fn ze76_commit_io_matches_independent_vfs_witness() {
    use crate::property_graph::resources::GraphResources;
    use crate::vfs::CountingVfs;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native");
    let vfs = Arc::new(CountingVfs::new(RecordingVfs::default()));
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(16 * 1024 * 1024),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    // Initialize the lazy outer WAL header before measuring one op-10 frame.
    let warmup = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze76", "warmup").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&warmup)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let before = resources.work_ledger().unwrap();
    vfs.reset();
    vfs.inner().take();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze76", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let after = resources.work_ledger().unwrap();
    let events = vfs.inner().take();
    let append = events
        .iter()
        .position(|e| matches!(e, DurabilityEvent::Append(_)))
        .unwrap();
    let directory_sync = events[..append]
        .iter()
        .rposition(|e| matches!(e,DurabilityEvent::Sync(p,SyncKind::Full) if p == &path))
        .unwrap();
    let start = events[..directory_sync]
        .iter()
        .rposition(|e| matches!(e,DurabilityEvent::Sync(p,SyncKind::Full) if p == &path))
        .map_or(0, |i| i + 1);
    let creates: Vec<_> = events[start..directory_sync]
        .iter()
        .filter_map(|e| {
            if let DurabilityEvent::Create(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    let written: u64 = creates
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();
    assert!(!creates.is_empty());
    assert_eq!(
        after.artifact_writes - before.artifact_writes,
        creates.len() as u64
    );
    assert_eq!(
        after.artifact_bytes_written - before.artifact_bytes_written,
        written
    );
    assert_eq!(after.wal_appends - before.wal_appends, 1);
    assert_eq!(after.wal_appends - before.wal_appends, vfs.append_calls());
    assert_eq!(
        after.wal_bytes_appended - before.wal_bytes_appended,
        vfs.bytes_appended()
    );
    assert_eq!(
        after.encoded_wal_bytes - before.encoded_wal_bytes,
        vfs.bytes_appended()
    );
    assert_eq!(
        after.full_sync_attempts - before.full_sync_attempts,
        creates.len() as u64 + 1
    );
    assert_eq!(
        after.full_sync_successes - before.full_sync_successes,
        creates.len() as u64 + 1
    );
    assert_eq!(
        after.directory_sync_attempts - before.directory_sync_attempts,
        1
    );
    assert_eq!(
        after.directory_sync_successes - before.directory_sync_successes,
        1
    );
    // Independent framing walk: two size passes and one encoding pass.
    // Each payload byte is processed seven times; each record contributes
    // 640 header/footer/hash units. The final envelope hash covers preceding
    // records, and each pass measures + writes its one inventory descriptor.
    let wal_path = events
        .iter()
        .find_map(|e| {
            if let DurabilityEvent::Append(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    let wal = std::fs::read(wal_path).unwrap();
    let outer = crate::wal::replay::replay(&wal);
    let record = outer.records.last().unwrap();
    assert_eq!(record.op, crate::ingest::wal_payload::GRAPH_COMMIT_V1);
    let envelope = record.payload;
    assert_eq!(
        vfs.bytes_appended(),
        (envelope.len() + crate::wal::record::MIN_RECORD_LEN) as u64
    );
    let mut offset = 0usize;
    let mut units = 0u64;
    let mut records = 0u64;
    let mut commit_start = 0usize;
    let mut prepared_count = 0u64;
    while offset < envelope.len() {
        assert_eq!(&envelope[offset..offset + 4], b"ZGWF");
        let payload =
            u32::from_le_bytes(envelope[offset + 8..offset + 12].try_into().unwrap()) as usize;
        if u16::from_le_bytes(envelope[offset + 4..offset + 6].try_into().unwrap()) == 6 {
            commit_start = offset;
            let state = &envelope[offset + 64 + 16..offset + 64 + payload];
            let mut cancelled = || false;
            let mut resources = crate::property_graph::wal::WalResources::new(
                u64::MAX,
                crate::property_graph::wal::STACK_RESERVATION_BYTES,
                &mut cancelled,
            )
            .unwrap();
            prepared_count = crate::property_graph::wal::decode_commit_state(state, &mut resources)
                .unwrap()
                .prepared_inventories
                .len()
                .unwrap() as u64;
        }
        if u16::from_le_bytes(envelope[offset + 4..offset + 6].try_into().unwrap()) == 2 {
            let provenance =
                u64::from_le_bytes(envelope[offset + 72..offset + 80].try_into().unwrap());
            units += 6 * provenance;
        }
        units += 7 * payload as u64 + 640;
        records += 1;
        offset += 72 + payload;
    }
    assert_eq!(offset, envelope.len());
    units += commit_start as u64 + 2 * (records - 2) + 6 * prepared_count;
    assert_eq!(after.wal_codec_units - before.wal_codec_units, units);
    store.close().unwrap();
}

#[test]
fn ze76_failed_sync_keeps_exact_attempt_and_release_prefix() {
    use crate::lifecycle::native_graph::NativeGraphError;
    use crate::property_graph::resources::GraphResources;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(16 * 1024 * 1024),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let before = resources.work_ledger().unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let write = StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "ze76", "a").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    };
    vfs.take();
    vfs.arm_fault(FaultPoint::ObjectSync);
    let error = store
        .apply_native_graph(&[write], &QueryControl::Cancel(CancelToken::new()))
        .err()
        .unwrap();
    assert!(matches!(error, NativeGraphError::Io { .. }));
    vfs.assert_fired_once();
    let after = resources.work_ledger().unwrap();
    assert_eq!(after.full_sync_attempts - before.full_sync_attempts, 1);
    assert_eq!(after.full_sync_successes - before.full_sync_successes, 0);
    assert_eq!(after.artifact_writes - before.artifact_writes, 1);
    assert_eq!(after.wal_appends - before.wal_appends, 0);
    assert_eq!(after.wal_bytes_appended - before.wal_bytes_appended, 0);
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        1
    );
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    let result = store
        .apply_native_graph(&[write], &QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    assert_eq!(result.changed_generation().unwrap().get(), 2);
    drop(result);
    store.close().unwrap();
}

#[test]
fn ze76_counter_failure_cannot_change_commit() {
    use crate::vfs::CountingVfs;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native");
    let vfs = Arc::new(CountingVfs::new(RecordingVfs::default()));
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(16 * 1024 * 1024),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    store.accounting.ze76_overflow_work();
    let merges = store.accounting.ze76_work_merges();
    vfs.reset();
    vfs.inner().take();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze76", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        2
    );
    assert_eq!(store.accounting.ze76_work_merges() - merges, 1);
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze76", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        3
    );
}

#[test]
fn ze76_poisoned_counter_cannot_change_commit() {
    use crate::vfs::CountingVfs;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native");
    let vfs = Arc::new(CountingVfs::new(RecordingVfs::default()));
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(16 * 1024 * 1024),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    store.accounting.ze76_poison_work();
    let merges = store.accounting.ze76_work_merges();
    vfs.reset();
    vfs.inner().take();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze76", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        2
    );
    assert_eq!(store.accounting.ze76_work_merges() - merges, 1);
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze76", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        3
    );
}

#[cfg(test)]
mod graph_write {
    use super::*;

    #[test]
    fn writable_reopen_does_not_reset_the_reclaim_count_allowance() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), OpenOptions::new()).unwrap();
        store.enable_graph().unwrap();
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "allowance", "one").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
        store.close().unwrap();
        let reopened = Store::open(directory.path(), OpenOptions::new()).unwrap();
        assert_eq!(
            reopened
                .native_graph
                .commits_since_reclaim
                .load(Ordering::Relaxed),
            crate::lifecycle::native_graph::automatic::RECLAIM_AFTER_COMMITS,
            "reopening retained history must not grant a fresh count allowance"
        );
        reopened.close().unwrap();
    }

    #[test]
    fn a_group_cap_rejection_is_definite_and_keeps_admissions_open() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::new(),
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap();
        store.enable_graph().unwrap();
        vfs.clear_events();
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let names = ["x".repeat(crate::wal::DEFAULT_MAX_GROUP_BYTES)];
        let writes: Vec<_> = names
            .iter()
            .map(|name| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "group-cap", name).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let error = store
            .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
            .err()
            .expect("legal graph envelope exceeds this store's one MiB group cap");
        assert!(
            matches!(
                error,
                crate::lifecycle::native_graph::NativeGraphError::Store(
                    crate::lifecycle::StoreError::WalWrite(
                        crate::wal::WalWriteError::GroupTooLarge { .. }
                    )
                )
            ),
            "{error:?}"
        );
        assert_eq!(store.admit_native_read().unwrap().bundle().sequence(), 0);
        assert!(
            !vfs.take()
                .iter()
                .any(|event| matches!(event, DurabilityEvent::Append(_)))
        );
        store.close().unwrap();
    }

    #[test]
    fn reclaim_proof_writes_follow_the_store_policy() {
        for (mode, tier, expected) in [
            (DurabilityMode::Derived, CommitTier::Ordered, None),
            (
                DurabilityMode::Durable,
                CommitTier::Ordered,
                Some(SyncKind::Barrier),
            ),
            (
                DurabilityMode::Durable,
                CommitTier::Durable,
                Some(SyncKind::Full),
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let vfs = Arc::new(RecordingVfs::default());
            let store = Store::open_with_test_dependencies(
                directory.path(),
                OpenOptions::new().with_durability(mode, tier),
                crate::lifecycle::StoreTestDependencies::new(
                    vfs.clone(),
                    Arc::new(crate::lifecycle::SystemMonotonicClock),
                ),
            )
            .unwrap();
            store.enable_graph().unwrap();
            vfs.clear_events();
            let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "policy", "node").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&image)),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap();
            super::super::consolidation::commit_maintenance(&store).unwrap();
            let events = vfs.take();
            let syncs: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    DurabilityEvent::Sync(_, kind) => Some(*kind),
                    _ => None,
                })
                .collect();
            assert_eq!(!syncs.is_empty(), expected.is_some());
            assert!(
                syncs.iter().all(|kind| Some(*kind) == expected),
                "{mode:?}/{tier:?}: {syncs:?}"
            );
            store.close().unwrap();
        }
    }

    #[test]
    fn writable_recovery_completes_reclaim_on_wal_ze() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let options =
            OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable);
        let store = Store::open_with_test_dependencies(
            directory.path(),
            options.clone(),
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap();
        store.enable_graph().unwrap();
        for index in 0..10 {
            let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "reclaim", &index.to_string())
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&contents)),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap();
        }
        super::super::consolidation::commit_maintenance(&store).unwrap();
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        vfs.arm_fault_after(FaultPoint::Delete, 1);
        let mut interrupted = false;
        for _ in 0..8 {
            if super::super::consolidation::commit_maintenance(&store).is_err() {
                interrupted = true;
                break;
            }
            store
                .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                .unwrap();
        }
        assert!(interrupted, "fixture must reach a candidate unlink");
        vfs.assert_fired_once();
        assert!(store.native_graph.has_pending_reclaim().unwrap());
        store.close().unwrap();
        let reader = Store::open(
            directory.path(),
            options
                .clone()
                .with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
        )
        .unwrap();
        assert!(reader.native_graph.has_pending_reclaim().unwrap());
        reader.close().unwrap();
        vfs.clear_events();
        let writer = Store::open_with_test_dependencies(
            directory.path(),
            options,
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap();
        assert!(!writer.native_graph.has_pending_reclaim().unwrap());
        assert!(vfs.take().iter().any(
            |event| matches!(event, DurabilityEvent::Append(path) if path.ends_with("wal.ze"))
        ));
        assert!(!directory.path().join("graph-root.ze").exists());
        writer.close().unwrap();
    }

    #[test]
    fn a_failure_after_append_is_indeterminate() {
        for point in [
            FaultPoint::PartialAppend,
            FaultPoint::WalSync,
            FaultPoint::Publish,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let vfs = Arc::new(RecordingVfs::default());
            let store = Store::open_with_test_dependencies(
                directory.path(),
                OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable),
                crate::lifecycle::StoreTestDependencies::new(
                    vfs.clone(),
                    Arc::new(crate::lifecycle::SystemMonotonicClock),
                ),
            )
            .unwrap();
            store.enable_graph().unwrap();
            let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            if point == FaultPoint::Publish {
                store
                    .native_graph
                    .fail_next_publication
                    .store(true, Ordering::Release);
            } else {
                vfs.arm_fault(point);
            }
            let error = store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "failure", "node").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&contents)),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .err()
                .expect("append fault must refuse");
            assert!(
                matches!(
                    error,
                    crate::lifecycle::native_graph::NativeGraphError::CommitIndeterminate { .. }
                ),
                "{error:?}"
            );
            if point == FaultPoint::Publish {
                assert!(
                    !store
                        .native_graph
                        .fail_next_publication
                        .load(Ordering::Acquire)
                );
            } else {
                vfs.assert_fired_once();
            }
            assert!(store.admit_native_read().is_err());
            store.close().unwrap();
            let reopened = Store::open(directory.path(), OpenOptions::new()).unwrap();
            assert_eq!(
                reopened.admit_native_read().unwrap().bundle().sequence(),
                u64::from(point != FaultPoint::PartialAppend)
            );
            reopened.close().unwrap();
        }
    }

    #[test]
    fn objects_are_synced_before_the_wal_append() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::new()
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_durability(DurabilityMode::Durable, CommitTier::Durable),
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap();
        store.enable_graph().unwrap();
        vfs.clear_events();
        let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let request = StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "first").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&contents)),
        };
        let result = store
            .apply_native_graph(&[request], &QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        assert_eq!(
            result.changed_generation(),
            Some(crate::property_graph::GraphGeneration::new(2))
        );
        let events = vfs.take();
        let append = events.iter().position(|event| matches!(event, DurabilityEvent::Append(path) if path.file_name().unwrap() == "wal.ze")).unwrap();
        let objects: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                DurabilityEvent::Create(path)
                    if path.extension().is_some_and(|ext| ext == "zgraph") =>
                {
                    Some(path)
                }
                _ => None,
            })
            .collect();
        assert!(!objects.is_empty());
        for path in objects {
            assert!(
                events[..append].contains(&DurabilityEvent::Sync(path.clone(), SyncKind::Full))
            );
        }
        assert!(events[..append].contains(&DurabilityEvent::Sync(
            directory.path().to_path_buf(),
            SyncKind::Full
        )));
        assert!(!directory.path().join("graph-root.ze").exists());
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("graph-wal-")
        }));
        store.close().unwrap();
    }
}

#[cfg(test)]
mod graph_checkpoint {
    use super::*;

    fn write_one(store: &Store) {
        let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "checkpoint", "node").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&contents)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
    }

    #[test]
    fn a_captured_base_binds_the_fold_generation_without_changing_graph_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), OpenOptions::new()).unwrap();
        store.enable_graph().unwrap();
        write_one(&store);
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let state = crate::lifecycle::native_graph::write::commit_state(lease.bundle());
        let fold = lease.bundle().base().fold;
        assert!(fold.manifest_generation > state.generation.get());
        let binding = crate::property_graph::storage::reclaim::SpillBinding {
            store: state.store,
            session: crate::property_graph::storage::artifact::ArtifactId::new(77).unwrap(),
            capture_generation: state.generation,
            target_generation: crate::property_graph::GraphGeneration::new(
                fold.manifest_generation + 1,
            ),
            sequence: state.sequence,
            serial_fence: state.high_waters.creation_serial,
        };
        let mut output = vec![0; 8192];
        let mut cancelled = || false;
        let mut resources = crate::property_graph::wal::WalResources::new(
            u64::MAX,
            crate::property_graph::wal::STACK_RESERVATION_BYTES,
            &mut cancelled,
        )
        .unwrap();
        let (length, _) = crate::property_graph::storage::reclaim::encode_captured_base(
            binding,
            fold,
            state,
            &mut output,
            &mut resources,
        )
        .unwrap();
        let captured = crate::property_graph::storage::reclaim::decode_captured_base(
            &output[..length],
            &mut resources,
        )
        .unwrap();
        assert_eq!(captured.fold, fold);
        assert_eq!(captured.state.generation, state.generation);
        drop(lease);
        store.close().unwrap();
    }

    #[test]
    fn refuses_a_document_tail_that_it_cannot_fold() {
        use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), OpenOptions::new()).unwrap();
        store.enable_graph().unwrap();
        write_one(&store);
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(91), Revision::new(1)),
                vec![1.0, 0.0],
            )]))
            .unwrap();
        let before = super::super::recovery::file_snapshot(directory.path());
        assert!(
            store
                .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                .is_err(),
            "a graph-only fold cannot count the later document batch again on reopen"
        );
        assert_eq!(
            super::super::recovery::file_snapshot(directory.path()),
            before
        );
        store.close().unwrap();
    }

    #[test]
    fn advances_graph_absorbed_through_without_sealing() {
        use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
        let directory = tempfile::tempdir().unwrap();
        let options = OpenOptions::new()
            .with_max_resident_bytes(256 * 1024 * 1024)
            .with_durability(DurabilityMode::Durable, CommitTier::Durable);
        let store = Store::open(directory.path(), options.clone()).unwrap();
        store.enable_graph().unwrap();
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(91), Revision::new(1)),
                vec![1.0, 0.0],
            )]))
            .unwrap();
        write_one(&store);
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        let manifest =
            crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 2)
                .unwrap();
        assert_eq!(manifest.generation, 4);
        assert_eq!(manifest.log_seq, 0);
        assert!(manifest.segments.is_empty());
        let graph = manifest.graph.as_ref().unwrap();
        assert_eq!(graph.graph_absorbed_through, 2);
        assert_eq!(graph.state().unwrap().generation.get(), 3);
        assert_eq!(graph.state().unwrap().sequence, 1);
        store.close().unwrap();
        let reopened = Store::open(
            directory.path(),
            options.with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
        )
        .unwrap();
        assert_eq!(reopened.snapshot().unwrap().generation(), 4);
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
        assert_eq!(
            reopened
                .admit_native_read()
                .unwrap()
                .bundle()
                .base()
                .fold
                .manifest_generation,
            4
        );
        reopened.close().unwrap();
    }

    #[test]
    fn rotates_the_wal_when_the_active_segment_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let options = OpenOptions::new()
            .with_max_resident_bytes(256 * 1024 * 1024)
            .with_durability(DurabilityMode::Durable, CommitTier::Durable);
        let store = Store::open(directory.path(), options.clone()).unwrap();
        store.enable_graph().unwrap();
        write_one(&store);
        assert!(
            !crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
                .unwrap()
                .records()
                .is_empty()
        );
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        let manifest =
            crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 1)
                .unwrap();
        assert_eq!(manifest.log_seq, 1);
        assert_eq!(manifest.graph.as_ref().unwrap().graph_absorbed_through, 1);
        assert!(
            crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
                .unwrap()
                .records()
                .is_empty()
        );
        store.close().unwrap();
        let reopened = Store::open(directory.path(), options).unwrap();
        assert_eq!(reopened.snapshot().unwrap().generation(), 3);
        assert_eq!(
            reopened
                .admit_native_read()
                .unwrap()
                .bundle()
                .base()
                .generation
                .get(),
            2
        );
        write_one_duplicate(&reopened);
        reopened.close().unwrap();
    }

    #[test]
    fn a_rotation_directory_sync_failure_refuses_later_document_writes() {
        use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::new()
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_durability(DurabilityMode::Durable, CommitTier::Durable),
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap();
        store.enable_graph().unwrap();
        write_one(&store);
        vfs.take();
        vfs.arm_fault_after(FaultPoint::DirectorySync, 1);
        assert!(
            store
                .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                .is_err()
        );
        vfs.assert_fired_once();
        assert!(vfs.take().iter().any(|event| matches!(event,
            DurabilityEvent::Rename(_, path) if path.file_name().is_some_and(|name| name == "wal.ze")
        )));
        let before = std::fs::read(directory.path().join("wal.ze")).unwrap();
        let result = store.ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0, 0.0],
        )]));
        assert!(
            matches!(
                result,
                Err(crate::ingest::IngestError::Store(
                    crate::lifecycle::StoreError::WalWrite(
                        crate::wal::WalWriteError::Failed { .. }
                    )
                ))
            ),
            "a failed WAL-name sync must stop every writer"
        );
        assert_eq!(
            std::fs::read(directory.path().join("wal.ze")).unwrap(),
            before
        );
        let _ = store.close();
    }

    fn write_one_duplicate(store: &Store) {
        let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let result = store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "checkpoint", "node").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&contents)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
        assert!(result.changed_generation().is_none());
        assert_eq!(store.snapshot().unwrap().generation(), 3);
    }
}

#[test]
fn ze76_ordered_commit_counts_only_full_syncs() {
    use crate::property_graph::resources::GraphResources;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("ordered-ledger");
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Ordered)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let write = |key: &str| {
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ordered", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
    };
    write("warmup");
    let resources = GraphResources::from_store(&store).unwrap();
    let before = resources.work_ledger().unwrap();
    vfs.take();
    write("measured");
    let events = vfs.take();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DurabilityEvent::Sync(_, SyncKind::Barrier)))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, DurabilityEvent::Sync(_, SyncKind::Full)))
    );
    let after = resources.work_ledger().unwrap();
    assert_eq!(after.full_sync_attempts - before.full_sync_attempts, 0);
    assert_eq!(after.full_sync_successes - before.full_sync_successes, 0);
    store.close().unwrap();
}
