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

#[derive(Clone, Debug, Eq, PartialEq)]
enum DurabilityEvent {
    Create(PathBuf),
    Append(PathBuf),
    Sync(PathBuf, SyncKind),
    Rename(PathBuf, PathBuf),
    Published(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultPoint {
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
}

#[derive(Default)]
struct FaultSchedule {
    armed: Option<FaultPoint>,
    fires: u64,
}

#[derive(Default)]
struct RecordingVfs {
    events: Arc<Mutex<Vec<DurabilityEvent>>>,
    wal_sync_gate: Arc<Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>>,
    faults: Arc<Mutex<FaultSchedule>>,
}

struct RecordingFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    events: Arc<Mutex<Vec<DurabilityEvent>>>,
    wal_sync_gate: Arc<Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>>,
    faults: Arc<Mutex<FaultSchedule>>,
}

impl RecordingVfs {
    fn take(&self) -> Vec<DurabilityEvent> {
        std::mem::take(&mut *self.events.lock().expect("recording VFS events"))
    }

    fn record(&self, event: DurabilityEvent) {
        self.events
            .lock()
            .expect("recording VFS events")
            .push(event);
    }

    fn arm_wal_full_sync(&self) -> (Arc<Barrier>, Arc<Barrier>) {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *self.wal_sync_gate.lock().expect("WAL sync gate") =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        (entered, release)
    }

    fn arm_fault(&self, point: FaultPoint) {
        let mut faults = self.faults.lock().expect("fault schedule");
        assert!(faults.armed.replace(point).is_none());
        faults.fires = 0;
    }

    fn fire(&self, point: FaultPoint) -> bool {
        let mut faults = self.faults.lock().expect("fault schedule");
        if faults.armed == Some(point) {
            faults.armed = None;
            faults.fires += 1;
            true
        } else {
            false
        }
    }

