#![allow(clippy::expect_used, clippy::panic)]

use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, ManualMonotonicClock, OpenOptions, QueryControl, SnapshotLease, Store,
};
use zeppelin_embed::property_graph::query::resources::{MemoryError, QueryMemory};
use zeppelin_embed::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeError, RuntimeLimits, WorkKind,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::storage::adjacency::{
    Edge, MAX_MERGED_ENTRIES, MERGE_STATE_BYTES, RangeScratch,
};
use zeppelin_embed::property_graph::storage::tree::directory::{TreeError, TreeResources};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

struct View {
    token: QueryView,
    lease: SnapshotLease,
}
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }

    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}

fn view(store: &Store) -> View {
    View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("store identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("retained lease"),
    }
}

#[test]
fn query_tree_workspace_uses_the_runtime_memory_and_releases_typed_refusals() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let retained = view(&store);
    let control = QueryControl::Cancel(CancelToken::new());

    {
        let memory = QueryMemory::new(&shared, 512 * 1024).expect("query allowance");
        let mut context =
            RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
                .expect("runtime");
        let before = memory.reserved_bytes();
        {
            let resources = TreeResources::for_query(&mut context).expect("query tree resources");
            assert_eq!(resources.reserved_bytes(), 256 * 1024);
            assert_eq!(memory.reserved_bytes(), before + 256 * 1024);
        }
        assert_eq!(memory.reserved_bytes(), before);
    }

    {
        let memory = QueryMemory::new(&shared, 128 * 1024).expect("small query allowance");
        let mut context =
            RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
                .expect("runtime");
        let before = memory.reserved_bytes();
        assert!(matches!(
            TreeResources::for_query(&mut context),
            Err(TreeError::Runtime(RuntimeError::Memory(MemoryError::Limit)))
        ));
        assert_eq!(memory.reserved_bytes(), before);
    }

    drop(retained);
    store.close().expect("close");
}

#[test]
fn query_tree_admission_preserves_deadline_and_close_priority_without_charges() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = std::sync::Arc::new(
        Store::open(
            directory.path(),
            OpenOptions::new()
                .with_max_resident_bytes(2 * 1024 * 1024)
                .with_reader_drain_timeout(std::time::Duration::ZERO),
        )
        .expect("store"),
    );
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 128 * 1024).expect("small query allowance");
    let retained = view(&store);

    let deadline =
        QueryControl::Deadline(Deadline::after(std::time::Duration::ZERO).expect("zero deadline"));
    let before_deadline = memory.reserved_bytes();
    assert!(matches!(
        RuntimeContext::new(&retained, &deadline, &memory, RuntimeLimits::default()),
        Err(RuntimeError::Value(QueryError::Timeout))
    ));
    assert_eq!(memory.reserved_bytes(), before_deadline);

    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let mut context = RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
        .expect("runtime");
    let before_query = memory.reserved_bytes();
    let before_shared = shared.reserved_bytes().expect("shared reserved bytes");
    let registry_bytes = store.stats().expect("stats").native_graph_bytes;
    let close_store = std::sync::Arc::clone(&store);
    let closing = std::thread::spawn(move || close_store.close());
    retained
        .lease
        .wait_for_close_cancellation()
        .expect("close cancellation");
    token.cancel();
    assert!(matches!(
        TreeResources::for_query(&mut context),
        Err(TreeError::Runtime(RuntimeError::Value(
            QueryError::ReadCancelled
        )))
    ));
    assert_eq!(memory.reserved_bytes(), before_query);
    assert_eq!(
        shared.reserved_bytes().expect("shared reserved bytes"),
        before_shared - registry_bytes
    );

    drop(context);
    drop(retained);
    closing.join().expect("close thread").expect("store close");
}

