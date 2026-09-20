//! Actual private producer histories; admitted-base/catalog ownership is an
//! explicit test fixture, not ZE45 view/lease or publication qualification.
use super::*;
use zeppelin_embed::property_graph::storage::adjacency::*;
use zeppelin_embed::property_graph::wal::{
    ArtifactDescriptor, CommitState, ReferenceList, RequiredRef, WalGraphRoots,
};

fn bootstrap(base: BaseIdentity, sequence: u64) -> CommitState<'static> {
    let artifact = ArtifactId::new(99).unwrap();
    CommitState {
        store: base.store,
        generation: base.generation,
        sequence,
        graph: WalGraphRoots::default(),
        catalog: RequiredRef {
            object: ArtifactDescriptor {
                store: base.store,
                artifact,
                generation: base.generation,
                serial: 1,
                bytes: 200,
                family: 17,
                version: 1,
                checksum: 0,
            },
            block: PhysicalRef {
                artifact,
                offset: 96,
                length: 24,
                kind: BlockKind::CommitParticipant,
                version: 1,
            },
        },
        vector: None,
        text: None,
        reclaim: None,
        high_waters: zeppelin_embed::property_graph::wal::HighWaters {
            node: Empty.high_waters().node,
            relationship: Empty.high_waters().relationship,
            creation_serial: 1,
            ..Default::default()
        },
        prepared_inventories: ReferenceList::Values(&[]),
    }
}

#[test]
fn actual_native_producer_refuses_large_batch_at_unchanged_work_limit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 200_000_000).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    with_local_refs(|refs| {
        let mut view = GraphBatchReadView::new(&Empty, &writer, 2051, &mut |_| Ok(())).unwrap();
        for index in 0..2 {
            view.create(
                BatchEntityRef::Node(NodeRef::Local(refs.node(index).unwrap())),
                WriteImage::Node(&node),
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        for index in 0..2049 {
            view.create(
                BatchEntityRef::Relationship(RelRef::Local(refs.relationship(index).unwrap())),
                WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                },
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        let batch = view.finish(&mut |_| Ok(())).unwrap();
        let base = Empty.identity();
        let roots = GraphRoots::from_references(base.store, base.generation, [None; 8]).unwrap();
        let mut objects = packed(&Missing, 1, &memory, &mut r);
        let result = prepare_native_graph(
            &mut objects,
            &batch,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base,
                    roots,
                },
                committed: bootstrap(base, 100),
            },
            &Catalog { base, symbols: &[] },
            None,
            &memory,
            &mut r,
        );
        eprintln!(
            "2049 incoming: storage_peak={} writer={} shared={} packs={} result={:?}",
            memory.peak_reserved_bytes(),
            writer.reserved_bytes(),
            shared.reserved_bytes().unwrap(),
            objects.len(),
            result.as_ref().map(|c| c.sequence())
        );
        assert!(matches!(&result, Err(TreeError::Work)));
        drop(result);
        assert_eq!(objects.abort_inventory().count(), objects.len());
        assert!(
            objects.artifact(0).is_err(),
            "failed preparation cannot finalize an artifact"
        );
        drop(objects);
        assert_eq!(
            memory.reserved_bytes(),
            r.reserved_bytes() as usize + std::mem::size_of::<StorageMemory<'_>>()
        );
    });
}