    fn assert_fired_once(&self) {
        let faults = self.faults.lock().expect("fault schedule");
        assert_eq!(faults.fires, 1);
        assert!(faults.armed.is_none());
        VERIFIED_FAULTS.with(|count| count.set(count.get() + faults.fires));
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
        self.inner.append_vectored(buffers)?;
        self.events
            .lock()
            .expect("recording VFS events")
            .push(DurabilityEvent::Append(self.path.clone()));
        Ok(())
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        if kind == SyncKind::Full
            && self
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("graph-wal-"))
        {
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
        if kind == SyncKind::Full
            && self
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("graph-wal-"))
        {
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
        StdVfs.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.write(path, bytes)
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
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        if self.fire(FaultPoint::OpenAppend) {
            return Err(std::io::Error::other(
                "scheduled append-handle open failure",
            ));
        }
        Ok(Box::new(RecordingFile {
            inner: StdVfs.open_append(path)?,
            path: path.to_path_buf(),
            events: Arc::clone(&self.events),
            wal_sync_gate: Arc::clone(&self.wal_sync_gate),
            faults: Arc::clone(&self.faults),
        }))
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
                matches!(event, DurabilityEvent::Rename(_, to) if to.file_name().is_some_and(|name| name == "graph-root.ze")));
            if selected && self.fire(FaultPoint::SelectorSync) {
                return Err(std::io::Error::other(
                    "scheduled selector directory sync failure",
                ));
            }
            let point = if path.is_dir() {
                FaultPoint::DirectorySync
            } else {
                FaultPoint::ObjectSync
            };
            if self.fire(point) {
                return Err(std::io::Error::other("scheduled path Full-sync failure"));
            }
        }
        StdVfs.sync(path, kind)?;
        self.record(DurabilityEvent::Sync(path.to_path_buf(), kind));
        Ok(())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.delete(path)
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
        !path.join("wal.ze").exists(),
        "legacy WAL must not bootstrap graph"
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
        .rposition(|event| {
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
            matches!(event, DurabilityEvent::Sync(target, SyncKind::Full) if target.file_name().is_some_and(|name| name.to_string_lossy().starts_with("graph-wal-")))
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

fn property_fixture() -> Vec<GraphProperty<'static>> {
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
struct GenerationSnapshot {
    generation: u64,
    revision: u64,
    canonical: Vec<u8>,
    text: Vec<u8>,
    vector: Vec<u32>,
    old_relationship: bool,
    new_relationship: bool,
    out: Vec<RelId>,
    incoming: Vec<RelId>,
    sparse_text: bool,
    sparse_vector: bool,
}

fn snapshot_for_lease(
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
        assert_eq!(
            store
                .admit_native_read()
                .expect("admission while WAL Full sync is held")
                .bundle()
                .base()
                .generation
                .get(),
            1,
            "publication must remain old until WAL Full sync returns"
        );
        release.wait();
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
            generation: 1,
            revision: 1,
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
            generation: 2,
            revision: 2,
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
    assert_eq!(replay[0].generation.get(), 1);
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
    assert_eq!(mixed_receipts[0].generation.get(), 1);
    assert!(!mixed_receipts[1].replayed);
    assert_eq!(mixed_receipts[1].generation.get(), 2);
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
        assert_eq!(receipt[0].generation.get(), revision);
    }
    let before = store.admit_native_read().expect("generation 64 admission");
    assert_eq!(
        before.bundle().prepared_inventories().len(),
        64,
        "each complete allocation inventory remains protected until consolidation"
    );
    let checkpoint_before = before.bundle().root_envelope();
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
    assert_eq!(receipt[0].generation.get(), 65);
    let current = store.admit_native_read().expect("generation 65 admission");
    assert_ne!(current.bundle().root_envelope(), checkpoint_before);
    assert_eq!(current.bundle().root_envelope().object.generation.get(), 64);
    assert_eq!(current.bundle().base().generation.get(), 65);
    let selected = super::super::persistence::classify_native_graph(vfs.as_ref(), &path);
    assert!(matches!(
        selected,
        super::super::persistence::NativeStoreClassification::Complete { root, .. }
            if root == current.bundle().root_envelope()
    ));
    drop(current);

    let events = vfs.take();
    let rotated_wal = events
        .iter()
        .position(|event| {
            matches!(event, DurabilityEvent::Create(path) if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("graph-wal-")))
        })
        .expect("rotated WAL create");
    let selected_root = events
        .iter()
        .position(|event| {
            matches!(event, DurabilityEvent::Rename(_, target) if target.file_name().is_some_and(|name| name == "graph-root.ze"))
        })
        .expect("checkpoint selector rename");
    let changed_append = events
        .iter()
        .rposition(|event| matches!(event, DurabilityEvent::Append(_)))
        .expect("post-checkpoint append");
    assert!(rotated_wal < selected_root && selected_root < changed_append);

    let blocked = [StructuredWrite {
        key: create[0].key,
        revision: GraphRevision::new(66).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: Some(WriteImage::Node(&image)),
    }];
    for point in [
        FaultPoint::ObjectSync,
        FaultPoint::DirectorySync,
        FaultPoint::OpenAppend,
        FaultPoint::Rename,
        FaultPoint::SelectorSync,
    ] {
        vfs.arm_fault(point);
        assert!(matches!(
            store.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new())),
            Err(super::super::NativeGraphError::Io { .. })
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
            65
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
    assert_eq!(
        store
            .apply_native_graph(&blocked, &QueryControl::Cancel(CancelToken::new()))
            .expect("changed admission resumes")[0]
            .generation
            .get(),
        66
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
    let key = "k".repeat(512 * 1024);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut rotated = false;
    for revision in 1..=40 {
        let (old_identity, old_bytes, old_count) = {
            let guard = store.native_graph.writer.lock().unwrap();
            let writer = guard.as_ref().unwrap();
            assert_eq!(
                std::fs::metadata(&writer.wal.path).unwrap().len() as usize,
                writer.wal.bytes
            );
            (
                writer.wal.identity,
                writer.wal.bytes,
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
        assert_eq!(
            store
                .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
                .unwrap()[0]
                .generation
                .get(),
            revision
        );
        let guard = store.native_graph.writer.lock().unwrap();
        let writer = guard.as_ref().unwrap();
        assert_eq!(
            std::fs::metadata(&writer.wal.path).unwrap().len() as usize,
            writer.wal.bytes
        );
        let tail = writer.wal.bytes - crate::property_graph::wal::HEADER_BYTES;
        assert!(tail <= crate::property_graph::wal::MAX_ENVELOPE_BYTES);
        if writer.wal.identity != old_identity {
            assert!(
                old_count < 64,
                "byte trigger precedes envelope-count trigger"
            );
            assert_eq!(writer.complete_envelopes, 1);
            assert!(
                old_bytes - crate::property_graph::wal::HEADER_BYTES + tail
                    > crate::property_graph::wal::MAX_ENVELOPE_BYTES,
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
    let old_root = admitted.bundle().root_envelope();
    drop(admitted);
    let before = store
        .capture_native_read_roots()
        .expect("initial protected capture");
    assert!(before.contains(old_root));
    assert!(
        before
            .wal()
            .is_some_and(|wal| wal.first_sequence == 1 && wal.bytes > 0)
    );
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
        let capture_store = Arc::clone(&store);
        let captured = scope.spawn(move || {
            capture_store
                .capture_native_read_roots()
                .expect("racing capture")
        });
        release.wait();
        let old_lease = retained.join().expect("read thread");
        let receipt = commit.join().expect("write thread");
        assert_eq!(receipt[0].generation.get(), 2);
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
    assert_eq!(current.bundle().root_envelope(), old_root);
    let checkpoint_catalog = current.bundle().catalog();
    let raw_lagged = super::super::NativeGraphBundleInput {
        base: crate::property_graph::staging::BaseIdentity {
            store: current.bundle().base().store,
            generation: crate::property_graph::GraphGeneration::new(3),
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
    store
        .commit_native_graph_maintenance(&maintenance, &QueryControl::Cancel(CancelToken::new()))
        .expect("bounded checkpoint maintenance");
    let after = store
        .capture_native_read_roots()
        .expect("post-checkpoint capture");
    assert!(after.serial_fence() > before_fence);
    assert!(after.wal().is_some_and(|wal| wal.first_sequence == 3));
    assert!(after.prepared.is_empty());
    let selected = store
        .admit_native_read()
        .expect("selected maintenance root");
    assert_ne!(selected.bundle().root_envelope(), old_root);
    assert_eq!(selected.bundle().base().generation.get(), 2);
    assert_eq!(selected.bundle().catalog(), checkpoint_catalog);
    drop(selected);
    drop(maintenance);
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
        2
    );
    assert_eq!(
        store
            .apply_native_graph(&relationship, &QueryControl::Cancel(CancelToken::new()))
            .unwrap()[0]
            .generation
            .get(),
        3
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
            1
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
        assert_eq!(retry[0].generation.get(), 2);
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
            assert!(vfs.take().iter().any(|event| matches!(event, DurabilityEvent::Sync(path, SyncKind::Full) if path.file_name().unwrap().to_string_lossy().starts_with("graph-wal-"))));
        } else {
            vfs.assert_fired_once();
        }
        assert_eq!(old.bundle().base().generation.get(), 0);
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
            1
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
        0
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
    let selected_before = vfs
        .read(&path.join("graph-root.ze"))
        .expect("selected root");
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
        vfs.read(&path.join("graph-root.ze"))
            .expect("unchanged selected root"),
        selected_before
    );
    assert!(matches!(
        super::super::persistence::classify_native_graph(vfs.as_ref(), &path),
        super::super::persistence::NativeStoreClassification::Complete { .. }
    ));
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
        Err(super::super::NativeGraphError::StoreInitializationIncomplete)
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
        Err(super::super::NativeGraphError::Invalid(
            "incompatible native graph root"
        ))
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
        Err(super::super::NativeGraphError::Invalid(
            "corrupt native graph root"
        ))
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
    let _ = run_ze39_retained_reader_and_new_admission_observe_whole_generations();
}
#[cfg_attr(test, test)]
fn ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work() {
    let _ = run_ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work();
}
#[cfg_attr(test, test)]
fn ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state() {
    let _ = run_ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state();
}
#[cfg_attr(test, test)]
fn ze39_protection_capture_and_maintenance_recheck_are_atomic() {
    let _ = run_ze39_protection_capture_and_maintenance_recheck_are_atomic();
}
#[cfg_attr(test, test)]
fn ze39_precommit_failure_keeps_graph_and_path_ownership_private() {
    let _ = run_ze39_precommit_failure_keeps_graph_and_path_ownership_private();
}
#[cfg_attr(test, test)]
fn ze39_commit_attempt_errors_are_indeterminate_and_stop_admission() {
    let _ = run_ze39_commit_attempt_errors_are_indeterminate_and_stop_admission();
}
#[cfg_attr(test, test)]
fn ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary() {
    let _ = run_ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary();
}
#[cfg_attr(test, test)]
fn ze39_fresh_create_failures_never_adopt_or_replace_identity() {
    let _ = run_ze39_fresh_create_failures_never_adopt_or_replace_identity();
}

#[cfg(feature = "test-support")]
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
        VERIFIED_FAULTS.with(|count| count.set(0));
        probe();
        let fires = VERIFIED_FAULTS.with(std::cell::Cell::get);
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