#[test]
fn query_range_scratch_uses_the_same_memory_owner_and_releases_all_capacity() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 1024 * 1024).expect("query allowance");
    let other = QueryMemory::new(&shared, 1024 * 1024).expect("other query allowance");
    let retained = view(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
        .expect("runtime");
    let mut resources = TreeResources::for_query(&mut context).expect("query tree resources");

    let other_before = other.reserved_bytes();
    assert!(matches!(
        RangeScratch::for_query(&other, &mut resources),
        Err(TreeError::Invalid("query memory owner mismatch"))
    ));
    assert_eq!(other.reserved_bytes(), other_before);

    let before = memory.reserved_bytes();
    {
        let scratch =
            RangeScratch::for_query(&memory, &mut resources).expect("query range scratch");
        let expected = std::mem::size_of::<RangeScratch<'_>>()
            + MERGE_STATE_BYTES
            + MAX_MERGED_ENTRIES * std::mem::size_of::<Edge>();
        assert_eq!(scratch.owned_bytes(), expected);
        assert_eq!(memory.reserved_bytes(), before + expected);
    }
    assert_eq!(memory.reserved_bytes(), before);

    drop(resources);
    drop(context);
    drop(retained);
    store.close().expect("close");
}

#[test]
fn query_range_scratch_checks_close_before_memory_refusal_without_charging() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = std::sync::Arc::new(
        Store::open(
            directory.path(),
            OpenOptions::new()
                .with_max_resident_bytes(2 * 1024 * 1024)
                .with_reader_drain_timeout(std::time::Duration::ZERO),
        )
        .expect("store"),
    );
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 320 * 1024).expect("small query allowance");
    let retained = view(&store);
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let mut context = RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
        .expect("runtime");
    let mut resources = TreeResources::for_query(&mut context).expect("query tree resources");
    let before_query = memory.reserved_bytes();
    let before_shared = shared.reserved_bytes().expect("shared reserved bytes");
    let registry_bytes = store.stats().expect("stats").native_graph_bytes;

    let close_store = std::sync::Arc::clone(&store);
    let closing = std::thread::spawn(move || close_store.close());
    retained
        .lease
        .wait_for_close_cancellation()
        .expect("close cancellation");
    token.cancel();
    assert!(matches!(
        RangeScratch::for_query(&memory, &mut resources),
        Err(TreeError::Runtime(RuntimeError::Value(
            QueryError::ReadCancelled
        )))
    ));
    assert_eq!(memory.reserved_bytes(), before_query);
    assert_eq!(
        shared.reserved_bytes().expect("shared reserved bytes"),
        before_shared - registry_bytes
    );

    drop(resources);
    drop(context);
    drop(retained);
    closing.join().expect("close thread").expect("store close");
}