/// Fixture external retained bytes are charged to the same authentic aggregate,
/// independently of the current writer's private 32MiB participant.
struct ExternalBytes {
    bytes: Vec<u8>,
    identity: ArtifactIdentity,
    _charge: zeppelin_embed::property_graph::resources::GraphReservation,
}
struct ChargedVec<T> {
    values: Vec<T>,
    _charge: zeppelin_embed::property_graph::resources::GraphReservation,
}
impl<T> ChargedVec<T> {
    fn new(shared: &GraphResources, capacity: usize) -> Self {
        let charge = shared.reserve(capacity * std::mem::size_of::<T>()).unwrap();
        let mut values = Vec::new();
        values.try_reserve_exact(capacity).unwrap();
        assert_eq!(values.capacity(), capacity);
        Self {
            values,
            _charge: charge,
        }
    }
}
struct Frozen<'a> {
    previous: &'a dyn BlockSource,
    frames: &'a [ArtifactFrame<'a>],
}
impl BlockSource for Frozen<'_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        for frame in self.frames {
            r.step(1)?;
            if frame.identity().artifact == reference.artifact {
                return frame.framed_block(reference).map_err(TreeError::Format);
            }
        }
        self.previous.resolve(reference, r)
    }
}
struct Forward<'a>(&'a dyn BlockSource);
impl BlockSource for Forward<'_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.0.resolve(reference, r)
    }
}
trait FinalPacks {
    fn len(&self) -> usize;
    fn artifact(
        &self,
        index: usize,
    ) -> Result<zeppelin_embed::property_graph::storage::prepared::PreparedArtifact<'_>, TreeError>;
}
impl<S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>> FinalPacks
    for PreparedObjects<'_, '_, S, F>
{
    fn len(&self) -> usize {
        PreparedObjects::len(self)
    }
    fn artifact(
        &self,
        index: usize,
    ) -> Result<zeppelin_embed::property_graph::storage::prepared::PreparedArtifact<'_>, TreeError>
    {
        PreparedObjects::artifact(self, index)
    }
}
struct SplitPacks<'a, 'b, S, F> {
    pages: PreparedObjects<'a, 'b, S, F>,
    records: PreparedObjects<'a, 'b, S, F>,
}
impl<S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>> BlockSource
    for SplitPacks<'_, '_, S, F>
{
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        if reference.artifact.get() % 2 == 1 {
            self.pages.resolve(reference, r)
        } else {
            self.records.resolve(reference, r)
        }
    }
}
impl<S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>> BlockSink
    for SplitPacks<'_, '_, S, F>
{
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        if kind == BlockKind::TreePage {
            self.pages.append(kind, generation, bytes, r)
        } else {
            self.records.append(kind, generation, bytes, r)
        }
    }
}
impl<S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>> FinalPacks
    for SplitPacks<'_, '_, S, F>
{
    fn len(&self) -> usize {
        self.pages.len() + self.records.len()
    }
    fn artifact(
        &self,
        index: usize,
    ) -> Result<zeppelin_embed::property_graph::storage::prepared::PreparedArtifact<'_>, TreeError>
    {
        if index < self.pages.len() {
            self.pages.artifact(index)
        } else {
            self.records.artifact(index - self.pages.len())
        }
    }
}
impl<S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>> SplitPacks<'_, '_, S, F> {
    fn finish(&mut self, r: &mut TreeResources<'_>) -> Result<(), TreeError> {
        let allocated =
            self.pages.abort_inventory().count() + self.records.abort_inventory().count();
        assert_eq!(allocated, self.len());
        self.pages.finish(r)?;
        self.records.finish(r)
    }
}
fn lane_packed<'a, 'b, S: BlockSource>(
    source: &'b S,
    generation: u64,
    lane: u64,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> PreparedObjects<'a, 'b, S, impl FnMut() -> Result<ArtifactIdentity, TreeError> + use<S>> {
    let mut serial = generation * 10_000 + lane;
    PreparedObjects::new(
        source,
        move || {
            serial += 2;
            Ok(ArtifactIdentity {
                store: Empty.identity().store,
                artifact: ArtifactId::new(serial as u128).unwrap(),
                generation: GraphGeneration::new(generation),
                creation_serial: serial,
            })
        },
        Empty.identity().store,
        GraphGeneration::new(generation),
        PackLimits {
            artifact_bytes: 512 * 1024,
            blocks: 256,
        },
        memory,
        r,
    )
    .unwrap()
}
fn split_packed<'a, 'b, S: BlockSource>(
    source: &'b S,
    generation: u64,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> SplitPacks<'a, 'b, S, impl FnMut() -> Result<ArtifactIdentity, TreeError> + use<S>> {
    SplitPacks {
        pages: lane_packed(source, generation, 1, memory, r),
        records: lane_packed(source, generation, 2, memory, r),
    }
}
fn reachable_page_packs(
    source: &impl BlockSource,
    roots: GraphRoots,
    shared: &GraphResources,
    r: &mut TreeResources<'_>,
) -> ChargedVec<ArtifactId> {
    use zeppelin_embed::property_graph::storage::tree::{Cell, decode_page};
    fn walk(
        source: &impl BlockSource,
        kind: TreeKind,
        reference: PhysicalRef,
        output: &mut ChargedVec<ArtifactId>,
        r: &mut TreeResources<'_>,
        depth: usize,
    ) {
        assert!(depth < 32);
        r.step(1).unwrap();
        if !output.values.contains(&reference.artifact) {
            assert!(output.values.len() < output.values.capacity());
            output.values.push(reference.artifact);
        }
        let framed = source.resolve(reference, r).unwrap();
        r.step(framed.payload().len() as u64).unwrap();
        let page = decode_page(kind, framed.payload()).unwrap();
        let count = u32::from_le_bytes(framed.payload()[12..16].try_into().unwrap());
        for index in 0..count {
            r.step(1).unwrap();
            if let Cell::Branch { child, .. } = page.cell(index as usize).unwrap() {
                walk(source, kind, child, output, r, depth + 1);
            }
        }
    }
    let mut output = ChargedVec::new(shared, 8192);
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
        TreeKind::ObjectInventory,
    ] {
        if let Some(reference) = roots.directory(kind).unwrap().reference() {
            walk(source, kind, reference, &mut output, r, 0);
        }
    }
    output
}
fn state_after(
    old: CommitState<'static>,
    objects: &impl FinalPacks,
    roots: GraphRoots,
    high: HighWaters,
) -> CommitState<'static> {
    let mut state = old;
    state.generation = roots.generation();
    state.sequence += 1;
    state.high_waters.node = high.node;
    state.high_waters.relationship = high.relationship;
    for index in 0..objects.len() {
        state.high_waters.creation_serial = state
            .high_waters
            .creation_serial
            .max(objects.artifact(index).unwrap().identity().creation_serial);
    }
    for (slot, reference) in roots.references().into_iter().enumerate() {
        state.graph.slots[slot] = reference.map(|block| {
            for index in 0..objects.len() {
                let object = objects.artifact(index).unwrap();
                let identity = object.identity();
                if identity.artifact == block.artifact {
                    return RequiredRef {
                        block,
                        object: ArtifactDescriptor {
                            store: identity.store,
                            artifact: identity.artifact,
                            generation: identity.generation,
                            serial: identity.creation_serial,
                            bytes: object.bytes().len() as u32,
                            family: 17,
                            version: 1,
                            checksum: u64::from_le_bytes(
                                object.bytes()[object.bytes().len() - 8..]
                                    .try_into()
                                    .unwrap(),
                            ),
                        },
                    };
                }
            }
            let previous = old.graph.slots[slot].unwrap();
            assert_eq!(previous.block, block);
            previous
        });
    }
    state
}
fn retained_files(
    objects: &impl FinalPacks,
    reachable: &[ArtifactId],
    directory: &std::path::Path,
    shared: &GraphResources,
    r: &mut TreeResources<'_>,
) -> ChargedVec<ExternalBytes> {
    use std::io::Read;
    let mut output = ChargedVec::new(shared, objects.len());
    for index in 0..objects.len() {
        let object = objects.artifact(index).unwrap();
        let path = directory.join(format!("{}.graph", object.identity().artifact.get()));
        std::fs::write(&path, object.bytes()).unwrap();
        // Keep all emitted physical files; fixture RAM retains only currently
        // reachable page packs, plus every record/adjacency pack. Previously
        // retained snapshots still own all their own page packs recursively.
        if object.identity().artifact.get() % 2 == 1
            && !reachable.contains(&object.identity().artifact)
        {
            continue;
        }
        let length = object.bytes().len();
        let charge = shared.reserve(length).unwrap();
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).unwrap();
        assert_eq!(bytes.capacity(), length);
        let mut file = std::fs::File::open(&path).unwrap();
        let mut buffer = [0; 65_536];
        while bytes.len() < length {
            let count = (length - bytes.len()).min(buffer.len());
            r.step(count as u64).unwrap();
            file.read_exact(&mut buffer[..count]).unwrap();
            r.step(count as u64).unwrap();
            bytes.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(file.read(&mut buffer[..1]).unwrap(), 0);
        output.values.push(ExternalBytes {
            bytes,
            identity: object.identity(),
            _charge: charge,
        });
    }
    output
}

