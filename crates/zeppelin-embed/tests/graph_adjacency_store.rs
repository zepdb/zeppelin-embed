#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use zeppelin_embed::property_graph::storage::adjacency::{
    Direction, RANGE_DESCRIPTOR_BYTES, RangeDescriptor, UpperBound,
};
use zeppelin_embed::property_graph::storage::tree::TreeKind;

fn key_bytes() -> [u8; 40] {
    let mut key = [0; 40];
    key[..16].copy_from_slice(&(1u128 << 100).to_le_bytes());
    key[16..24].copy_from_slice(&(1u64 << 40).to_le_bytes());
    key[24..].copy_from_slice(&(1u128 << 110).to_le_bytes());
    key
}

fn descriptor_bytes() -> [u8; 328] {
    let mut bytes = [0; 328];
    bytes[..2].copy_from_slice(&1u16.to_le_bytes());
    bytes[2] = 1;
    bytes[3] = 1;
    bytes[4] = 2;
    bytes[24..32].copy_from_slice(&100u64.to_le_bytes());
    bytes[32..36].copy_from_slice(&3u32.to_le_bytes());
    bytes[36..40].copy_from_slice(&2u32.to_le_bytes());
    for (offset, artifact, length, kind) in [
        (40, (1u128 << 110) + 1, 184u32, 13u16),
        (72, (1u128 << 110) + 2, 160, 14),
        (104, (1u128 << 110) + 3, 200, 14),
    ] {
        bytes[offset..offset + 16].copy_from_slice(&artifact.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&128u64.to_le_bytes());
        bytes[offset + 24..offset + 28].copy_from_slice(&length.to_le_bytes());
        bytes[offset + 28..offset + 30].copy_from_slice(&kind.to_le_bytes());
        bytes[offset + 30..offset + 32].copy_from_slice(&1u16.to_le_bytes());
    }
    bytes
}

#[test]
fn native_adjacency_descriptor_pins_every_byte_and_rejects_noncanonical_slots() {
    assert_eq!(RANGE_DESCRIPTOR_BYTES, 328);
    let literal = descriptor_bytes();
    let decoded = RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &literal).unwrap();
    assert_eq!(decoded.key().direction, Direction::Out);
    assert_eq!(decoded.key().upper, UpperBound::Infinity);
    assert_eq!(decoded.key().node.get(), 1u128 << 100);
    assert_eq!(decoded.key().rel_type.get(), 1u64 << 40);
    assert_eq!(decoded.key().lower.get(), 1u128 << 110);
    assert_eq!(decoded.watermark(), 100);
    assert_eq!(decoded.base_count(), 2);
    assert_eq!(decoded.pending_count(), 3);
    assert_eq!(decoded.deltas().count(), 2);
    let mut encoded = [0xcc; 328];
    decoded.encode(&mut encoded).unwrap();
    assert_eq!(encoded, literal);
    for (offset, value) in [
        (0, 2),
        (2, 2),
        (3, 2),
        (4, 9),
        (5, 1),
        (6, 1),
        (7, 1),
        (8, 1),
        (136, 1),
        (327, 1),
        (68, 14),
        (100, 13),
        (70, 2),
    ] {
        let mut bad = literal;
        bad[offset] = value;
        assert!(
            RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &bad).is_err(),
            "offset {offset}"
        );
    }
    assert!(RangeDescriptor::decode(TreeKind::InRanges, &key_bytes(), &literal).is_err());
    assert!(RangeDescriptor::decode(TreeKind::Nodes, &key_bytes(), &literal).is_err());
    assert!(RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &literal[..327]).is_err());
    let mut absent_base = literal;
    absent_base[40..72].fill(0);
    assert!(RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &absent_base).is_err());
    let mut empty = literal;
    empty[4] = 0;
    empty[32..40].fill(0);
    empty[72..].fill(0);
    assert!(RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &empty).is_err());
    let mut finite = literal;
    finite[3] = 0;
    finite[8..24].copy_from_slice(&u128::MAX.to_le_bytes());
    assert!(RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &finite).is_ok());
    finite[8..24].copy_from_slice(&(1u128 << 110).to_le_bytes());
    assert!(RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &finite).is_err());
}