#[test]
fn directory_cursor_rejects_runtime_context_swaps_and_latches_both_apis() {
    use zeppelin_embed::property_graph::storage::artifact::{FramedBlock, PhysicalRef};
    use zeppelin_embed::property_graph::storage::tree::TreeKind;
    use zeppelin_embed::property_graph::storage::tree::directory::{
        BlockSource, DirectoryCursor, DirectoryRoot,
    };

    struct EmptySource;
    impl BlockSource for EmptySource {
        fn resolve<'a>(
            &'a self,
            _: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            Err(TreeError::Missing)
        }
    }

    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 2 * 1024 * 1024).expect("query allowance");
    let retained = view(&store);
    let first_control = QueryControl::Cancel(CancelToken::new());
    let second_control = QueryControl::Cancel(CancelToken::new());
    let root = DirectoryRoot::empty(
        StoreInstanceId::new(1).expect("store identity"),
        TreeKind::Nodes,
        GraphGeneration::new(0),
    );

    let mut first =
        RuntimeContext::new(&retained, &first_control, &memory, RuntimeLimits::default())
            .expect("first runtime");
    let mut second = RuntimeContext::new(
        &retained,
        &second_control,
        &memory,
        RuntimeLimits::default(),
    )
    .expect("second runtime");
    {
        let mut first_resources = TreeResources::for_query(&mut first).expect("first resources");
        let mut second_resources = TreeResources::for_query(&mut second).expect("second resources");
        let first_work = first_resources.work();
        let second_work = second_resources.work();
        let mut cursor =
            DirectoryCursor::seek(&EmptySource, root, None, &mut first_resources).expect("cursor");
        assert!(matches!(
            cursor.next_entry(&mut second_resources),
            Err(TreeError::Invalid("query cursor owner mismatch"))
        ));
        assert!(matches!(
            cursor.next_entry(&mut first_resources),
            Err(TreeError::Invalid("cursor previously failed"))
        ));
        assert_eq!(first_resources.work(), first_work + 1);
        assert_eq!(second_resources.work(), second_work);
    }
    assert_eq!(first.counters().get(WorkKind::Scans), 1);
    assert_eq!(second.counters().get(WorkKind::Scans), 0);

    let mut third =
        RuntimeContext::new(&retained, &first_control, &memory, RuntimeLimits::default())
            .expect("third runtime");
    let mut fourth = RuntimeContext::new(
        &retained,
        &second_control,
        &memory,
        RuntimeLimits::default(),
    )
    .expect("fourth runtime");
    {
        let mut third_resources = TreeResources::for_query(&mut third).expect("third resources");
        let mut fourth_resources = TreeResources::for_query(&mut fourth).expect("fourth resources");
        let third_work = third_resources.work();
        let fourth_work = fourth_resources.work();
        let mut cursor =
            DirectoryCursor::seek(&EmptySource, root, None, &mut third_resources).expect("cursor");
        assert!(matches!(
            cursor.next(&mut [0; 16], &mut [0; 16], &mut fourth_resources),
            Err(TreeError::Invalid("query cursor owner mismatch"))
        ));
        assert!(matches!(
            cursor.next(&mut [0; 16], &mut [0; 16], &mut third_resources),
            Err(TreeError::Invalid("cursor previously failed"))
        ));
        assert_eq!(third_resources.work(), third_work + 1);
        assert_eq!(fourth_resources.work(), fourth_work);
    }
    assert_eq!(third.counters().get(WorkKind::Scans), 1);
    assert_eq!(fourth.counters().get(WorkKind::Scans), 0);

    drop((first, second, third, fourth));
    drop(retained);
    store.close().expect("close");
}

#[test]
fn empty_directory_lookup_and_scan_charge_typed_cumulative_work() {
    use zeppelin_embed::property_graph::storage::artifact::{FramedBlock, PhysicalRef};
    use zeppelin_embed::property_graph::storage::tree::TreeKind;
    use zeppelin_embed::property_graph::storage::tree::directory::{
        BlockSource, DirectoryCursor, DirectoryRoot, lookup_entry,
    };

    struct EmptySource;
    impl BlockSource for EmptySource {
        fn resolve<'a>(
            &'a self,
            _: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            Err(TreeError::Missing)
        }
    }

    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 512 * 1024).expect("query allowance");
    let retained = view(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::Lookups, 1)
        .expect("lookup limit")
        .with_limit(WorkKind::Scans, 1)
        .expect("scan limit");
    let mut context = RuntimeContext::new(&retained, &control, &memory, limits).expect("runtime");
    {
        let mut resources = TreeResources::for_query(&mut context).expect("tree resources");
        let root = DirectoryRoot::empty(
            StoreInstanceId::new(1).expect("store identity"),
            TreeKind::Nodes,
            GraphGeneration::new(0),
        );
        let key = 1u128.to_le_bytes();
        assert!(
            lookup_entry(&EmptySource, root, &key, &mut resources)
                .expect("missing lookup")
                .is_none()
        );
        let mut cursor =
            DirectoryCursor::seek(&EmptySource, root, None, &mut resources).expect("empty scan");
        assert!(
            cursor
                .next_entry(&mut resources)
                .expect("empty cursor")
                .is_none()
        );
        assert!(matches!(
            lookup_entry(&EmptySource, root, &key, &mut resources),
            Err(TreeError::Runtime(RuntimeError::Limit(WorkKind::Lookups)))
        ));
    }
    assert_eq!(context.counters().get(WorkKind::Lookups), 1);
    assert_eq!(context.counters().get(WorkKind::Scans), 1);

    drop(context);
    drop(retained);
    store.close().expect("close");
}