#[inline(never)]
fn verify_fresh_final(
    source: &impl BlockSource,
    roots: GraphRoots,
    state: CommitState<'_>,
    base: &Fixture<'_>,
    total: u128,
    memory: &StorageMemory<'_>,
) {
    // A separate bounded read operation on fresh admitted file bytes,
    // after all private packs were dropped. This allowance is fixture
    // read work, independent of the measured producer allowance.
    let mut verify = TreeResources::for_prepare(memory, 1_000_000_000).unwrap();
    let catalog = Catalog {
        base: base.identity,
        symbols: &base.symbols,
    };
    let reader = NativeGraphReader::new(source, roots, state.sequence, &catalog, None);
    assert_eq!(
        reader.relationship_count(&mut verify).unwrap() as u128,
        total
    );
    let mut scratch = RangeScratch::for_prepare(memory, &mut verify).unwrap();
    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        let root = roots.directory(kind).unwrap();
        let mut cursor = DirectoryCursor::seek(source, root, None, &mut verify).unwrap();
        let mut found = 0u128;
        while let Some(entry) = cursor.next_entry(&mut verify).unwrap() {
            let range = validate_range(
                source,
                root,
                entry,
                state.sequence,
                &mut scratch,
                &mut verify,
            )
            .unwrap();
            for edge in range.edges() {
                found += 1;
                assert_eq!(edge.rel.get(), Empty.high_waters().relationship + found);
                assert_eq!(
                    edge.neighbor.get(),
                    Empty.high_waters().node + if kind == TreeKind::OutRanges { 2 } else { 1 }
                );
            }
        }
        assert_eq!(found, total);
    }
    eprintln!(
        "fresh final file verification edges={total} work={}",
        verify.work()
    );
}