#[test]
fn native_adjacency_admits_complete_private_runs_and_original_leaf_generation() {
    use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use zeppelin_embed::property_graph::resources::GraphResources;
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::adjacency::{
        self as a, Action, DeltaEntry, Edge, RangeEditContext, RangeScratch, put_range,
        remove_range, validate_range,
    };
    use zeppelin_embed::property_graph::storage::artifact::{
        ArtifactId, ArtifactIdentity, BlockKind, FramedBlock, PhysicalRef, encode_reference,
    };
    use zeppelin_embed::property_graph::storage::memory::StorageMemory;
    use zeppelin_embed::property_graph::storage::prepared::{PackLimits, PreparedObjects};
    use zeppelin_embed::property_graph::storage::tree::directory::{
        BlockSink, BlockSource, DirectoryRoot, TreeError, TreeResources, TreeScratch, insert,
        lookup_entry,
    };
    use zeppelin_embed::property_graph::{GraphGeneration, NodeId, RelId, StoreInstanceId};
    struct Empty;
    impl BlockSource for Empty {
        fn resolve<'a>(
            &'a self,
            _: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            Err(TreeError::Missing)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let lease = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&lease).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
    let mut scratch = RangeScratch::for_prepare(&memory, &mut r).unwrap();
    let mut tree = TreeScratch::for_prepare(&memory).unwrap();
    let store = StoreInstanceId::new(42).unwrap();
    let generation = GraphGeneration::new(7);
    let mut next = 1u128;
    let mut objects = PreparedObjects::new(
        &Empty,
        || {
            let serial = next;
            next += 1;
            Ok(ArtifactIdentity {
                store,
                generation,
                artifact: ArtifactId::new(serial)?,
                creation_serial: serial as u64,
            })
        },
        store,
        generation,
        PackLimits::default(),
        &memory,
        &mut r,
    )
    .unwrap();
    let mut descriptor = descriptor_bytes();
    descriptor[4] = 1;
    descriptor[32..36].copy_from_slice(&2u32.to_le_bytes());
    descriptor[36..40].copy_from_slice(&1u32.to_le_bytes());
    descriptor[104..].fill(0);
    let key = RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &descriptor)
        .unwrap()
        .key();
    let first = Edge {
        rel: key.lower,
        neighbor: NodeId::new(99).unwrap(),
    };
    let last = Edge {
        rel: RelId::new(u128::MAX).unwrap(),
        neighbor: NodeId::new(100).unwrap(),
    };
    let mut base_bytes = [0; 128];
    a::encode_base(
        key,
        100,
        &[first],
        &mut base_bytes,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let base = objects
        .append(BlockKind::AdjacencyBase, generation, &base_bytes, &mut r)
        .unwrap();
    let mut delta_bytes = [0; 176];
    a::encode_delta(
        key,
        101,
        &[
            DeltaEntry {
                edge: first,
                action: Action::Delete,
            },
            DeltaEntry {
                edge: last,
                action: Action::Insert,
            },
        ],
        &mut delta_bytes,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let delta = objects
        .append(BlockKind::AdjacencyDelta, generation, &delta_bytes, &mut r)
        .unwrap();
    encode_reference(base, &mut descriptor[40..72]).unwrap();
    encode_reference(delta, &mut descriptor[72..104]).unwrap();
    let root = insert(
        &mut objects,
        DirectoryRoot::empty(store, TreeKind::OutRanges, generation),
        &key_bytes(),
        &descriptor,
        generation,
        &mut tree,
        &mut r,
    )
    .unwrap();
    let entry = lookup_entry(&objects, root, &key_bytes(), &mut r)
        .unwrap()
        .unwrap();
    let admitted = validate_range(&objects, root, entry, 101, &mut scratch, &mut r).unwrap();
    assert_eq!(admitted.edges(), &[last]);
    assert_eq!(admitted.descriptor().watermark(), 100);
    assert!(
        validate_range(&objects, root, entry, 100, &mut scratch, &mut r).is_err(),
        "sequence is not generation"
    );
    let retained = DirectoryRoot::from_reference(
        store,
        TreeKind::OutRanges,
        GraphGeneration::new(8),
        root.reference(),
    )
    .unwrap();
    let retained_entry = lookup_entry(&objects, retained, &key_bytes(), &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(
        validate_range(
            &objects,
            retained,
            retained_entry,
            101,
            &mut scratch,
            &mut r
        )
        .unwrap()
        .edges(),
        &[last]
    );
    for (offset, value) in [(32, 1), (36, 2)] {
        let mut bad = descriptor;
        bad[offset] = value;
        let bad_root = insert(
            &mut objects,
            root,
            &key_bytes(),
            &bad,
            generation,
            &mut tree,
            &mut r,
        )
        .unwrap();
        let bad_entry = lookup_entry(&objects, bad_root, &key_bytes(), &mut r)
            .unwrap()
            .unwrap();
        assert!(
            validate_range(&objects, bad_root, bad_entry, 101, &mut scratch, &mut r).is_err(),
            "validated inner count must match descriptor"
        );
    }
    // A framed descriptor leaf claiming an older generation cannot inherit its
    // lifted root's generation to authorize these newer physical descendants.
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut bytes = [0; PAGE_BYTES];
    encode_page(
        PageHeader {
            kind: TreeKind::OutRanges,
            level: 0,
            generation: GraphGeneration::new(6),
        },
        &[Cell::Leaf {
            key: Key::Inline(&key_bytes()),
            value: &descriptor,
        }],
        &mut bytes,
    )
    .unwrap();
    let old_generation = GraphGeneration::new(6);
    let mut old = PreparedObjects::new(
        &objects,
        || {
            Ok(ArtifactIdentity {
                store,
                generation: old_generation,
                artifact: ArtifactId::new(10000)?,
                creation_serial: 10000,
            })
        },
        store,
        old_generation,
        PackLimits::default(),
        &memory,
        &mut r,
    )
    .unwrap();
    let old_reference = old
        .append(BlockKind::TreePage, old_generation, &bytes, &mut r)
        .unwrap();
    let old_root = DirectoryRoot::from_reference(
        store,
        TreeKind::OutRanges,
        GraphGeneration::new(8),
        Some(old_reference),
    )
    .unwrap();
    let old_entry = lookup_entry(&old, old_root, &key_bytes(), &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(old_entry.creation_generation(), old_generation);
    assert!(
        validate_range(&old, old_root, old_entry, 101, &mut scratch, &mut r).is_err(),
        "new root generation must not bless future descendants of an old leaf"
    );
    assert!(scratch.owned_bytes() < 4 * 1024 * 1024);
    assert!(memory.peak_reserved_bytes() < 32 * 1024 * 1024);
    drop(old);
    struct Counted<S> {
        source: S,
        appends: usize,
    }
    impl<S: BlockSource> BlockSource for Counted<S> {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            r: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            self.source.resolve(reference, r)
        }
    }
    impl<S: BlockSink> BlockSink for Counted<S> {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            r: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            self.appends += 1;
            self.source.append(kind, generation, bytes, r)
        }
    }
    let mut objects = Counted {
        source: objects,
        appends: 0,
    };
    let context = RangeEditContext::new(GraphGeneration::new(6), 100).unwrap();
    let original = RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &descriptor).unwrap();
    let mut bad = descriptor;
    bad[36] = 2;
    let bad = RangeDescriptor::decode(TreeKind::OutRanges, &key_bytes(), &bad).unwrap();
    assert!(
        put_range(
            &mut objects,
            root,
            bad,
            context,
            &mut scratch,
            &mut tree,
            &mut r
        )
        .is_err(),
        "new supplied value must be completely validated before insert_checked"
    );
    assert_eq!(objects.appends, 0);
    let replaced = put_range(
        &mut objects,
        root,
        original,
        context,
        &mut scratch,
        &mut tree,
        &mut r,
    )
    .unwrap();
    assert_eq!(objects.appends, 1);
    let mut overlap_key = key;
    overlap_key.lower = RelId::new(key.lower.get() + 1).unwrap();
    a::encode_base(overlap_key, 101, &[last], &mut base_bytes, &mut |_| {
        Ok::<_, ()>(())
    })
    .unwrap();
    let overlap_base = objects
        .append(BlockKind::AdjacencyBase, generation, &base_bytes, &mut r)
        .unwrap();
    let overlap = RangeDescriptor::new(overlap_key, 101, 1, overlap_base, &[], 0).unwrap();
    let before = objects.appends;
    assert!(matches!(
        put_range(
            &mut objects,
            replaced,
            overlap,
            context,
            &mut scratch,
            &mut tree,
            &mut r
        ),
        Err(TreeError::Invalid("adjacency predecessor overlap"))
    ));
    assert_eq!(
        objects.appends, before,
        "overlap is refused before any page append"
    );
    let stale = RangeEditContext::new(generation, 100).unwrap();
    assert!(
        matches!(
            put_range(
                &mut objects,
                root,
                original,
                stale,
                &mut scratch,
                &mut tree,
                &mut r
            ),
            Err(TreeError::Invalid("adjacency inner format"))
        ),
        "copying an old leaf must use its base cutoff, never the target cutoff"
    );
    assert_eq!(objects.appends, before);
    let empty = remove_range(
        &mut objects,
        replaced,
        original,
        context,
        &mut scratch,
        &mut tree,
        &mut r,
    )
    .unwrap();
    assert!(empty.reference().is_none());
    let middle = RelId::new(key.lower.get() + 100).unwrap();
    let left_key = a::RangeKey {
        upper: UpperBound::Exclusive(middle),
        ..key
    };
    let right_key = a::RangeKey {
        lower: middle,
        ..key
    };
    let mut descriptors = Vec::new();
    for (range, edge) in [(left_key, first), (right_key, last)] {
        a::encode_base(range, 101, &[edge], &mut base_bytes, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
        let reference = objects
            .append(BlockKind::AdjacencyBase, generation, &base_bytes, &mut r)
            .unwrap();
        descriptors.push(RangeDescriptor::new(range, 101, 1, reference, &[], 0).unwrap());
    }
    let right = put_range(
        &mut objects,
        empty,
        descriptors[1],
        context,
        &mut scratch,
        &mut tree,
        &mut r,
    )
    .unwrap();
    let split = put_range(
        &mut objects,
        right,
        descriptors[0],
        context,
        &mut scratch,
        &mut tree,
        &mut r,
    )
    .unwrap();
    for (descriptor, expected) in descriptors.iter().zip([first, last]) {
        let entry = lookup_entry(
            &objects,
            split,
            &descriptor.directory_key().unwrap(),
            &mut r,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            validate_range(&objects, split, entry, 101, &mut scratch, &mut r)
                .unwrap()
                .edges(),
            &[expected]
        );
    }
    // This fully valid wide replacement reaches the successor guard rather
    // than failing inner-group validation or a preceding-range comparison.
    a::encode_base(
        key,
        101,
        &[first],
        &mut base_bytes,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let wide_base = objects
        .append(BlockKind::AdjacencyBase, generation, &base_bytes, &mut r)
        .unwrap();
    let wide = RangeDescriptor::new(key, 101, 1, wide_base, &[], 0).unwrap();
    let before = objects.appends;
    assert!(matches!(
        put_range(
            &mut objects,
            split,
            wide,
            context,
            &mut scratch,
            &mut tree,
            &mut r
        ),
        Err(TreeError::Invalid("adjacency successor overlap"))
    ));
    assert_eq!(objects.appends, before);
    let gap = remove_range(
        &mut objects,
        split,
        descriptors[0],
        context,
        &mut scratch,
        &mut tree,
        &mut r,
    )
    .unwrap();
    assert!(
        lookup_entry(&objects, gap, &key_bytes(), &mut r)
            .unwrap()
            .is_none()
    );
    let restored = put_range(
        &mut objects,
        gap,
        descriptors[0],
        context,
        &mut scratch,
        &mut tree,
        &mut r,
    )
    .unwrap();
    assert!(
        lookup_entry(&objects, restored, &key_bytes(), &mut r)
            .unwrap()
            .is_some()
    );
    let mut empty_delta = [0; a::HEADER_BYTES];
    a::encode_delta(left_key, 102, &[], &mut empty_delta, &mut |_| {
        Ok::<_, ()>(())
    })
    .unwrap();
    let empty_delta = objects
        .append(BlockKind::AdjacencyDelta, generation, &empty_delta, &mut r)
        .unwrap();
    let empty_run =
        RangeDescriptor::new(left_key, 101, 1, descriptors[0].base(), &[empty_delta], 1).unwrap();
    let before = objects.appends;
    let next = RangeEditContext::new(generation, 101).unwrap();
    assert!(matches!(
        put_range(
            &mut objects,
            split,
            empty_run,
            next,
            &mut scratch,
            &mut tree,
            &mut r
        ),
        Err(TreeError::Invalid("empty persisted adjacency run"))
    ));
    assert_eq!(objects.appends, before);
    assert!(RangeEditContext::new(generation, u64::MAX).is_err());
    assert!(RangeEditContext::new(GraphGeneration::new(u64::MAX), 101).is_err());
    let mut delete_bytes = [0; a::HEADER_BYTES + 40];
    a::encode_delta(
        key,
        102,
        &[DeltaEntry {
            edge: first,
            action: Action::Delete,
        }],
        &mut delete_bytes,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let deletion = objects
        .append(BlockKind::AdjacencyDelta, generation, &delete_bytes, &mut r)
        .unwrap();
    let no_survivors = RangeDescriptor::new(key, 101, 1, wide_base, &[deletion], 1).unwrap();
    let mut invalid_value = [0; RANGE_DESCRIPTOR_BYTES];
    no_survivors.encode(&mut invalid_value).unwrap();
    let invalid_root = insert(
        &mut objects,
        empty,
        &key_bytes(),
        &invalid_value,
        generation,
        &mut tree,
        &mut r,
    )
    .unwrap();
    let invalid_entry = lookup_entry(&objects, invalid_root, &key_bytes(), &mut r)
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            validate_range(
                &objects,
                invalid_root,
                invalid_entry,
                102,
                &mut scratch,
                &mut r
            ),
            Err(TreeError::Invalid("empty persisted adjacency range"))
        ),
        "an empty merged interval must be removed, never accepted as an invisible persisted range"
    );
}

#[test]
fn outer_descriptor_fuzz_seeds_reach_expected_parser_outcomes() {
    use zeppelin_embed::property_graph::storage::adjacency::RangeDescriptor;
    use zeppelin_embed::property_graph::storage::tree::TreeKind;
    let out = include_bytes!("../../../fuzz/seeds/native_graph_adjacency/descriptor-1-v1");
    let incoming = include_bytes!("../../../fuzz/seeds/native_graph_adjacency/descriptor-2-v1");
    let reserved =
        include_bytes!("../../../fuzz/seeds/native_graph_adjacency/descriptor-reserved-byte");
    for (kind, bytes) in [
        (TreeKind::OutRanges, out.as_slice()),
        (TreeKind::InRanges, incoming.as_slice()),
    ] {
        let value = RangeDescriptor::decode(kind, &bytes[..40], &bytes[40..]).unwrap();
        assert_eq!(value.base_count(), 1);
        assert_eq!(value.watermark(), 100);
        let mut encoded = [0; 328];
        value.encode(&mut encoded).unwrap();
        assert_eq!(encoded, &bytes[40..]);
    }
    assert!(RangeDescriptor::decode(TreeKind::OutRanges, &[], &[]).is_err());
    assert!(
        RangeDescriptor::decode(TreeKind::OutRanges, &reserved[..40], &reserved[40..]).is_err()
    );
    assert!(RangeDescriptor::decode(TreeKind::InRanges, &out[..40], &out[40..]).is_err());
}