#[test]
fn payload_copies_charge_only_bytes_actually_copied_before_refusal() {
    use zeppelin_embed::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind, FramedBlock,
        PhysicalRef,
    };
    use zeppelin_embed::property_graph::storage::payload::PayloadRef;
    use zeppelin_embed::property_graph::storage::stream::{PayloadCursor, PayloadSlice};
    use zeppelin_embed::property_graph::storage::tree::directory::BlockSource;

    struct Source {
        scoped: bool,
        bytes: Vec<u8>,
        reference: PhysicalRef,
        identity: ArtifactIdentity,
    }
    impl BlockSource for Source {
        fn scoped_blocks(&self) -> bool {
            self.scoped
        }
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            if reference != self.reference {
                return Err(TreeError::Missing);
            }
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((self.identity.store, self.identity.artifact)),
                &self.bytes,
            )?;
            Ok(frame.framed_block(reference)?)
        }
    }

    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(7).expect("store identity"),
        artifact: ArtifactId::new(9).expect("artifact identity"),
        generation: GraphGeneration::new(3),
        creation_serial: 11,
    };
    let payload = b"payload";
    let blocks = [Block {
        kind: BlockKind::StoredText,
        payload,
    }];
    let mut bytes =
        vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).expect("encoded length")];
    artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes)
        .expect("encoded source");
    let frame = artifact::decode(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        &bytes,
    )
    .expect("decoded source");
    let reference = frame.reference(0).expect("payload reference");
    let source = Source {
        scoped: false,
        bytes,
        reference,
        identity,
    };
    let payload = PayloadRef::new(BlockKind::StoredText, 7, reference).expect("logical payload");
    let slice = PayloadSlice::new(&source, identity.store, identity.generation, payload);

    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 512 * 1024).expect("query allowance");
    let retained = view(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, 7)
        .expect("copy limit");
    let mut context = RuntimeContext::new(&retained, &control, &memory, limits).expect("runtime");
    {
        let mut resources = TreeResources::for_query(&mut context).expect("tree resources");
        let mut first = [0; 3];
        assert_eq!(
            slice.read_at(1, &mut first, &mut resources).expect("copy"),
            3
        );
        assert_eq!(&first, b"ayl");
        let mut cursor = PayloadCursor::new(slice.subslice(3, 4).expect("cursor slice"));
        assert_eq!(
            cursor.read_array::<4>(&mut resources).expect("fixed copy"),
            *b"load"
        );
        let mut refused = [0xaa];
        assert!(matches!(
            slice.read_at(0, &mut refused, &mut resources),
            Err(TreeError::Runtime(RuntimeError::Limit(
                WorkKind::CopiedBytes
            )))
        ));
        assert_eq!(refused, [0xaa]);
    }
    assert_eq!(context.counters().get(WorkKind::CopiedBytes), 7);

    drop(context);
    let scoped_source = Source {
        scoped: true,
        ..source
    };
    let slice = PayloadSlice::new(&scoped_source, identity.store, identity.generation, payload);
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, 8)
        .expect("cache fill plus first field");
    let mut context = RuntimeContext::new(&retained, &control, &memory, limits).expect("runtime");
    {
        let mut resources = TreeResources::for_query(&mut context).expect("resources");
        let mut cursor =
            PayloadCursor::new_with_resources(slice, &mut resources).expect("copied cache");
        assert_eq!(
            cursor.read_array::<1>(&mut resources).expect("prime cache"),
            *b"p"
        );
        let mut output = [0xaa];
        let result = cursor.read_array::<1>(&mut resources);
        if let Ok(value) = result.as_ref() {
            output = *value;
        }
        assert!(matches!(
            result,
            Err(TreeError::Runtime(RuntimeError::Limit(
                WorkKind::CopiedBytes
            )))
        ));
        assert_eq!(output, [0xaa], "refusal returns no partial field");
        assert_eq!(
            cursor.position(),
            1,
            "refusal does not advance the cached cursor"
        );
    }
    assert_eq!(context.counters().get(WorkKind::CopiedBytes), 8);
    drop(context);
    drop(retained);
    store.close().expect("close");
}