#[allow(clippy::too_many_arguments)]
fn history(
    base: &Fixture<'_>,
    source: &dyn BlockSource,
    roots: GraphRoots,
    state: CommitState<'static>,
    sizes: &[usize],
    work_limit: u64,
    writer: &WriteMemory<'_>,
    memory: &StorageMemory<'_>,
    shared: &GraphResources,
    directory: &std::path::Path,
) {
    let Some((&count, remaining)) = sizes.split_first() else {
        return;
    };
    let generation = base.identity.generation.get() + 1;
    let mut r = TreeResources::for_prepare(memory, work_limit).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    with_local_refs(|refs| {
        let create_nodes = generation == 1;
        let mut view = GraphBatchReadView::new(
            base,
            writer,
            count + if create_nodes { 2 } else { 0 },
            &mut |_| Ok(()),
        )
        .unwrap();
        if create_nodes {
            for index in 0..2 {
                view.create(
                    BatchEntityRef::Node(NodeRef::Local(refs.node(index).unwrap())),
                    WriteImage::Node(&node),
                    &mut |_| Ok(()),
                )
                .unwrap();
            }
        }
        for index in 0..count {
            let endpoint = |index| {
                if create_nodes {
                    NodeRef::Local(refs.node(index).unwrap())
                } else {
                    NodeRef::Existing(NodeId::new((1u128 << 100) + index as u128 + 1).unwrap())
                }
            };
            view.create(
                BatchEntityRef::Relationship(RelRef::Local(refs.relationship(index).unwrap())),
                WriteImage::Relationship {
                    source: endpoint(0),
                    target: endpoint(1),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                },
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        let batch = view.finish(&mut |_| Ok(())).unwrap();
        let forward = Forward(source);
        let mut objects = split_packed(&forward, generation, memory, &mut r);
        let catalog = Catalog {
            base: base.identity,
            symbols: &base.symbols,
        };
        let candidate = prepare_native_graph(
            &mut objects,
            &batch,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base.identity,
                    roots,
                },
                committed: state,
            },
            &catalog,
            None,
            memory,
            &mut r,
        )
        .unwrap();
        let next_roots = candidate.roots();
        let total =
            state.high_waters.relationship - Empty.high_waters().relationship + count as u128;
        assert_eq!(
            batch.high_waters().relationship,
            Empty.high_waters().relationship + total
        );
        let mut scratch = RangeScratch::for_prepare(memory, &mut r).unwrap();
        for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
            let root = next_roots.directory(kind).unwrap();
            let mut cursor = DirectoryCursor::seek(&objects, root, None, &mut r).unwrap();
            let mut actual = 0;
            let mut intervals = 0;
            while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
                let range = validate_range(
                    &objects,
                    root,
                    entry,
                    candidate.sequence(),
                    &mut scratch,
                    &mut r,
                )
                .unwrap();
                assert!(range.descriptor().base_count() <= 4096);
                assert!(range.descriptor().deltas().count() <= 8);
                assert!(range.descriptor().pending_count() <= 2048);
                if remaining.is_empty() && total == 4097 {
                    assert_eq!(
                        range.descriptor().base_count(),
                        if intervals == 0 { 4096 } else { 1 }
                    );
                    assert_eq!(range.descriptor().deltas().count(), 0);
                    let boundary = RelId::new(Empty.high_waters().relationship + 4097).unwrap();
                    if intervals == 0 {
                        assert_eq!(
                            range.descriptor().key().upper,
                            UpperBound::Exclusive(boundary)
                        );
                    } else {
                        assert_eq!(range.descriptor().key().lower, boundary);
                        assert_eq!(range.descriptor().key().upper, UpperBound::Infinity);
                    }
                }
                intervals += 1;
                if remaining.is_empty() && total == 2049 {
                    assert_eq!(range.descriptor().base_count(), 2049);
                    assert_eq!(range.descriptor().deltas().count(), 0);
                    assert_eq!(range.descriptor().watermark(), state.sequence + 1);
                }
                if generation == 9 && sizes.len() == 1 && total == 9 {
                    assert_eq!(range.descriptor().base_count(), 9);
                    assert_eq!(range.descriptor().deltas().count(), 0);
                    assert_eq!(range.descriptor().watermark(), 109);
                }
                for edge in range.edges() {
                    actual += 1;
                    assert_eq!(edge.rel.get(), Empty.high_waters().relationship + actual);
                    assert_eq!(
                        edge.neighbor.get(),
                        Empty.high_waters().node + if kind == TreeKind::OutRanges { 2 } else { 1 }
                    );
                }
            }
            assert_eq!(actual, total, "generation{generation} {kind:?}");
            if remaining.is_empty() && total == 4097 {
                assert_eq!(intervals, 2);
            }
        }
        drop(scratch);
        drop(candidate);
        objects.finish(&mut r).unwrap();
        let next_state = state_after(state, &objects, next_roots, batch.high_waters());
        let producer_work = r.work();
        drop(r);
        let mut r = TreeResources::for_prepare(memory, 200_000_000).unwrap();
        let reachable = reachable_page_packs(&objects, next_roots, shared, &mut r);
        let files = retained_files(&objects, &reachable.values, directory, shared, &mut r);
        drop(reachable);
        eprintln!(
            "history generation={generation} edges={total} count={count} packs={} work={} storage_peak={} shared={}",
            objects.len(),
            producer_work,
            memory.peak_reserved_bytes(),
            shared.reserved_bytes().unwrap()
        );
        drop(objects);
        let mut frames = ChargedVec::new(shared, files.values.len());
        for file in &files.values {
            let frame = decode_with_control(
                ContainerKind::Object,
                Some((file.identity.store, file.identity.artifact)),
                &file.bytes,
                &mut |bytes| r.step(bytes as u64),
            )
            .unwrap();
            assert_eq!(frame.identity(), file.identity);
            frames.values.push(frame);
        }
        let frozen = Frozen {
            previous: source,
            frames: &frames.values,
        };
        // Reserve the fixture's bounded clone before allocation. Its provenance
        // borrows remain backed by the charged live staged batches below.
        // Four descriptor copies cover Vec replacement overlap during the
        // existing fixture builder; reconcile to exact retained capacity after.
        let estimate = (base.entries.len() + batch.deltas().len())
            * std::mem::size_of::<FixtureEntry<'_>>()
            * 4
            + base
                .entries
                .iter()
                .filter_map(|e| e.canonical.as_ref())
                .map(Vec::len)
                .sum::<usize>()
            + batch
                .deltas()
                .iter()
                .filter_map(|d| d.canonical())
                .map(<[u8]>::len)
                .sum::<usize>()
            + (base.symbols.len() + batch.symbols().len())
                * std::mem::size_of::<SymbolEntry<'_>>()
                * 4;
        let mut fixture_charge = shared.reserve(estimate).unwrap();
        let next_base = base.after(&batch);
        let actual = next_base.entries.capacity() * std::mem::size_of::<FixtureEntry<'_>>()
            + next_base
                .entries
                .iter()
                .filter_map(|e| e.canonical.as_ref())
                .map(Vec::capacity)
                .sum::<usize>()
            + next_base.symbols.capacity() * std::mem::size_of::<SymbolEntry<'_>>();
        assert!(actual <= estimate);
        fixture_charge.resize(actual).unwrap();
        drop(r);
        if remaining.is_empty() {
            verify_fresh_final(&frozen, next_roots, next_state, &next_base, total, memory);
        }
        history(
            &next_base, &frozen, next_roots, next_state, remaining, work_limit, writer, memory,
            shared, directory,
        );
        drop(next_base);
        drop(fixture_charge);
    });
    if generation == 1 {
        eprintln!(
            "history full shared_peak={} storage_peak={} writer_retained={}",
            shared.peak_reserved_bytes().unwrap(),
            memory.peak_reserved_bytes(),
            writer.reserved_bytes()
        );
    }
}

#[test]
fn actual_native_producer_consolidates_before_ninth_run() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path().join("store"),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let base = Fixture::empty();
    let roots =
        GraphRoots::from_references(base.identity.store, base.identity.generation, [None; 8])
            .unwrap();
    history(
        &base,
        &Missing,
        roots,
        bootstrap(base.identity, 100),
        &[1; 9],
        200_000_000,
        &writer,
        &memory,
        &shared,
        directory.path(),
    );
}

#[test]
fn actual_native_producer_consolidates_before_2049_pending_entries() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path().join("store"),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let base = Fixture::empty();
    let roots =
        GraphRoots::from_references(base.identity.store, base.identity.generation, [None; 8])
            .unwrap();
    history(
        &base,
        &Missing,
        roots,
        bootstrap(base.identity, 100),
        &[300, 300, 300, 300, 300, 300, 248, 1],
        400_000_000,
        &writer,
        &memory,
        &shared,
        directory.path(),
    );
}
#[test]
fn actual_native_producer_splits_4097_edges_from_bounded_batches() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path().join("store"),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let base = Fixture::empty();
    let roots =
        GraphRoots::from_references(base.identity.store, base.identity.generation, [None; 8])
            .unwrap();
    let mut sizes = [128; 36];
    sizes[31] = 125;
    sizes[32..].fill(1);
    history(
        &base,
        &Missing,
        roots,
        bootstrap(base.identity, 100),
        &sizes,
        200_000_000,
        &writer,
        &memory,
        &shared,
        directory.path(),
    );
}
#[test]
fn property_only_with_detach_preserves_adjacency_then_raw_cleanup_is_paired() {
    use std::cell::Cell;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
    let base = Fixture::empty();
    let roots0 =
        GraphRoots::from_references(base.identity.store, base.identity.generation, [None; 8])
            .unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let properties = [GraphProperty::new(
        GraphName::new("changed").unwrap(),
        PropertyValue::new(PropertyData::I64(7)).unwrap(),
    )];
    with_local_refs(|refs| {
        let mut view = GraphBatchReadView::new(&base, &writer, 5, &mut |_| Ok(())).unwrap();
        for index in 0..2 {
            view.create(
                BatchEntityRef::Node(NodeRef::Local(refs.node(index).unwrap())),
                WriteImage::Node(&node),
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        for (index, target) in [1, 0, 1].into_iter().enumerate() {
            view.create(
                BatchEntityRef::Relationship(RelRef::Local(refs.relationship(index).unwrap())),
                WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(target).unwrap()),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                },
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        let first = view.finish(&mut |_| Ok(())).unwrap();
        let mut p1 = packed(&Missing, 1, &memory, &mut r);
        let c1 = prepare_native_graph(
            &mut p1,
            &first,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base.identity,
                    roots: roots0,
                },
                committed: bootstrap(base.identity, 100),
            },
            &Catalog {
                base: base.identity,
                symbols: &[],
            },
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        let roots1 = c1.roots();
        p1.finish(&mut r).unwrap();
        let state1 = state_after(
            bootstrap(base.identity, 100),
            &p1,
            roots1,
            first.high_waters(),
        );
        let base1 = base.after(&first);
        let a = NodeId::new(Empty.high_waters().node + 1).unwrap();
        let b = NodeId::new(Empty.high_waters().node + 2).unwrap();
        let rels = [1, 2, 3].map(|i| RelId::new(Empty.high_waters().relationship + i).unwrap());
        let mut view = GraphBatchReadView::new(&base1, &writer, 2, &mut |_| Ok(())).unwrap();
        view.replace(
            BatchEntityRef::Relationship(RelRef::Existing(rels[0])),
            WriteImage::Relationship {
                source: NodeRef::Existing(a),
                target: NodeRef::Existing(b),
                relationship_type: GraphName::new("R").unwrap(),
                properties: &properties,
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        view.delete(
            BatchEntityRef::Node(NodeRef::Existing(b)),
            GraphDeleteMode::Detach,
            &mut |_| Ok(()),
        )
        .unwrap();
        let second = view.finish(&mut |_| Ok(())).unwrap();
        assert_eq!(base1.incidents.get(), 0);
        struct Trace<'a, S> {
            source: &'a S,
            adjacency: Cell<usize>,
        }
        impl<S: BlockSource> BlockSource for Trace<'_, S> {
            fn resolve<'a>(
                &'a self,
                reference: PhysicalRef,
                r: &mut TreeResources<'_>,
            ) -> Result<FramedBlock<'a>, TreeError> {
                if matches!(
                    reference.kind,
                    BlockKind::AdjacencyBase | BlockKind::AdjacencyDelta
                ) {
                    self.adjacency.set(self.adjacency.get() + 1);
                }
                self.source.resolve(reference, r)
            }
        }
        let traced = Trace {
            source: &p1,
            adjacency: Cell::new(0),
        };
        let mut p2 = packed(&traced, 2, &memory, &mut r);
        let c2 = prepare_native_graph(
            &mut p2,
            &second,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base1.identity,
                    roots: roots1,
                },
                committed: state1,
            },
            &Catalog {
                base: base1.identity,
                symbols: &base1.symbols,
            },
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        assert_eq!(
            traced.adjacency.get(),
            0,
            "property-only+DETACH reads constant root metadata but never an incident base/delta"
        );
        let roots2 = c2.roots();
        for kind in [
            TreeKind::OutRanges,
            TreeKind::InRanges,
            TreeKind::RelationshipTypes,
        ] {
            assert_eq!(
                roots2.directory(kind).unwrap().reference(),
                roots1.directory(kind).unwrap().reference()
            );
        }
        assert_ne!(
            record_ref(&p2, roots2, EntityId::Relationship(rels[0]), &mut r),
            record_ref(&p1, roots1, EntityId::Relationship(rels[0]), &mut r)
        );
        let base2 = base1.after(&second);
        let catalog2 = Catalog {
            base: base2.identity,
            symbols: &base2.symbols,
        };
        p2.finish(&mut r).unwrap();
        let reader2 = NativeGraphReader::new(&p2, roots2, c2.sequence(), &catalog2, None);
        assert_eq!(reader2.relationship_count(&mut r).unwrap(), 1);
        assert!(reader2.relationship(rels[0], &mut r).unwrap().is_none());
        let state2 = state_after(state1, &p2, roots2, second.high_waters());
        let mut view = GraphBatchReadView::new(&base2, &writer, 3, &mut |_| Ok(())).unwrap();
        for id in [rels[0], rels[1]] {
            view.delete(
                BatchEntityRef::Relationship(RelRef::Existing(id)),
                GraphDeleteMode::Restrict,
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        view.delete(
            BatchEntityRef::Node(NodeRef::Existing(a)),
            GraphDeleteMode::Restrict,
            &mut |_| Ok(()),
        )
        .unwrap();
        let third = view.finish(&mut |_| Ok(())).unwrap();
        assert!(
            base2.incidents.get() > 0,
            "plain DELETE checks admitted live incidents excluding explicit removals"
        );
        let mut p3 = packed(&p2, 3, &memory, &mut r);
        let c3 = prepare_native_graph(
            &mut p3,
            &third,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base2.identity,
                    roots: roots2,
                },
                committed: state2,
            },
            &catalog2,
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        assert_eq!(rows(&p3, c3.roots(), TreeKind::Relationships, &mut r), 1);
        let reader3 = NativeGraphReader::new(&p3, c3.roots(), c3.sequence(), &catalog2, None);
        assert_eq!(reader3.relationship_count(&mut r).unwrap(), 0);
        let mut scratch = RangeScratch::for_prepare(&memory, &mut r).unwrap();
        for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
            let root = c3.roots().directory(kind).unwrap();
            let mut cursor = DirectoryCursor::seek(&p3, root, None, &mut r).unwrap();
            let entry = cursor.next_entry(&mut r).unwrap().unwrap();
            let range =
                validate_range(&p3, root, entry, c3.sequence(), &mut scratch, &mut r).unwrap();
            assert_eq!(
                range.edges(),
                &[Edge {
                    rel: rels[2],
                    neighbor: if kind == TreeKind::OutRanges { b } else { a }
                }]
            );
            assert!(cursor.next_entry(&mut r).unwrap().is_none());
        }
        assert_eq!(
            NativeGraphReader::new(
                &p1,
                roots1,
                c1.sequence(),
                &Catalog {
                    base: base1.identity,
                    symbols: &base1.symbols
                },
                None
            )
            .relationship_count(&mut r)
            .unwrap(),
            3
        );
        assert_eq!(reader2.relationship_count(&mut r).unwrap(), 1);
    });
}