#[test]
fn query_storage_deadline_expires_after_runtime_admission_before_source_or_copy() {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::time::Duration;
    use zeppelin_embed::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind, FramedBlock,
        PhysicalRef,
    };
    use zeppelin_embed::property_graph::storage::payload::PayloadRef;
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::storage::tree::directory::BlockSource;

    struct Source {
        bytes: Vec<u8>,
        reference: PhysicalRef,
        identity: ArtifactIdentity,
        calls: Cell<usize>,
    }
    impl BlockSource for Source {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            self.calls.set(self.calls.get() + 1);
            if reference != self.reference {
                return Err(TreeError::Missing);
            }
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((self.identity.store, self.identity.artifact)),
                &self.bytes,
            )?;
            Ok(frame.framed_block(reference)?)
        }
    }

    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(17).expect("store identity"),
        artifact: ArtifactId::new(29).expect("artifact identity"),
        generation: GraphGeneration::new(3),
        creation_serial: 31,
    };
    let blocks = [Block {
        kind: BlockKind::StoredText,
        payload: b"deadline",
    }];
    let mut bytes =
        vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).expect("encoded length")];
    artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes)
        .expect("encoded source");
    let reference = artifact::decode(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        &bytes,
    )
    .expect("decoded source")
    .reference(0)
    .expect("payload reference");
    let source = Source {
        bytes,
        reference,
        identity,
        calls: Cell::new(0),
    };
    let payload = PayloadRef::new(BlockKind::StoredText, 8, reference).expect("logical payload");
    let slice = PayloadSlice::new(&source, identity.store, identity.generation, payload);

    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let memory = QueryMemory::new(&shared, 512 * 1024).expect("query allowance");
    let retained = view(&store);

    let admission_clock = Arc::new(ManualMonotonicClock::new());
    let admission_control = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(1), admission_clock.clone())
            .expect("deadline"),
    );
    let mut admission_context = RuntimeContext::new(
        &retained,
        &admission_control,
        &memory,
        RuntimeLimits::default(),
    )
    .expect("runtime before deadline");
    let before_admission = memory.reserved_bytes();
    admission_clock.advance(Duration::from_secs(2));
    assert!(matches!(
        TreeResources::for_query(&mut admission_context),
        Err(TreeError::Runtime(RuntimeError::Value(QueryError::Timeout)))
    ));
    assert_eq!(memory.reserved_bytes(), before_admission);
    assert_eq!(admission_context.counters().get(WorkKind::CopiedBytes), 0);
    drop(admission_context);

    let read_clock = Arc::new(ManualMonotonicClock::new());
    let read_control = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(1), read_clock.clone())
            .expect("deadline"),
    );
    let mut read_context =
        RuntimeContext::new(&retained, &read_control, &memory, RuntimeLimits::default())
            .expect("runtime before deadline");
    {
        let mut resources =
            TreeResources::for_query(&mut read_context).expect("resources before deadline");
        let before_work = resources.work();
        let before_query = memory.reserved_bytes();
        let before_shared = shared.reserved_bytes().expect("shared reserved bytes");
        let mut output = [0xa5; 8];
        read_clock.advance(Duration::from_secs(2));
        assert!(matches!(
            slice.read_at(0, &mut output, &mut resources),
            Err(TreeError::Runtime(RuntimeError::Value(QueryError::Timeout)))
        ));
        assert_eq!(source.calls.get(), 0, "deadline must precede source access");
        assert_eq!(output, [0xa5; 8], "deadline must precede copying");
        assert_eq!(resources.work(), before_work);
        assert_eq!(memory.reserved_bytes(), before_query);
        assert_eq!(
            shared.reserved_bytes().expect("shared reserved bytes"),
            before_shared
        );
    }
    assert_eq!(read_context.counters().get(WorkKind::CopiedBytes), 0);

    drop(read_context);
    drop(retained);
    store.close().expect("close");
}