#[test]
fn native_candidate_faults_after_out_before_in_return_no_candidate() {
    #[derive(Clone, Copy)]
    enum Fault {
        None,
        Io,
        Cancel,
        Work,
    }
    struct Interrupted<'a, S> {
        inner: &'a mut S,
        token: &'a CancelToken,
        fault: Fault,
        bases: usize,
    }
    impl<S: BlockSource> BlockSource for Interrupted<'_, S> {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            r: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            self.inner.resolve(reference, r)
        }
    }
    impl<S: BlockSink> BlockSink for Interrupted<'_, S> {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            r: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            if kind == BlockKind::AdjacencyBase {
                self.bases += 1;
                if self.bases == 2 {
                    match self.fault {
                        Fault::None => (),
                        Fault::Io => return Err(TreeError::Io(std::io::ErrorKind::Other.into())),
                        Fault::Cancel => {
                            self.token.cancel();
                            r.step(0)?;
                        }
                        Fault::Work => r.step(100_000_001)?,
                    }
                }
            }
            self.inner.append(kind, generation, bytes, r)
        }
    }
    for fault in [Fault::None, Fault::Io, Fault::Cancel, Fault::Work] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
        let before = memory.reserved_bytes();
        let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        with_local_refs(|refs| {
            let requests = [
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "n").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&node)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "app", "r").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Local(refs.node(0).unwrap()),
                        target: NodeRef::Local(refs.node(0).unwrap()),
                        relationship_type: GraphName::new("R").unwrap(),
                        properties: &[],
                    }),
                },
            ];
            let batch = stage_structured(&Empty, &requests, &writer, &mut |_| Ok(())).unwrap();
            let base = Empty.identity();
            let roots =
                GraphRoots::from_references(base.store, base.generation, [None; 8]).unwrap();
            let mut objects = packed(&Missing, 1, &memory, &mut r);
            let mut interrupted = Interrupted {
                inner: &mut objects,
                token: &token,
                fault,
                bases: 0,
            };
            let result = prepare_native_graph(
                &mut interrupted,
                &batch,
                NativeGraphBase {
                    directories: DirectoryBase {
                        identity: base,
                        roots,
                    },
                    committed: bootstrap(base, 100),
                },
                &Catalog { base, symbols: &[] },
                None,
                &memory,
                &mut r,
            );
            assert_eq!(
                interrupted.bases, 2,
                "identical preparation reaches the real reverse-direction append"
            );
            match fault {
                Fault::None => assert!(result.is_ok()),
                Fault::Io => assert!(matches!(&result, Err(TreeError::Io(_)))),
                Fault::Cancel => assert!(matches!(&result, Err(TreeError::Control(_)))),
                Fault::Work => assert!(matches!(&result, Err(TreeError::Work))),
            }
            drop(result);
            assert!(!objects.is_empty());
            assert_eq!(objects.abort_inventory().count(), objects.len());
            assert!(objects.artifact(0).is_err());
            assert!(
                roots.references().iter().all(Option::is_none),
                "base roots never change when a candidate fails"
            );
            drop(objects);
            assert_eq!(
                memory.reserved_bytes(),
                before,
                "all private pack/scratch/candidate capacity released"
            );
        });
    }
}

#[cfg(feature = "allocation-audit")]
#[test]
fn native_adjacency_participant_attributes_and_refuses_each_real_allocation() {
    use zeppelin_embed::adversarial_test_support::{audit_engine_path, fail_attributed_allocation};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    with_local_refs(|refs| {
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "n").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "r").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(0).unwrap()),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                }),
            },
        ];
        let batch = stage_structured(&Empty, &requests, &writer, &mut |_| Ok(())).unwrap();
        let identity = Empty.identity();
        let roots =
            GraphRoots::from_references(identity.store, identity.generation, [None; 8]).unwrap();
        let base = NativeGraphBase {
            directories: DirectoryBase { identity, roots },
            committed: bootstrap(identity, 100),
        };
        let catalog = Catalog {
            base: identity,
            symbols: &[],
        };
        let before = memory.reserved_bytes();
        let allocations = {
            let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
            let mut objects = packed(&Missing, 1, &memory, &mut r);
            let (candidate, audit) = audit_engine_path(|| {
                prepare_native_graph(&mut objects, &batch, base, &catalog, None, &memory, &mut r)
            });
            let candidate = candidate.unwrap();
            assert_eq!(audit.unattributed_bytes, 0);
            assert!(audit.allocations > 0);
            assert!(
                candidate
                    .roots()
                    .directory(TreeKind::OutRanges)
                    .unwrap()
                    .reference()
                    .is_some()
            );
            assert!(
                candidate
                    .roots()
                    .directory(TreeKind::InRanges)
                    .unwrap()
                    .reference()
                    .is_some()
            );
            audit.allocations
        };
        assert_eq!(memory.reserved_bytes(), before);
        for ordinal in 1..=allocations {
            {
                let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
                let mut objects = packed(&Missing, 1, &memory, &mut r);
                let (result, fires) = fail_attributed_allocation(ordinal, || {
                    prepare_native_graph(
                        &mut objects,
                        &batch,
                        base,
                        &catalog,
                        None,
                        &memory,
                        &mut r,
                    )
                });
                assert_eq!(fires, 1, "actual allocation {ordinal}/{allocations}");
                assert!(
                    matches!(&result, Err(TreeError::Memory)),
                    "refusal {ordinal} yields no candidate"
                );
                drop(result);
                assert_eq!(objects.abort_inventory().count(), objects.len());
                assert!(objects.artifact(0).is_err());
            }
            assert_eq!(
                memory.reserved_bytes(),
                before,
                "refusal {ordinal} releases every private owner"
            );
        }
        let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
        let mut clean = packed(&Missing, 1, &memory, &mut r);
        let candidate =
            prepare_native_graph(&mut clean, &batch, base, &catalog, None, &memory, &mut r)
                .unwrap();
        clean.finish(&mut r).unwrap();
        assert!(
            candidate
                .roots()
                .directory(TreeKind::InRanges)
                .unwrap()
                .reference()
                .is_some()
        );
        println!(
            "ZE44 actual adjacency allocations={allocations} refusal_fires={allocations} unattributed_bytes=0 storage_peak={} shared_peak={}",
            memory.peak_reserved_bytes(),
            shared.peak_reserved_bytes().unwrap()
        );
    });
}
