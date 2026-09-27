#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::storage::artifact::{
    self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind, FramedBlock, PhysicalRef,
};
use zeppelin_embed::property_graph::storage::tree::TreeKind;
use zeppelin_embed::property_graph::storage::tree::directory::{
    BlockSink, BlockSource, DirectoryCursor, DirectoryRoot, TreeError, TreeResources, TreeScratch,
    insert, lookup, remove,
};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

fn root_review_two_level(
    objects: &mut Objects,
    r: &mut TreeResources<'_>,
) -> (DirectoryRoot, PhysicalRef) {
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut bytes = vec![0; PAGE_BYTES];
    let mut leaves = Vec::new();
    for id in [10u128, 30, 80, 100] {
        encode_page(
            PageHeader {
                kind: TreeKind::Nodes,
                level: 0,
                generation: GraphGeneration::new(1),
            },
            &[Cell::Leaf {
                key: Key::Inline(&id.to_le_bytes()),
                value: b"old",
            }],
            &mut bytes,
        )
        .unwrap();
        leaves.push(
            objects
                .append(BlockKind::TreePage, GraphGeneration::new(1), &bytes, r)
                .unwrap(),
        );
    }
    let mut branches = Vec::new();
    for (a, b, upper) in [(leaves[0], leaves[1], 20u128), (leaves[2], leaves[3], 90)] {
        encode_page(
            PageHeader {
                kind: TreeKind::Nodes,
                level: 1,
                generation: GraphGeneration::new(2),
            },
            &[
                Cell::Branch {
                    upper: Some(Key::Inline(&upper.to_le_bytes())),
                    child: a,
                },
                Cell::Branch {
                    upper: None,
                    child: b,
                },
            ],
            &mut bytes,
        )
        .unwrap();
        branches.push(
            objects
                .append(BlockKind::TreePage, GraphGeneration::new(2), &bytes, r)
                .unwrap(),
        );
    }
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 2,
            generation: GraphGeneration::new(3),
        },
        &[
            Cell::Branch {
                upper: Some(Key::Inline(&50u128.to_le_bytes())),
                child: branches[0],
            },
            Cell::Branch {
                upper: None,
                child: branches[1],
            },
        ],
        &mut bytes,
    )
    .unwrap();
    let reference = objects
        .append(BlockKind::TreePage, GraphGeneration::new(3), &bytes, r)
        .unwrap();
    (
        DirectoryRoot::from_reference(
            objects.store,
            TreeKind::Nodes,
            GraphGeneration::new(3),
            Some(reference),
        )
        .unwrap(),
        leaves[1],
    )
}

#[test]
fn root_review_predecessor_ascends_between_branch_subtrees_and_releases_work_failures() {
    use zeppelin_embed::property_graph::storage::tree::{Key, directory::lookup_predecessor};
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let baseline = shared.reserved_bytes().unwrap();
    let (root, _) = {
        let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
        root_review_two_level(&mut objects, &mut r)
    };
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
    for (probe, expected) in [
        (1u128, None),
        (10, Some(10u128)),
        (29, Some(10)),
        (30, Some(30)),
        (49, Some(30)),
        (50, Some(30)),
        (79, Some(30)),
        (80, Some(80)),
        (90, Some(80)),
        (99, Some(80)),
        (100, Some(100)),
        (u128::MAX, Some(100)),
    ] {
        let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
        let before = shared.reserved_bytes().unwrap();
        let entry = lookup_predecessor(&objects, root, &probe.to_le_bytes(), &mut r).unwrap();
        assert_eq!(
            shared.reserved_bytes().unwrap(),
            before,
            "cursor capacity releases before returning borrowed entry"
        );
        assert_eq!(entry.is_some(), expected.is_some());
        if let (Some(entry), Some(expected)) = (entry, expected) {
            assert_eq!(entry.key(), Key::Inline(&expected.to_le_bytes()));
            assert_eq!(entry.creation_generation(), GraphGeneration::new(1));
        }
    }
    let mut refusals = 0;
    let mut successes = 0;
    for limit in [0, 1, 10, 100, 1_000, 10_000, 100_000, 1_000_000] {
        let mut r = TreeResources::new(&control, &shared, limit).unwrap();
        let before = shared.reserved_bytes().unwrap();
        match lookup_predecessor(&objects, root, &50u128.to_le_bytes(), &mut r) {
            Err(TreeError::Work) => refusals += 1,
            Ok(Some(_)) => successes += 1,
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(shared.reserved_bytes().unwrap(), before);
    }
    assert!(refusals > 0 && successes > 0);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
}

#[test]
fn root_review_predecessor_rejects_missing_previous_leaf() {
    use zeppelin_embed::property_graph::storage::tree::directory::lookup_predecessor;
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let (root, previous) = root_review_two_level(&mut objects, &mut r);
    objects.objects.remove(&previous.artifact.get()).unwrap();
    assert!(
        lookup_predecessor(&objects, root, &100u128.to_le_bytes(), &mut r)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        lookup_predecessor(&objects, root, &50u128.to_le_bytes(), &mut r),
        Err(TreeError::Missing)
    ));
}

#[test]
fn root_review_predecessor_discards_previous_leaf_when_cancel_fires_during_read() {
    use zeppelin_embed::property_graph::storage::tree::directory::lookup_predecessor;
    struct CancelAtPrevious<'a> {
        objects: &'a Objects,
        previous: PhysicalRef,
        token: CancelToken,
    }
    impl BlockSource for CancelAtPrevious<'_> {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            let block = self.objects.resolve(reference, resources)?;
            if reference == self.previous {
                self.token.cancel();
            }
            Ok(block)
        }
    }
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let (root, previous) = root_review_two_level(&mut objects, &mut r);
    let source = CancelAtPrevious {
        objects: &objects,
        previous,
        token,
    };
    let before = shared.reserved_bytes().unwrap();
    assert!(matches!(
        lookup_predecessor(&source, root, &50u128.to_le_bytes(), &mut r),
        Err(TreeError::Control(_))
    ));
    assert_eq!(shared.reserved_bytes().unwrap(), before);
}

#[test]
fn root_review_predecessor_rejects_malformed_probe_even_on_empty_root() {
    use zeppelin_embed::property_graph::storage::tree::directory::{
        lookup_entry, lookup_predecessor,
    };
    let objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 100_000).unwrap();
    let root = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    assert!(lookup_entry(&objects, root, &[], &mut r).is_err());
    assert!(
        lookup_predecessor(&objects, root, &[], &mut r).is_err(),
        "checked floor lookup must reject the same malformed numeric key on empty and populated trees"
    );
}

struct Objects {
    store: StoreInstanceId,
    next: u128,
    objects: BTreeMap<u128, Vec<u8>>,
    writes: usize,
}
impl Objects {
    fn new() -> Self {
        Self {
            store: StoreInstanceId::new(1u128 << 100).unwrap(),
            next: 1,
            objects: BTreeMap::new(),
            writes: 0,
        }
    }
}
impl BlockSource for Objects {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        resources.step(1)?;
        let bytes = self
            .objects
            .get(&reference.artifact.get())
            .ok_or(TreeError::Missing)?;
        let frame = artifact::decode(
            ContainerKind::Object,
            Some((self.store, reference.artifact)),
            bytes,
        )?;
        Ok(frame.framed_block(reference)?)
    }
}
impl BlockSink for Objects {
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        resources.step(1)?;
        let id = ArtifactId::new(self.next)?;
        let identity = ArtifactIdentity {
            store: self.store,
            artifact: id,
            generation,
            creation_serial: self.next as u64,
        };
        let blocks = [Block {
            kind,
            payload: bytes,
        }];
        let mut output = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks)?];
        artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut output)?;
        let reference = artifact::decode(ContainerKind::Object, Some((self.store, id)), &output)?
            .reference(0)?;
        assert!(self.objects.insert(self.next, output).is_none());
        self.next += 1;
        self.writes += 1;
        Ok(reference)
    }
}

#[test]
fn immutable_directory_preserves_full_ids_and_old_root_values() {
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 100_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let empty = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    let low = 7u128.to_le_bytes();
    let high = ((1u128 << 100) + 7).to_le_bytes();
    let first = insert(
        &mut objects,
        empty,
        &low,
        b"old",
        GraphGeneration::new(1),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    let second = insert(
        &mut objects,
        first,
        &high,
        b"high",
        GraphGeneration::new(2),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    let third = insert(
        &mut objects,
        second,
        &low,
        b"new",
        GraphGeneration::new(3),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    let mut output = [0u8; 32];
    assert_eq!(
        lookup(&objects, first, &low, &mut output, &mut resources).unwrap(),
        Some(3)
    );
    assert_eq!(&output[..3], b"old");
    assert_eq!(
        lookup(&objects, first, &high, &mut output, &mut resources).unwrap(),
        None
    );
    assert_eq!(
        lookup(&objects, third, &low, &mut output, &mut resources).unwrap(),
        Some(3)
    );
    assert_eq!(&output[..3], b"new");
    assert_eq!(
        lookup(&objects, third, &high, &mut output, &mut resources).unwrap(),
        Some(4)
    );
    assert_eq!(&output[..4], b"high");
    assert_eq!(objects.writes, 3);
    assert!(scratch.owned_bytes() > 0);
}

#[test]
fn predecessor_rejects_malformed_probe_even_on_empty_root() {
    use zeppelin_embed::property_graph::storage::tree::directory::{
        lookup_entry, lookup_predecessor,
    };
    let objects = Objects::new();
    let (_directory, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 100_000).unwrap();
    let root = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    assert!(lookup_entry(&objects, root, &[], &mut r).is_err());
    assert!(
        lookup_predecessor(&objects, root, &[], &mut r).is_err(),
        "empty roots must validate the same probe as populated roots"
    );
    assert!(lookup_predecessor(&objects, root, &0u128.to_le_bytes(), &mut r).is_err());
    assert!(
        lookup_predecessor(&objects, root, &1u128.to_le_bytes(), &mut r)
            .unwrap()
            .is_none()
    );
}

#[test]
fn predecessor_seek_crosses_leaves_without_scanning_prior_adjacency_groups() {
    use zeppelin_embed::property_graph::storage::tree::Key;
    use zeppelin_embed::property_graph::storage::tree::directory::lookup_predecessor;
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 500_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut root =
        DirectoryRoot::empty(objects.store, TreeKind::OutRanges, GraphGeneration::new(0));
    let key = |(node, kind, lower): (u128, u64, u128)| {
        let mut bytes = [0; 40];
        bytes[..16].copy_from_slice(&node.to_le_bytes());
        bytes[16..24].copy_from_slice(&kind.to_le_bytes());
        bytes[24..].copy_from_slice(&lower.to_le_bytes());
        bytes
    };
    let mut expected = BTreeMap::new();
    // Opaque large values force leaf boundaries; this source fixture proves
    // numeric routing only, not native adjacency admission or memory quota.
    for index in 0..160u64 {
        let tuple = (
            (1u128 << 100) + u128::from(index / 40) + 1,
            (1u64 << 40) + index / 10 % 4 + 1,
            (1u128 << 110) + u128::from(index % 10) * 2 + 2,
        );
        let mut value = [0; 2048];
        value[..8].copy_from_slice(&index.to_le_bytes());
        root = insert(
            &mut objects,
            root,
            &key(tuple),
            &value,
            GraphGeneration::new(index + 1),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        expected.insert(tuple, index);
    }
    let writes = objects.writes;
    let mut probes = vec![(1, 1, 1), (u128::MAX, u64::MAX, u128::MAX)];
    for &(node, kind, lower) in expected.keys() {
        probes.extend([
            (node, kind, lower - 1),
            (node, kind, lower),
            (node, kind, lower + 1),
        ]);
    }
    for probe in probes {
        let want = expected.range(..=probe).next_back();
        let got = lookup_predecessor(&objects, root, &key(probe), &mut resources).unwrap();
        assert_eq!(got.is_some(), want.is_some());
        if let (Some(got), Some((tuple, index))) = (got, want) {
            assert_eq!(got.key(), Key::Inline(&key(*tuple)));
            assert_eq!(&got.value()[..8], &index.to_le_bytes());
        }
    }
    assert_eq!(objects.writes, writes, "predecessor never prepares a page");
    let token = CancelToken::new();
    let cancelled = QueryControl::Cancel(token.clone());
    let mut cancelled = TreeResources::new(&cancelled, &shared, 100_000).unwrap();
    token.cancel();
    assert!(matches!(
        lookup_predecessor(
            &objects,
            root,
            &key((u128::MAX, u64::MAX, u128::MAX)),
            &mut cancelled
        ),
        Err(TreeError::Control(_))
    ));
}

#[test]
fn predecessor_seek_rejects_future_leaf_on_selected_and_previous_paths() {
    use zeppelin_embed::property_graph::storage::tree::directory::lookup_predecessor;
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let mut bytes = vec![0; PAGE_BYTES];
    let mut leaves = Vec::new();
    for (id, gen_no) in [(1u128, 3u64), (9, 1)] {
        let generation = GraphGeneration::new(gen_no);
        encode_page(
            PageHeader {
                kind: TreeKind::Nodes,
                level: 0,
                generation,
            },
            &[Cell::Leaf {
                key: Key::Inline(&id.to_le_bytes()),
                value: b"valid",
            }],
            &mut bytes,
        )
        .unwrap();
        leaves.push(
            objects
                .append(BlockKind::TreePage, generation, &bytes, &mut r)
                .unwrap(),
        );
    }
    let generation = GraphGeneration::new(2);
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 1,
            generation,
        },
        &[
            Cell::Branch {
                upper: Some(Key::Inline(&8u128.to_le_bytes())),
                child: leaves[0],
            },
            Cell::Branch {
                upper: None,
                child: leaves[1],
            },
        ],
        &mut bytes,
    )
    .unwrap();
    let reference = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut r)
        .unwrap();
    let root = DirectoryRoot::from_reference(
        objects.store,
        TreeKind::Nodes,
        GraphGeneration::new(4),
        Some(reference),
    )
    .unwrap();
    for probe in [1u128, 8] {
        assert!(
            lookup_predecessor(&objects, root, &probe.to_le_bytes(), &mut r).is_err(),
            "selected and cross-leaf paths retain the original parent generation"
        );
    }
    let entry = lookup_predecessor(&objects, root, &9u128.to_le_bytes(), &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(entry.value(), b"valid");
    assert_eq!(entry.creation_generation(), GraphGeneration::new(1));
}

#[test]
fn directory_leaf_splits_preserve_old_roots_and_share_untouched_pages() {
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 100_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut root = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    let mut expected = BTreeMap::new();
    for index in 0..900u128 {
        let id = (1u128 << 96) + ((index * 277) % 900) + 1;
        let value = id.to_be_bytes();
        root = insert(
            &mut objects,
            root,
            &id.to_le_bytes(),
            &value,
            GraphGeneration::new(index as u64 + 1),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        expected.insert(id, value);
    }
    let old_root = root;
    let writes = objects.writes;
    let changed_id = (1u128 << 96) + 31;
    root = insert(
        &mut objects,
        root,
        &changed_id.to_le_bytes(),
        b"replacement",
        GraphGeneration::new(901),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    assert_eq!(
        objects.writes - writes,
        2,
        "one leaf and one ancestor, not directory copy"
    );
    let mut output = [0u8; 32];
    for (&id, value) in &expected {
        let length = lookup(
            &objects,
            old_root,
            &id.to_le_bytes(),
            &mut output,
            &mut resources,
        )
        .unwrap()
        .unwrap();
        assert_eq!(&output[..length], value);
        let length = lookup(
            &objects,
            root,
            &id.to_le_bytes(),
            &mut output,
            &mut resources,
        )
        .unwrap()
        .unwrap();
        if id == changed_id {
            assert_eq!(&output[..length], b"replacement");
        } else {
            assert_eq!(&output[..length], value);
        }
    }
}

#[test]
fn multilevel_directory_deletion_restores_last_bounds_and_collapses_root() {
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 2_000_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut root =
        DirectoryRoot::empty(objects.store, TreeKind::KeyFences, GraphGeneration::new(0));
    let key = |index: u32| {
        let mut bytes = vec![1];
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(format!("{index:08}").as_bytes());
        bytes.resize(409, b'x');
        bytes
    };
    for index in 0..850u32 {
        // Each immutable edit has its own bounded operation budget.
        let mut resources = TreeResources::new(&control, &shared, 10_000_000).unwrap();
        root = insert(
            &mut objects,
            root,
            &key(index),
            &index.to_le_bytes(),
            GraphGeneration::new(index as u64 + 1),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
    }
    let old = root;
    let block = objects
        .resolve(root.reference().unwrap(), &mut resources)
        .unwrap();
    let page = zeppelin_embed::property_graph::storage::tree::decode_page(
        TreeKind::KeyFences,
        block.payload(),
    )
    .unwrap();
    assert!(
        page.header().level >= 2,
        "must actually split an internal page"
    );
    for index in (0..850u32).rev() {
        let mut resources = TreeResources::new(&control, &shared, 10_000_000).unwrap();
        root = remove(
            &mut objects,
            root,
            &key(index),
            GraphGeneration::new(1700 - index as u64),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        let mut output = [0; 4];
        assert_eq!(
            lookup(&objects, root, &key(index), &mut output, &mut resources).unwrap(),
            None
        );
        assert_eq!(
            lookup(&objects, old, &key(index), &mut output, &mut resources).unwrap(),
            Some(4)
        );
        assert_eq!(u32::from_le_bytes(output), index);
        if index % 37 == 0 && index > 0 {
            let inserted = insert(
                &mut objects,
                root,
                &key(900),
                b"late",
                root.generation(),
                &mut scratch,
                &mut resources,
            )
            .unwrap();
            assert_eq!(
                lookup(&objects, inserted, &key(900), &mut output, &mut resources).unwrap(),
                Some(4)
            );
            assert_eq!(output, *b"late");
        }
    }
    assert_eq!(root.reference(), None);
    let before = objects.writes;
    let same = remove(
        &mut objects,
        root,
        &key(1),
        GraphGeneration::new(2000),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    assert_eq!(same, root);
    assert_eq!(objects.writes, before);
}

#[test]
fn directory_cursor_matches_ordered_map_after_middle_first_and_last_deletes() {
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 150_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut root = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    let mut expected = BTreeMap::new();
    for index in 0..750u128 {
        let id = ((index * 277) % 750) + (1u128 << 80) + 1;
        let value = id.to_le_bytes();
        root = insert(
            &mut objects,
            root,
            &id.to_le_bytes(),
            &value,
            GraphGeneration::new(index as u64 + 1),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        expected.insert(id, value);
    }
    for index in 0..750u128 {
        if index % 3 != 1 {
            let id = index + (1u128 << 80) + 1;
            root = remove(
                &mut objects,
                root,
                &id.to_le_bytes(),
                GraphGeneration::new(751 + index as u64),
                &mut scratch,
                &mut resources,
            )
            .unwrap();
            expected.remove(&id);
        }
    }
    for lower in [None, Some((1u128 << 80) + 201), Some((1u128 << 80) + 751)] {
        let bytes = lower.map(u128::to_le_bytes);
        let mut cursor = DirectoryCursor::seek(
            &objects,
            root,
            bytes.as_ref().map(|b| b.as_slice()),
            &mut resources,
        )
        .unwrap();
        let mut seen = Vec::new();
        let (mut key, mut value) = ([0; 16], [0; 16]);
        while let Some((key_len, value_len)) =
            cursor.next(&mut key, &mut value, &mut resources).unwrap()
        {
            assert_eq!((key_len, value_len), (16, 16));
            seen.push((u128::from_le_bytes(key), value));
        }
        let wanted: Vec<_> = expected
            .iter()
            .filter(|(key, _)| lower.is_none_or(|lo| **key >= lo))
            .map(|(key, value)| (*key, *value))
            .collect();
        assert_eq!(seen, wanted);
        assert_eq!(
            cursor.next(&mut key, &mut value, &mut resources).unwrap(),
            None
        );
    }
}

#[test]
fn lossless_extent_payloads_read_across_chunks_and_preserve_relocated_bytes() {
    use zeppelin_embed::property_graph::storage::payload::{PayloadRef, prepare_payload};
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 30_000_000).unwrap();
    let mut original = vec![b'x'; 150_001];
    original[65_535..65_539].copy_from_slice("🦊".as_bytes());
    let store = objects.store;
    let first = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::CanonicalImage,
        &original,
        &mut resources,
    )
    .unwrap();
    let relocated = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(2),
        BlockKind::CanonicalImage,
        &original,
        &mut resources,
    )
    .unwrap();
    assert_ne!(first.reference(), relocated.reference());
    assert_eq!(first.reference().kind, BlockKind::ExtentList);
    assert_eq!(first.len(), original.len() as u64);
    first
        .validate_all(&objects, store, GraphGeneration::new(2), &mut resources)
        .unwrap();
    let mut buffer = [0u8; 79];
    for offset in [0, 65_520, 65_535, 65_536, 131_060, 150_000, 150_001] {
        let got = first
            .read_at(
                &objects,
                store,
                GraphGeneration::new(2),
                offset,
                &mut buffer,
                &mut resources,
            )
            .unwrap();
        assert_eq!(
            &buffer[..got],
            &original[offset as usize..offset as usize + got]
        );
        let mut other = [0; 79];
        let count = relocated
            .read_at(
                &objects,
                store,
                GraphGeneration::new(2),
                offset,
                &mut other,
                &mut resources,
            )
            .unwrap();
        assert_eq!((&buffer[..got], got), (&other[..count], count));
    }
    assert!(
        PayloadRef::new(
            BlockKind::CanonicalImage,
            original.len() as u64,
            first.reference()
        )
        .is_ok()
    );
    let small = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(2),
        BlockKind::StoredText,
        b"",
        &mut resources,
    )
    .unwrap();
    assert_eq!(small.reference().kind, BlockKind::StoredText);
    small
        .validate_all(&objects, store, GraphGeneration::new(2), &mut resources)
        .unwrap();
}

#[test]
fn overflow_keys_compare_full_eight_mib_and_cursor_preserves_exact_bytes() {
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 2_000_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut key = vec![b'a'; 8 * 1024 * 1024];
    key[0] = 1;
    key[1..9].copy_from_slice(&7u64.to_le_bytes());
    key[65535..65539].copy_from_slice("🦊".as_bytes());
    let mut later = key.clone();
    *later.last_mut().unwrap() = b'b';
    let empty = DirectoryRoot::empty(objects.store, TreeKind::KeyFences, GraphGeneration::new(0));
    let first = insert(
        &mut objects,
        empty,
        &key,
        b"first",
        GraphGeneration::new(1),
        &mut scratch,
        &mut resources,
    )
    .expect("the full admitted key cap must fit through overflow");
    let second = insert(
        &mut objects,
        first,
        &later,
        b"later",
        GraphGeneration::new(2),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    let mut value = [0; 8];
    assert_eq!(
        lookup(&objects, second, &key, &mut value, &mut resources).unwrap(),
        Some(5)
    );
    assert_eq!(&value[..5], b"first");
    assert_eq!(
        lookup(&objects, first, &later, &mut value, &mut resources).unwrap(),
        None
    );
    let mut output = vec![0; key.len()];
    let mut cursor = DirectoryCursor::seek(&objects, second, Some(&key), &mut resources).unwrap();
    assert_eq!(
        cursor
            .next(&mut output, &mut value, &mut resources)
            .unwrap(),
        Some((key.len(), 5))
    );
    assert_eq!(output, key);
    assert_eq!(
        cursor
            .next(&mut output, &mut value, &mut resources)
            .unwrap(),
        Some((later.len(), 5))
    );
    assert_eq!(output, later);
    assert_eq!(
        cursor
            .next(&mut output, &mut value, &mut resources)
            .unwrap(),
        None
    );
    assert!(
        objects
            .objects
            .values()
            .all(|object| object.len() <= artifact::MAX_ARTIFACT_BYTES)
    );
}

#[test]
fn reopened_near_page_inline_key_normalizes_without_changing_logical_bytes() {
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 30_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut key = vec![b'x'; 16_292];
    key[0] = 1;
    key[1..9].copy_from_slice(&1u64.to_le_bytes());
    let mut page = vec![0; PAGE_BYTES];
    encode_page(
        PageHeader {
            kind: TreeKind::KeyFences,
            level: 0,
            generation: GraphGeneration::new(1),
        },
        &[Cell::Leaf {
            key: Key::Inline(&key),
            value: &[],
        }],
        &mut page,
    )
    .unwrap();
    let reference = objects
        .append(
            BlockKind::TreePage,
            GraphGeneration::new(1),
            &page,
            &mut resources,
        )
        .unwrap();
    let root = DirectoryRoot::from_reference(
        objects.store,
        TreeKind::KeyFences,
        GraphGeneration::new(1),
        Some(reference),
    )
    .unwrap();
    let mut neighbor = key.clone();
    *neighbor.last_mut().unwrap() = b'y';
    let updated = insert(
        &mut objects,
        root,
        &neighbor,
        b"new",
        GraphGeneration::new(2),
        &mut scratch,
        &mut resources,
    )
    .unwrap();
    let mut value = [0; 4];
    assert_eq!(
        lookup(&objects, root, &key, &mut value, &mut resources).unwrap(),
        Some(0)
    );
    assert_eq!(
        lookup(&objects, updated, &key, &mut value, &mut resources).unwrap(),
        Some(0)
    );
    assert_eq!(
        lookup(&objects, updated, &neighbor, &mut value, &mut resources).unwrap(),
        Some(3)
    );
    let frame = objects
        .resolve(updated.reference().unwrap(), &mut resources)
        .unwrap();
    let page = zeppelin_embed::property_graph::storage::tree::decode_page(
        TreeKind::KeyFences,
        frame.payload(),
    )
    .unwrap();
    assert!(matches!(
        page.cell(0).unwrap(),
        Cell::Leaf {
            key: Key::Overflow { .. },
            ..
        }
    ));
    assert!(matches!(
        page.cell(1).unwrap(),
        Cell::Leaf {
            key: Key::Overflow { .. },
            ..
        }
    ));
}

#[test]
fn cancellation_after_final_sink_resolution_returns_no_prepared_root() {
    struct Sink {
        objects: Objects,
        cancel: CancelToken,
        armed: bool,
    }
    impl BlockSource for Sink {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            let result = self.objects.resolve(reference, resources)?;
            if self.armed {
                self.cancel.cancel();
            }
            Ok(result)
        }
    }
    impl BlockSink for Sink {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            resources: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            self.objects.append(kind, generation, bytes, resources)
        }
    }
    let cancel = CancelToken::new();
    let control = QueryControl::Cancel(cancel.clone());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 1_000_000).unwrap();
    let mut sink = Sink {
        objects: Objects::new(),
        cancel,
        armed: true,
    };
    let root = DirectoryRoot::empty(sink.objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let result = insert(
        &mut sink,
        root,
        &1u128.to_le_bytes(),
        b"x",
        GraphGeneration::new(1),
        &mut scratch,
        &mut resources,
    );
    assert!(
        matches!(result, Err(TreeError::Control(_))),
        "cancelled preparation escaped: {result:?}"
    );
    assert_eq!(root.reference(), None);
    assert_eq!(
        sink.objects.writes, 1,
        "private abort inventory remains explicit"
    );
    sink.armed = false;
    let clean = QueryControl::Cancel(CancelToken::new());
    let mut resources = TreeResources::new(&clean, &shared, 1_000_000).unwrap();
    assert!(
        insert(
            &mut sink,
            root,
            &1u128.to_le_bytes(),
            b"x",
            GraphGeneration::new(1),
            &mut scratch,
            &mut resources
        )
        .is_ok()
    );
}

fn memory_fixture() -> (
    tempfile::TempDir,
    zeppelin_embed::lifecycle::Store,
    zeppelin_embed::property_graph::resources::GraphResources,
) {
    use zeppelin_embed::lifecycle::{OpenOptions, Store};
    use zeppelin_embed::property_graph::resources::GraphResources;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    (dir, store, resources)
}

#[test]
fn tree_scratch_charges_real_shared_capacity_and_releases_before_other_work() {
    let (_dir, store, shared) = memory_fixture();
    let baseline = shared.reserved_bytes().unwrap();
    let scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    assert_eq!(
        shared.reserved_bytes().unwrap(),
        baseline + scratch.owned_bytes() as u64,
        "tree scratch must own a real reservation, not trust a caller-provided number"
    );
    let other = shared
        .reserve(256 * 1024 * 1024 - baseline as usize - scratch.owned_bytes())
        .unwrap();
    assert!(TreeScratch::new(&shared, 1024 * 1024).is_err());
    drop(other);
    drop(scratch);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
    store.close().unwrap();
}

#[test]
fn read_resources_charge_their_shared_fixed_working_set() {
    let (_dir, _store, shared) = memory_fixture();
    let baseline = shared.reserved_bytes().unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let resources = TreeResources::new(&control, &shared, 1_000_000).unwrap();
    assert!(
        shared.reserved_bytes().unwrap() > baseline,
        "lookup/payload operations must reserve their own bounded workspace even without TreeScratch"
    );
    drop(resources);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
}

#[test]
fn whole_directory_verifier_rejects_corruption_on_an_unvisited_point_path() {
    use zeppelin_embed::property_graph::storage::tree::directory::verify_directory;
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut resources = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let generation = GraphGeneration::new(1);
    let mut bytes = vec![0; PAGE_BYTES];
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 0,
            generation,
        },
        &[Cell::Leaf {
            key: Key::Inline(&1u128.to_le_bytes()),
            value: b"left",
        }],
        &mut bytes,
    )
    .unwrap();
    let left = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut resources)
        .unwrap();
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 0,
            generation,
        },
        &[Cell::Leaf {
            key: Key::Inline(&2u128.to_le_bytes()),
            value: b"wrong range",
        }],
        &mut bytes,
    )
    .unwrap();
    let right = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut resources)
        .unwrap();
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 1,
            generation,
        },
        &[
            Cell::Branch {
                upper: Some(Key::Inline(&8u128.to_le_bytes())),
                child: left,
            },
            Cell::Branch {
                upper: None,
                child: right,
            },
        ],
        &mut bytes,
    )
    .unwrap();
    let reference = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut resources)
        .unwrap();
    let root =
        DirectoryRoot::from_reference(objects.store, TreeKind::Nodes, generation, Some(reference))
            .unwrap();
    let mut output = [0; 16];
    assert_eq!(
        lookup(
            &objects,
            root,
            &1u128.to_le_bytes(),
            &mut output,
            &mut resources
        )
        .unwrap(),
        Some(4)
    );
    let mut verified = 0;
    assert!(
        verify_directory(&objects, root, &mut resources, &mut |entry, _| {
            assert_eq!(entry.value(), b"left");
            verified += 1;
            Ok(())
        })
        .is_err(),
        "checking only the routed leaf is not whole-directory proof"
    );
    assert_eq!(verified, 1);
    let mut cursor = DirectoryCursor::seek(&objects, root, None, &mut resources).unwrap();
    assert!(cursor.next_entry(&mut resources).unwrap().is_some());
    assert!(cursor.next_entry(&mut resources).is_err());
    assert!(matches!(
        cursor.next_entry(&mut resources),
        Err(TreeError::Invalid("cursor previously failed"))
    ));
}

#[test]
fn root_collapse_rejects_wrong_level_in_the_surviving_sibling() {
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut resources = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let generation = GraphGeneration::new(1);
    let mut bytes = vec![0; PAGE_BYTES];
    let mut leaves = Vec::new();
    for id in [1u128, 9] {
        encode_page(
            PageHeader {
                kind: TreeKind::Nodes,
                level: 0,
                generation,
            },
            &[Cell::Leaf {
                key: Key::Inline(&id.to_le_bytes()),
                value: b"live",
            }],
            &mut bytes,
        )
        .unwrap();
        leaves.push(
            objects
                .append(BlockKind::TreePage, generation, &bytes, &mut resources)
                .unwrap(),
        );
    }
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 1,
            generation,
        },
        &[Cell::Branch {
            upper: None,
            child: leaves[1],
        }],
        &mut bytes,
    )
    .unwrap();
    let bad = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut resources)
        .unwrap();
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 1,
            generation,
        },
        &[
            Cell::Branch {
                upper: Some(Key::Inline(&8u128.to_le_bytes())),
                child: leaves[0],
            },
            Cell::Branch {
                upper: None,
                child: bad,
            },
        ],
        &mut bytes,
    )
    .unwrap();
    let reference = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut resources)
        .unwrap();
    let root =
        DirectoryRoot::from_reference(objects.store, TreeKind::Nodes, generation, Some(reference))
            .unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let result = remove(
        &mut objects,
        root,
        &1u128.to_le_bytes(),
        GraphGeneration::new(2),
        &mut scratch,
        &mut resources,
    );
    assert!(
        matches!(result, Err(TreeError::Invalid(_))),
        "collapse silently repaired malformed topology: {result:?}"
    );
}

#[test]
fn derived_record_stream_spans_objects_without_relaxing_canonical_input_cap() {
    use zeppelin_embed::property_graph::storage::payload::{PayloadRef, prepare_stream};
    let mut objects = Objects::new();
    let store = objects.store;
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut resources = TreeResources::new(&control, &shared, 100_000_000).unwrap();
    let length: usize = 9 * 1024 * 1024 + 7;
    let mut calls = 0;
    let payload = prepare_stream(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::NodeRecord,
        length,
        &mut |offset, bytes, _| {
            calls += 1;
            assert!(bytes.len() <= 65536);
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = ((offset as usize + i) % 251) as u8;
            }
            Ok(())
        },
        &mut resources,
    )
    .unwrap();
    assert_eq!(calls, length.div_ceil(65536));
    payload
        .validate_all(&objects, store, GraphGeneration::new(1), &mut resources)
        .unwrap();
    let mut bytes = [0; 32];
    assert_eq!(
        payload
            .read_at(
                &objects,
                store,
                GraphGeneration::new(1),
                length as u64 - 32,
                &mut bytes,
                &mut resources
            )
            .unwrap(),
        32
    );
    for (i, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, ((length - 32 + i) % 251) as u8);
    }
    assert!(
        PayloadRef::new(
            BlockKind::CanonicalImage,
            length as u64,
            payload.reference()
        )
        .is_err()
    );
    let mut encoded = [0; 48];
    payload.encode_into(&mut encoded).unwrap();
    assert_eq!(PayloadRef::decode(&encoded).unwrap(), payload);
    assert_eq!(&encoded[..8], &[2, 0, 1, 0, 0, 0, 0, 0]);
    assert_eq!(
        u64::from_le_bytes(encoded[8..16].try_into().unwrap()),
        length as u64
    );
    assert!(
        objects
            .objects
            .values()
            .all(|object| object.len() <= artifact::MAX_ARTIFACT_BYTES)
    );
}

#[test]
fn graph_root_bundle_retains_typed_full_width_label_and_type_directories() {
    use zeppelin_embed::property_graph::storage::tree::directory::GraphRoots;
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut resources = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let original =
        GraphRoots::from_references(objects.store, GraphGeneration::new(0), [None; 8]).unwrap();
    let mut updated = original.for_generation(GraphGeneration::new(1)).unwrap();
    for kind in [TreeKind::Labels, TreeKind::RelationshipTypes] {
        let mut root = updated.directory(kind).unwrap();
        for (symbol, id) in [(256u64, 1u128), (1, (1u128 << 100) + 1), (1, 1)] {
            let mut key = Vec::new();
            key.extend(symbol.to_le_bytes());
            key.extend(id.to_le_bytes());
            root = insert(
                &mut objects,
                root,
                &key,
                &[],
                GraphGeneration::new(1),
                &mut scratch,
                &mut resources,
            )
            .unwrap();
        }
        updated.replace(root).unwrap();
        let mut cursor = DirectoryCursor::seek(
            &objects,
            updated.directory(kind).unwrap(),
            None,
            &mut resources,
        )
        .unwrap();
        let mut observed = Vec::new();
        while let Some(entry) = cursor.next_entry(&mut resources).unwrap() {
            let zeppelin_embed::property_graph::storage::tree::Key::Inline(key) = entry.key()
            else {
                panic!("numeric key overflow")
            };
            assert!(entry.value().is_empty());
            observed.push((
                u64::from_le_bytes(key[..8].try_into().unwrap()),
                u128::from_le_bytes(key[8..].try_into().unwrap()),
            ));
        }
        assert_eq!(observed, vec![(1, 1), (1, (1u128 << 100) + 1), (256, 1)]);
        assert_eq!(original.directory(kind).unwrap().reference(), None);
    }
    assert!(
        updated
            .replace(DirectoryRoot::empty(
                StoreInstanceId::new(2).unwrap(),
                TreeKind::Nodes,
                GraphGeneration::new(1)
            ))
            .is_err()
    );
    assert!(
        updated
            .replace(DirectoryRoot::empty(
                objects.store,
                TreeKind::Nodes,
                GraphGeneration::new(0)
            ))
            .is_err()
    );
    assert!(updated.for_generation(GraphGeneration::new(0)).is_err());
}

#[test]
fn retained_payload_slices_bound_cross_chunk_utf8_and_cursor_fields() {
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::stream::{PayloadCursor, PayloadSlice};
    let mut objects = Objects::new();
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut resources = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let generation = GraphGeneration::new(1);
    let text = format!("{}🦀\0tail", "a".repeat(65_525));
    let mut bytes = (text.len() as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(&((1u128 << 120) + 7).to_le_bytes());
    let store = objects.store;
    let payload = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::CanonicalImage,
        &bytes,
        &mut resources,
    )
    .unwrap();
    let whole = PayloadSlice::new(&objects, store, generation, payload);
    let mut cursor = PayloadCursor::new(whole);
    let stored_text = cursor.blob(&mut resources).unwrap();
    stored_text.validate_utf8(&mut resources).unwrap();
    assert_eq!(
        stored_text
            .compare_bytes(text.as_bytes(), &mut resources)
            .unwrap(),
        std::cmp::Ordering::Equal
    );
    assert_eq!(
        u128::from_le_bytes(cursor.read_array::<16>(&mut resources).unwrap()),
        (1u128 << 120) + 7
    );
    cursor.finish(&mut resources).unwrap();
    let mut tail = [0; 8];
    assert_eq!(
        stored_text
            .read_at(stored_text.len() - 5, &mut tail, &mut resources)
            .unwrap(),
        5
    );
    assert_eq!(&tail[..5], b"\0tail");
    assert!(stored_text.subslice(stored_text.len(), 1).is_err());
    assert!(stored_text.subslice(u64::MAX, 2).is_err());
    assert!(
        stored_text
            .subslice(65_525, 3)
            .unwrap()
            .validate_utf8(&mut resources)
            .is_err()
    );
    assert!(
        stored_text
            .subslice(65_526, 3)
            .unwrap()
            .validate_utf8(&mut resources)
            .is_err()
    );
    assert_eq!(
        stored_text
            .read_at(stored_text.len(), &mut tail, &mut resources)
            .unwrap(),
        0
    );
    assert!(
        stored_text
            .read_at(stored_text.len() + 1, &mut tail, &mut resources)
            .is_err()
    );
    let mut truncated = PayloadCursor::new(whole.subslice(0, whole.len() - 1).unwrap());
    truncated.blob(&mut resources).unwrap();
    assert!(truncated.read_array::<16>(&mut resources).is_err());
    let mut trailing = PayloadCursor::new(whole);
    trailing.blob(&mut resources).unwrap();
    assert!(trailing.finish(&mut resources).is_err());
}

#[test]
fn canonical_storage_walk_preserves_typed_empty_and_scalar_bits() {
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::{
        CanonicalVisitor, StoredProperty, verify_canonical,
    };
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::{
        CanonicalContents, GraphName, GraphProperty, PropertyData, PropertyValue,
    };
    struct Observe {
        labels: Vec<Vec<u8>>,
        properties: Vec<(Vec<u8>, u64, Vec<u8>)>,
    }
    impl CanonicalVisitor<Objects> for Observe {
        fn label(
            &mut self,
            name: PayloadSlice<'_, Objects>,
            r: &mut TreeResources<'_>,
        ) -> Result<(), TreeError> {
            let mut v = vec![0; name.len() as usize];
            name.read_at(0, &mut v, r)?;
            self.labels.push(v);
            Ok(())
        }
        fn property(
            &mut self,
            name: PayloadSlice<'_, Objects>,
            value: StoredProperty<'_, Objects>,
            r: &mut TreeResources<'_>,
        ) -> Result<(), TreeError> {
            let mut n = vec![0; name.len() as usize];
            name.read_at(0, &mut n, r)?;
            let mut v = vec![0; value.encoded().len() as usize];
            value.encoded().read_at(0, &mut v, r)?;
            self.properties.push((n, value.offset(), v));
            Ok(())
        }
    }
    let mut labels = [GraphName::new("z").unwrap(), GraphName::new("\0").unwrap()];
    let values = [
        PropertyData::EmptyList { count: 0 },
        PropertyData::Integers(&[]),
        PropertyData::F64(f64::from_bits(0x7ff8_1234_5678_9012)),
        PropertyData::I64(i64::MIN),
    ];
    let mut properties: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .zip(values)
        .map(|(n, v)| {
            GraphProperty::new(GraphName::new(n).unwrap(), PropertyValue::new(v).unwrap())
        })
        .collect();
    let canonical = CanonicalContents::node(&mut labels, &mut properties, Some(""), None).unwrap();
    let mut bytes = Vec::new();
    canonical.write_to(&mut bytes, &mut || Ok(())).unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(1);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let p = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::CanonicalImage,
        &bytes,
        &mut r,
    )
    .unwrap();
    let mut observe = Observe {
        labels: Vec::new(),
        properties: Vec::new(),
    };
    let view = verify_canonical(
        PayloadSlice::new(&objects, store, generation, p),
        None,
        &mut observe,
        &mut r,
    )
    .unwrap();
    assert_eq!(observe.labels, vec![vec![0], b"z".to_vec()]);
    assert_eq!(view.property_count(), 4);
    assert_eq!(view.stored_text().unwrap().len(), 0);
    assert!(view.stored_vector().is_none());
    assert_eq!(
        observe.properties[0].2,
        [vec![5], 0u64.to_le_bytes().to_vec()].concat()
    );
    assert_eq!(
        observe.properties[1].2,
        [vec![8], 0u64.to_le_bytes().to_vec()].concat()
    );
    assert_eq!(
        &observe.properties[2].2[1..],
        &0x7ff8_1234_5678_9012u64.to_le_bytes()
    );
    assert_eq!(&observe.properties[3].2[1..], &i64::MIN.to_le_bytes());
    // Reframe each corruption with valid outer checksums; logical decoding must fail.
    for (offset, value) in [
        (observe.properties[0].1 as usize + 1, 1),
        (observe.properties[1].1 as usize, 99),
        (5, 1),
    ] {
        let mut broken = bytes.clone();
        broken[offset] = value;
        let p = prepare_payload(
            &mut objects,
            store,
            generation,
            BlockKind::CanonicalImage,
            &broken,
            &mut r,
        )
        .unwrap();
        let mut observe = Observe {
            labels: Vec::new(),
            properties: Vec::new(),
        };
        assert!(
            verify_canonical(
                PayloadSlice::new(&objects, store, generation, p),
                None,
                &mut observe,
                &mut r
            )
            .is_err()
        );
    }
}

#[test]
fn provenance_storage_walk_retains_complete_fields_and_split_utf8_keys() {
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::verify_provenance;
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphDeleteMode, GraphOperation,
        GraphRevision, NodeId, OperationFields, OperationProvenance,
    };
    let namespace = format!("{}🦀\0", "n".repeat(65_517));
    let fields = OperationFields {
        operation: GraphOperation::StructuredDelete,
        key: Some(ApplicationKey::new(EntityKind::Node, &namespace, "").unwrap()),
        requested_revision: GraphRevision::new(9).unwrap(),
        installed_revision: GraphRevision::new(9).unwrap(),
        expected: ExpectedGraphState::Entity(EntityId::Node(
            NodeId::new((1u128 << 120) + 8).unwrap(),
        )),
        incarnation: EntityId::Node(NodeId::new((1u128 << 120) + 8).unwrap()),
        delete_mode: Some(GraphDeleteMode::Detach),
        original_generation: GraphGeneration::new(42),
    };
    let mut bytes = Vec::new();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut bytes, &mut || Ok(()))
        .unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(50);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let p = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    let decoded =
        verify_provenance(PayloadSlice::new(&objects, store, generation, p), &mut r).unwrap();
    assert_eq!(decoded.fields_with_key(fields.key, &mut r).unwrap(), fields);
    assert_eq!(
        decoded.key().unwrap().namespace().len(),
        namespace.len() as u64
    );
    assert!(decoded.key().unwrap().key().is_empty());
    assert!(decoded.fields_with_key(None, &mut r).is_err());
    assert!(
        decoded
            .fields_with_key(
                Some(ApplicationKey::new(EntityKind::Node, &namespace, "different").unwrap()),
                &mut r
            )
            .is_err()
    );
    for (offset, replacement) in [(4, 2), (6, 99), (7, 2), (8, 2), (65_535, 0xff)] {
        let mut broken = bytes.clone();
        broken[offset] = replacement;
        let p = prepare_payload(
            &mut objects,
            store,
            generation,
            BlockKind::OperationProvenance,
            &broken,
            &mut r,
        )
        .unwrap();
        assert!(
            verify_provenance(PayloadSlice::new(&objects, store, generation, p), &mut r).is_err(),
            "reframed corruption at {offset}"
        );
    }
    bytes.push(0);
    let p = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    assert!(verify_provenance(PayloadSlice::new(&objects, store, generation, p), &mut r).is_err());
}

#[test]
fn native_record_requires_complete_canonical_index_labels_and_provenance() {
    use zeppelin_embed::property_graph::catalog::{Symbol, SymbolKind};
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::{RecordCatalog, verify_record};
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::*;
    struct Catalog;
    impl RecordCatalog<Objects> for Catalog {
        fn resolve(
            &self,
            kind: SymbolKind,
            name: PayloadSlice<'_, Objects>,
            r: &mut TreeResources<'_>,
        ) -> Result<Symbol, TreeError> {
            let (expected, id) = match kind {
                SymbolKind::Label => (b"L".as_slice(), 7),
                SymbolKind::Property => (b"k".as_slice(), 9),
                _ => return Err(TreeError::Invalid("unexpected catalog domain")),
            };
            if name.compare_bytes(expected, r)? != std::cmp::Ordering::Equal {
                return Err(TreeError::Invalid("unknown exact name"));
            }
            Ok(Symbol::new(kind, id).unwrap())
        }
    }
    let id = NodeId::new((1u128 << 120) + 7).unwrap();
    let mut labels = [GraphName::new("L").unwrap()];
    let mut props = [GraphProperty::new(
        GraphName::new("k").unwrap(),
        PropertyValue::new(PropertyData::I64(-17)).unwrap(),
    )];
    let image = CanonicalContents::node(&mut labels, &mut props, None, None).unwrap();
    let mut canonical = Vec::new();
    image.write_to(&mut canonical, &mut || Ok(())).unwrap();
    assert_eq!(canonical.len(), 52);
    assert_eq!(canonical[41], 3);
    let fields = OperationFields {
        operation: GraphOperation::StructuredCreate,
        key: Some(ApplicationKey::new(EntityKind::Node, "app", "n").unwrap()),
        requested_revision: GraphRevision::new(4).unwrap(),
        installed_revision: GraphRevision::new(4).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: EntityId::Node(id),
        delete_mode: None,
        original_generation: GraphGeneration::new(1),
    };
    let mut provenance = Vec::new();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut provenance, &mut || Ok(()))
        .unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(2);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let c = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::CanonicalImage,
        &canonical,
        &mut r,
    )
    .unwrap();
    let p = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::OperationProvenance,
        &provenance,
        &mut r,
    )
    .unwrap();
    let mut record = vec![0; 176];
    record[..16].copy_from_slice(&id.get().to_le_bytes());
    record[16..24].copy_from_slice(&4u64.to_le_bytes());
    record[28..32].copy_from_slice(&1u32.to_le_bytes());
    record[32..40].copy_from_slice(&32u64.to_le_bytes());
    record[40..48].copy_from_slice(&7u64.to_le_bytes());
    record[48..52].copy_from_slice(&1u32.to_le_bytes());
    record[56..64].copy_from_slice(&9u64.to_le_bytes());
    record[64..72].copy_from_slice(&41u64.to_le_bytes());
    record[72..80].copy_from_slice(&9u64.to_le_bytes());
    c.encode_into(&mut record[80..128]).unwrap();
    p.encode_into(&mut record[128..176]).unwrap();
    let native = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::NodeRecord,
        &record,
        &mut r,
    )
    .unwrap();
    let view = verify_record(
        PayloadSlice::new(&objects, store, generation, native),
        EntityId::Node(id),
        &Catalog,
        None,
        &mut r,
    )
    .unwrap();
    assert_eq!(view.incarnation(), EntityId::Node(id));
    assert_eq!(view.revision().get(), 4);
    assert_eq!(
        view.provenance()
            .fields_with_key(fields.key, &mut r)
            .unwrap(),
        fields
    );
    assert_eq!(view.canonical().property_count(), 1);
    assert_eq!(view.label(0, &mut r).unwrap().get(), 7);
    assert!(view.label(1, &mut r).is_err());
    let value = view
        .property(catalog::PropertyKeyId::new(9).unwrap(), &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(
        value
            .compare_bytes(&[vec![3], (-17i64).to_le_bytes().to_vec()].concat(), &mut r)
            .unwrap(),
        std::cmp::Ordering::Equal
    );
    assert!(
        view.property(catalog::PropertyKeyId::new(10).unwrap(), &mut r)
            .unwrap()
            .is_none()
    );
    let mut zero_generation = provenance.clone();
    let end = zero_generation.len();
    zero_generation[end - 8..].fill(0);
    let zero_provenance = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &zero_generation,
        &mut r,
    )
    .unwrap();
    let mut bad = record.clone();
    zero_provenance.encode_into(&mut bad[128..176]).unwrap();
    let zero_native = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::NodeRecord,
        &bad,
        &mut r,
    )
    .unwrap();
    assert!(
        verify_record(
            PayloadSlice::new(&objects, store, generation, zero_native),
            EntityId::Node(id),
            &Catalog,
            None,
            &mut r
        )
        .is_err(),
        "installed content cannot originate at empty generation zero"
    );
    assert_native_cow_rejects_invalid_old_values(
        &mut objects,
        &Catalog,
        EntityId::Node(id),
        native,
        zero_native,
        &shared,
        &mut r,
    );
    let mut corruptions = Vec::new();
    for (offset, replacement) in [(15, 0), (16, 5), (40, 8), (64, 42)] {
        let mut bad = record.clone();
        bad[offset] = replacement;
        corruptions.push(bad);
    }
    let mut omitted = record.clone();
    omitted.drain(56..80);
    omitted[48..52].fill(0);
    omitted[32..40].copy_from_slice(&8u64.to_le_bytes());
    corruptions.push(omitted);
    let mut orphan = record.clone();
    let extra = [10u64.to_le_bytes(), 41u64.to_le_bytes(), 9u64.to_le_bytes()].concat();
    orphan.splice(80..80, extra);
    orphan[48..52].copy_from_slice(&2u32.to_le_bytes());
    orphan[32..40].copy_from_slice(&56u64.to_le_bytes());
    corruptions.push(orphan);
    for bad in corruptions {
        let native = prepare_payload(
            &mut objects,
            store,
            generation,
            BlockKind::NodeRecord,
            &bad,
            &mut r,
        )
        .unwrap();
        assert!(
            verify_record(
                PayloadSlice::new(&objects, store, generation, native),
                EntityId::Node(id),
                &Catalog,
                None,
                &mut r
            )
            .is_err(),
            "repaired outer checksum cannot conceal record mismatch"
        );
    }
}

#[test]
fn relationship_record_correlates_full_topology_and_rejects_future_payloads() {
    use zeppelin_embed::property_graph::catalog::{Symbol, SymbolKind};
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::{
        RecordCatalog, RecordShape, verify_record,
    };
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::*;
    struct Catalog;
    impl RecordCatalog<Objects> for Catalog {
        fn resolve(
            &self,
            kind: SymbolKind,
            name: PayloadSlice<'_, Objects>,
            r: &mut TreeResources<'_>,
        ) -> Result<Symbol, TreeError> {
            if kind != SymbolKind::RelationshipType
                || name.compare_bytes(b"R", r)? != std::cmp::Ordering::Equal
            {
                return Err(TreeError::Invalid("unknown exact relationship type"));
            }
            Ok(Symbol::new(kind, 3).unwrap())
        }
    }
    let id = RelId::new((1u128 << 110) + 7).unwrap();
    let a = NodeId::new((1u128 << 100) + 7).unwrap();
    let b = NodeId::new((1u128 << 101) + 7).unwrap();
    let image =
        CanonicalContents::relationship(a, b, GraphName::new("R").unwrap(), &mut []).unwrap();
    let mut canonical = Vec::new();
    image.write_to(&mut canonical, &mut || Ok(())).unwrap();
    let fields = OperationFields {
        operation: GraphOperation::CypherEdit,
        key: None,
        requested_revision: GraphRevision::new(1).unwrap(),
        installed_revision: GraphRevision::new(1).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: EntityId::Relationship(id),
        delete_mode: None,
        original_generation: GraphGeneration::new(1),
    };
    let mut provenance = Vec::new();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut provenance, &mut || Ok(()))
        .unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(2);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let c = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::CanonicalImage,
        &canonical,
        &mut r,
    )
    .unwrap();
    let p = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::OperationProvenance,
        &provenance,
        &mut r,
    )
    .unwrap();
    let mut record = vec![0; 184];
    record[..16].copy_from_slice(&id.get().to_le_bytes());
    record[16..32].copy_from_slice(&a.get().to_le_bytes());
    record[32..48].copy_from_slice(&b.get().to_le_bytes());
    record[48..56].copy_from_slice(&3u64.to_le_bytes());
    record[56..64].copy_from_slice(&1u64.to_le_bytes());
    record[72..80].copy_from_slice(&8u64.to_le_bytes());
    c.encode_into(&mut record[88..136]).unwrap();
    p.encode_into(&mut record[136..184]).unwrap();
    let native = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::RelRecord,
        &record,
        &mut r,
    )
    .unwrap();
    let view = verify_record(
        PayloadSlice::new(&objects, store, generation, native),
        EntityId::Relationship(id),
        &Catalog,
        None,
        &mut r,
    )
    .unwrap();
    assert_eq!(
        view.shape(),
        RecordShape::Relationship {
            id,
            source: a,
            target: b,
            relationship_type: catalog::RelTypeId::new(3).unwrap()
        }
    );
    assert_eq!(
        view.provenance().fields_with_key(None, &mut r).unwrap(),
        fields
    );
    for offset in [16, 32, 48, 64, 68] {
        let mut bad = record.clone();
        bad[offset] ^= 1;
        let native = prepare_payload(
            &mut objects,
            store,
            generation,
            BlockKind::RelRecord,
            &bad,
            &mut r,
        )
        .unwrap();
        assert!(
            verify_record(
                PayloadSlice::new(&objects, store, generation, native),
                EntityId::Relationship(id),
                &Catalog,
                None,
                &mut r
            )
            .is_err()
        );
    }
    let valid_native = native;
    let future = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(3),
        BlockKind::CanonicalImage,
        &canonical,
        &mut r,
    )
    .unwrap();
    future.encode_into(&mut record[88..136]).unwrap();
    let native = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::RelRecord,
        &record,
        &mut r,
    )
    .unwrap();
    assert!(
        verify_record(
            PayloadSlice::new(&objects, store, GraphGeneration::new(3), native),
            EntityId::Relationship(id),
            &Catalog,
            None,
            &mut r
        )
        .is_err(),
        "a later read view cannot legitimize a payload newer than its parent record"
    );
    assert_native_cow_rejects_invalid_old_values(
        &mut objects,
        &Catalog,
        EntityId::Relationship(id),
        valid_native,
        native,
        &shared,
        &mut r,
    );
}

#[test]
fn preparation_capacity_is_shared_by_scratch_buffers_and_writer() {
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::memory::{StorageBuffer, StorageMemory};
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let baseline = shared.reserved_bytes().unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let controls = memory.reserved_bytes();
    assert_eq!(writer.reserved_bytes(), controls);
    let first = StorageBuffer::<u8>::new(&memory, 17 * 1024 * 1024).unwrap();
    let charged = memory.reserved_bytes();
    let failed = StorageBuffer::<u8>::new(&memory, 16 * 1024 * 1024);
    assert!(matches!(failed, Err(TreeError::Memory)));
    drop(failed);
    assert_eq!(memory.reserved_bytes(), charged);
    assert_eq!(writer.reserved_bytes(), charged);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline + charged as u64);
    let scratch = TreeScratch::for_prepare(&memory).unwrap();
    let mut resources = TreeResources::for_prepare(&memory, 100).unwrap();
    let objects = Objects::new();
    let empty = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
    let cursor = DirectoryCursor::seek(&objects, empty, None, &mut resources).unwrap();
    let total = charged
        + scratch.owned_bytes()
        + resources.reserved_bytes() as usize
        + cursor.owned_bytes();
    assert_eq!(memory.reserved_bytes(), total);
    assert_eq!(writer.reserved_bytes(), total);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline + total as u64);
    assert_eq!(memory.peak_reserved_bytes(), total);
    drop(cursor);
    drop(resources);
    drop(scratch);
    drop(first);
    assert_eq!(memory.reserved_bytes(), controls);
    drop(memory);
    assert_eq!(writer.reserved_bytes(), 0);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
}

#[test]
#[cfg(feature = "allocation-audit")]
fn preparation_reservations_release_on_allocator_and_lower_owner_failures() {
    use zeppelin_embed::adversarial_test_support::{audit_engine_path, fail_attributed_allocation};
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::memory::{StorageBuffer, StorageMemory};
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 4096).unwrap();
    let local_before = memory.reserved_bytes();
    let aggregate_before = shared.reserved_bytes().unwrap();
    let ((failed, fires), audit) = audit_engine_path(|| {
        fail_attributed_allocation(1, || StorageBuffer::<u64>::new(&memory, 17))
    });
    assert!(matches!(failed, Err(TreeError::Memory)));
    drop(failed);
    assert_eq!(fires, 1, "actual System allocator refusal must fire");
    assert_eq!(audit.allocations, 0);
    assert_eq!(memory.reserved_bytes(), local_before);
    assert_eq!(writer.reserved_bytes(), local_before);
    assert_eq!(shared.reserved_bytes().unwrap(), aggregate_before);
    let (successful, audit) = audit_engine_path(|| StorageBuffer::<u64>::new(&memory, 17));
    let mut successful = successful.unwrap();
    successful.push(42).unwrap();
    assert_eq!(successful.as_slice(), &[42]);
    assert_eq!(audit.allocations, 1);
    assert_eq!(audit.attributed_bytes, successful.owned_bytes() as u64);
    assert_eq!(audit.unattributed_bytes, 0);
    drop(successful);
    assert_eq!(memory.reserved_bytes(), local_before);
    let remaining = 256 * 1024 * 1024 - shared.reserved_bytes().unwrap() as usize;
    let other = shared.reserve(remaining - 32).unwrap();
    let shared_before = shared.reserved_bytes().unwrap();
    let peak_before = memory.peak_reserved_bytes();
    let (failed, fires) = fail_attributed_allocation(1, || StorageBuffer::<u8>::new(&memory, 64));
    assert!(matches!(failed, Err(TreeError::Memory)));
    drop(failed);
    assert_eq!(fires, 0, "aggregate refusal must precede allocation");
    assert_eq!(memory.reserved_bytes(), local_before);
    assert_eq!(memory.peak_reserved_bytes(), peak_before);
    assert_eq!(writer.reserved_bytes(), local_before);
    assert_eq!(shared.reserved_bytes().unwrap(), shared_before);
    drop(other);
    drop(memory);
    assert_eq!(writer.reserved_bytes(), 0);
    let writer = WriteMemory::new(
        &shared,
        WriteLimits {
            writer_bytes: 1024,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let memory = StorageMemory::new(&writer, &control, 4096).unwrap();
    let before = memory.reserved_bytes();
    let shared_before = shared.reserved_bytes().unwrap();
    let (failed, fires) = fail_attributed_allocation(1, || StorageBuffer::<u8>::new(&memory, 1024));
    assert!(matches!(failed, Err(TreeError::Memory)));
    drop(failed);
    assert_eq!(fires, 0, "writer refusal must precede allocation");
    assert_eq!(memory.reserved_bytes(), before);
    assert_eq!(memory.peak_reserved_bytes(), before);
    assert_eq!(writer.reserved_bytes(), before);
    assert_eq!(shared.reserved_bytes().unwrap(), shared_before);
}

#[test]
fn prepared_native_record_sorts_symbols_without_rewriting_canonical_contents() {
    use zeppelin_embed::property_graph::catalog::{PropertyKeyId, Symbol, SymbolKind};
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::memory::StorageMemory;
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::{
        RecordCatalog, RecordInput, prepare_record, verify_record,
    };
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::*;
    struct Catalog;
    impl RecordCatalog<Objects> for Catalog {
        fn resolve(
            &self,
            kind: SymbolKind,
            name: PayloadSlice<'_, Objects>,
            r: &mut TreeResources<'_>,
        ) -> Result<Symbol, TreeError> {
            let id = if name.compare_bytes(b"a", r)?.is_eq() {
                20
            } else if name.compare_bytes(b"z", r)?.is_eq() {
                3
            } else {
                return Err(TreeError::Invalid("unknown fixture name"));
            };
            Symbol::new(kind, id).map_err(|_| TreeError::Invalid("fixture symbol"))
        }
    }
    let id = NodeId::new((1u128 << 117) + 9).unwrap();
    let mut labels = [GraphName::new("z").unwrap(), GraphName::new("a").unwrap()];
    let mut props = [
        GraphProperty::new(
            GraphName::new("z").unwrap(),
            PropertyValue::new(PropertyData::I64(-17)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("a").unwrap(),
            PropertyValue::new(PropertyData::F64(f64::from_bits(0x7ff8_0000_0000_0042))).unwrap(),
        ),
    ];
    let image = CanonicalContents::node(&mut labels, &mut props, Some(""), None).unwrap();
    let mut canonical = Vec::new();
    image.write_to(&mut canonical, &mut || Ok(())).unwrap();
    let fields = OperationFields {
        operation: GraphOperation::CypherEdit,
        key: None,
        requested_revision: GraphRevision::new(1).unwrap(),
        installed_revision: GraphRevision::new(1).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: EntityId::Node(id),
        delete_mode: None,
        original_generation: GraphGeneration::new(4),
    };
    let mut provenance = Vec::new();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut provenance, &mut || Ok(()))
        .unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(9);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 10_000_000).unwrap();
    let canonical_ref = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::CanonicalImage,
        &canonical,
        &mut r,
    )
    .unwrap();
    let provenance_ref = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &provenance,
        &mut r,
    )
    .unwrap();
    let before = memory.reserved_bytes();
    let record = prepare_record(
        &mut objects,
        RecordInput {
            store,
            generation,
            entity: EntityId::Node(id),
            canonical: canonical_ref,
            provenance: provenance_ref,
        },
        &Catalog,
        None,
        &memory,
        &mut r,
    )
    .unwrap();
    assert_eq!(
        memory.reserved_bytes(),
        before,
        "temporary index arrays are released"
    );
    let view = verify_record(
        PayloadSlice::new(&objects, store, generation, record),
        EntityId::Node(id),
        &Catalog,
        None,
        &mut r,
    )
    .unwrap();
    assert_eq!(view.label(0, &mut r).unwrap().get(), 3);
    assert_eq!(view.label(1, &mut r).unwrap().get(), 20);
    let a = view
        .property(PropertyKeyId::new(20).unwrap(), &mut r)
        .unwrap()
        .unwrap();
    let z = view
        .property(PropertyKeyId::new(3).unwrap(), &mut r)
        .unwrap()
        .unwrap();
    let mut a_bytes = [0; 9];
    a.read_at(0, &mut a_bytes, &mut r).unwrap();
    assert_eq!(a_bytes[0], 4);
    assert_eq!(
        u64::from_le_bytes(a_bytes[1..].try_into().unwrap()),
        0x7ff8_0000_0000_0042
    );
    let mut z_bytes = [0; 9];
    z.read_at(0, &mut z_bytes, &mut r).unwrap();
    assert_eq!(z_bytes[0], 3);
    assert_eq!(i64::from_le_bytes(z_bytes[1..].try_into().unwrap()), -17);
    assert_eq!(view.canonical().stored_text().unwrap().len(), 0);
    assert_eq!(
        PayloadSlice::new(&objects, store, generation, canonical_ref)
            .compare_bytes(&canonical, &mut r)
            .unwrap(),
        std::cmp::Ordering::Equal
    );
    assert_eq!(
        view.provenance().fields_with_key(None, &mut r).unwrap(),
        fields
    );
}

#[test]
fn packed_private_sink_reopens_files_and_keeps_unchanged_old_roots() {
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::memory::{StorageBuffer, StorageMemory};
    use zeppelin_embed::property_graph::storage::prepared::{
        OwnedArtifact, PackLimits, PreparedObjects,
    };
    let base = Objects::new();
    let store = base.store;
    let generation = GraphGeneration::new(3);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
    let mut next = 1u128;
    let mut objects = PreparedObjects::new(
        &base,
        || {
            let current = next;
            next += 1;
            Ok(ArtifactIdentity {
                store,
                artifact: ArtifactId::new(current)?,
                generation,
                creation_serial: current as u64,
            })
        },
        store,
        generation,
        PackLimits {
            artifact_bytes: 256 * 1024,
            blocks: 8,
            ..PackLimits::default()
        },
        &memory,
        &mut r,
    )
    .unwrap();
    let mut scratch = TreeScratch::for_prepare(&memory).unwrap();
    let mut root = DirectoryRoot::empty(store, TreeKind::Nodes, generation);
    let mut old = root;
    for id in 1..=20u128 {
        root = insert(
            &mut objects,
            root,
            &id.to_le_bytes(),
            &(id as u64).to_le_bytes(),
            generation,
            &mut scratch,
            &mut r,
        )
        .unwrap();
        if id == 5 {
            old = root;
        }
    }
    assert_eq!(
        objects.abort_inventory().count(),
        3,
        "eight immutable pages share each physical pack"
    );
    assert!(
        objects.artifact(0).is_err(),
        "no prepare result before complete finalization"
    );
    objects.finish(&mut r).unwrap();
    assert_eq!(objects.len(), 3);
    let directory = tempfile::tempdir().unwrap();
    let mut reopened = StorageBuffer::new(&memory, objects.len()).unwrap();
    for index in 0..objects.len() {
        let artifact = objects.artifact(index).unwrap();
        let path = directory.path().join(format!("{}.graph", index));
        std::fs::write(&path, artifact.bytes()).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        reopened
            .push(
                OwnedArtifact::read_from(
                    &mut file,
                    artifact.bytes().len(),
                    (store, artifact.identity().artifact),
                    &memory,
                    &mut r,
                )
                .unwrap(),
            )
            .unwrap();
    }
    struct Reopened<'a>(StorageBuffer<'a, OwnedArtifact<'a>>);
    impl BlockSource for Reopened<'_> {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            r: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            for object in self.0.as_slice() {
                r.step(1)?;
                if object.identity().artifact == reference.artifact {
                    return object.resolve(reference, r);
                }
            }
            Err(TreeError::Missing)
        }
    }
    let reopened = Reopened(reopened);
    for id in 1..=20u128 {
        let mut bytes = [0; 8];
        assert_eq!(
            lookup(&reopened, root, &id.to_le_bytes(), &mut bytes, &mut r).unwrap(),
            Some(8)
        );
        assert_eq!(u64::from_le_bytes(bytes), id as u64);
        assert_eq!(
            lookup(&reopened, old, &id.to_le_bytes(), &mut bytes, &mut r).unwrap(),
            (id <= 5).then_some(8)
        );
    }
    assert!(
        objects
            .append(BlockKind::StoredText, generation, b"late", &mut r)
            .is_err()
    );
}

#[test]
fn node_tombstone_preserves_full_deletion_provenance_without_dead_payloads() {
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::memory::StorageMemory;
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::{
        prepare_node_tombstone, verify_node_tombstone,
    };
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::*;
    let id = NodeId::new((1u128 << 110) + 7).unwrap();
    let fields = OperationFields {
        operation: GraphOperation::CypherEdit,
        key: None,
        requested_revision: GraphRevision::new(8).unwrap(),
        installed_revision: GraphRevision::new(8).unwrap(),
        expected: ExpectedGraphState::Entity(EntityId::Node(id)),
        incarnation: EntityId::Node(id),
        delete_mode: Some(GraphDeleteMode::Detach),
        original_generation: GraphGeneration::new(17),
    };
    let mut bytes = Vec::new();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut bytes, &mut || Ok(()))
        .unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(19);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 10_000_000).unwrap();
    let provenance = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    let tombstone =
        prepare_node_tombstone(&mut objects, store, generation, id, provenance, &mut r).unwrap();
    assert_eq!(
        tombstone.len(),
        88,
        "header40 + provenance48, no dead labels/properties/canonical data"
    );
    let source = PayloadSlice::new(&objects, store, generation, tombstone);
    let view = verify_node_tombstone(source, id, &mut r).unwrap();
    assert_eq!(view.node(), id);
    assert_eq!(view.revision().get(), 8);
    assert_eq!(
        view.provenance().fields_with_key(None, &mut r).unwrap(),
        fields
    );
    let mut encoded = vec![0; 88];
    source.read_at(0, &mut encoded, &mut r).unwrap();
    assert_eq!(u32::from_le_bytes(encoded[24..28].try_into().unwrap()), 1);
    assert!(encoded[28..40].iter().all(|b| *b == 0));
    for offset in [15, 16, 24, 28, 32] {
        let mut bad = encoded.clone();
        bad[offset] ^= 4;
        let bad = prepare_payload(
            &mut objects,
            store,
            generation,
            BlockKind::NodeRecord,
            &bad,
            &mut r,
        )
        .unwrap();
        assert!(
            verify_node_tombstone(
                PayloadSlice::new(&objects, store, generation, bad),
                id,
                &mut r
            )
            .is_err(),
            "reframed tombstone corruption at {offset}"
        );
    }
    let fields = OperationFields {
        delete_mode: None,
        ..fields
    };
    let mut bytes = Vec::new();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut bytes, &mut || Ok(()))
        .unwrap();
    let no_delete = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    assert!(
        prepare_node_tombstone(&mut objects, store, generation, id, no_delete, &mut r).is_err()
    );
}

#[test]
fn typed_fence_probes_stream_long_keys_without_copying_or_writing_on_lookup() {
    use zeppelin_embed::property_graph::storage::tree::directory::{
        FenceKey, insert_fence, lookup_fence,
    };
    use zeppelin_embed::property_graph::{EntityKind, catalog::NamespaceId};
    let long = format!("{}🦀\0z", "a".repeat(90_000));
    let mut objects = Objects::new();
    let store = objects.store;
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 30_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1 << 20).unwrap();
    let namespace = NamespaceId::new((1u64 << 60) + 3).unwrap();
    let mut root = DirectoryRoot::empty(store, TreeKind::KeyFences, GraphGeneration::new(0));
    for (key, value) in [("", b"empty".as_slice()), ("\0", b"nul"), (&long, b"long")] {
        root = insert_fence(
            &mut objects,
            root,
            FenceKey::new(EntityKind::Node, namespace, key).unwrap(),
            value,
            GraphGeneration::new(1),
            &mut scratch,
            &mut r,
        )
        .unwrap();
    }
    let writes = objects.writes;
    for (key, value) in [("", b"empty".as_slice()), ("\0", b"nul"), (&long, b"long")] {
        let mut bytes = [0; 5];
        let count = lookup_fence(
            &objects,
            root,
            FenceKey::new(EntityKind::Node, namespace, key).unwrap(),
            &mut bytes,
            &mut r,
        )
        .unwrap()
        .unwrap();
        assert_eq!(&bytes[..count], value);
    }
    assert_eq!(
        objects.writes, writes,
        "a borrowed lookup never persists its probe"
    );
    let different = format!("{}x", &long[..long.len() - 1]);
    let mut bytes = [0; 5];
    assert_eq!(
        lookup_fence(
            &objects,
            root,
            FenceKey::new(EntityKind::Node, namespace, &different).unwrap(),
            &mut bytes,
            &mut r
        )
        .unwrap(),
        None
    );
    assert_eq!(
        lookup_fence(
            &objects,
            root,
            FenceKey::new(EntityKind::Relationship, namespace, &long).unwrap(),
            &mut bytes,
            &mut r
        )
        .unwrap(),
        None
    );
    assert_eq!(
        lookup_fence(
            &objects,
            root,
            FenceKey::new(EntityKind::Node, NamespaceId::new(3).unwrap(), &long).unwrap(),
            &mut bytes,
            &mut r
        )
        .unwrap(),
        None
    );
}

#[test]
fn keyed_fence_retains_complete_history_and_checks_containing_leaf_generation() {
    use zeppelin_embed::property_graph::catalog::{NamespaceId, Symbol, SymbolKind};
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    use zeppelin_embed::property_graph::storage::records::{
        FenceInput, RecordCatalog, prepare_fence, verify_fence_entry,
    };
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::storage::tree::directory::{
        FenceKey, insert_fence, lookup_fence_entry,
    };
    use zeppelin_embed::property_graph::*;
    struct Catalog;
    impl RecordCatalog<Objects> for Catalog {
        fn resolve(
            &self,
            kind: SymbolKind,
            name: PayloadSlice<'_, Objects>,
            r: &mut TreeResources<'_>,
        ) -> Result<Symbol, TreeError> {
            if kind != SymbolKind::Namespace
                || name.compare_bytes(b"app\0", r)? != std::cmp::Ordering::Equal
            {
                return Err(TreeError::Invalid("unknown namespace"));
            }
            Ok(Symbol::Namespace(
                NamespaceId::new((1u64 << 61) + 3).unwrap(),
            ))
        }
    }
    let id = NodeId::new((1u128 << 120) + 37).unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let generation = GraphGeneration::new(2);
    let control = QueryControl::Cancel(CancelToken::new());
    let (_dir, _store, shared) = memory_fixture();
    let mut r = TreeResources::new(&control, &shared, 30_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1 << 20).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    let canonical = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::CanonicalImage,
        &bytes,
        &mut r,
    )
    .unwrap();
    let key = ApplicationKey::new(EntityKind::Node, "app\0", "🦀\0key").unwrap();
    let probe = FenceKey::new(
        EntityKind::Node,
        NamespaceId::new((1u64 << 61) + 3).unwrap(),
        "🦀\0key",
    )
    .unwrap();
    let fields = OperationFields {
        operation: GraphOperation::StructuredCreate,
        key: Some(key),
        requested_revision: GraphRevision::new(4).unwrap(),
        installed_revision: GraphRevision::new(4).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: EntityId::Node(id),
        delete_mode: None,
        original_generation: GraphGeneration::new(1),
    };
    bytes.clear();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut bytes, &mut || Ok(()))
        .unwrap();
    let provenance = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(1),
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    let input = FenceInput {
        store,
        generation,
        key: probe,
        provenance,
        canonical: Some(canonical),
    };
    let value = prepare_fence(&objects, input, &Catalog, None, &mut r).unwrap();
    assert_eq!(value.len(), 144);
    assert_eq!(&value[..8], &[1, 0, 1, 1, 0, 0, 0, 0]);
    assert_eq!(&value[8..24], &id.get().to_le_bytes());
    assert_eq!(&value[24..32], &4u64.to_le_bytes());
    assert_eq!(&value[32..40], &1u64.to_le_bytes());
    assert_eq!(&value[88..96], &[1, 0, 0, 0, 0, 0, 0, 0]);
    let root = insert_fence(
        &mut objects,
        DirectoryRoot::empty(store, TreeKind::KeyFences, generation),
        probe,
        &value,
        generation,
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let entry = lookup_fence_entry(&objects, root, probe, &mut r)
        .unwrap()
        .unwrap();
    let fence = verify_fence_entry(&objects, root, entry, &Catalog, None, &mut r).unwrap();
    let another_context = DirectoryRoot::from_reference(
        store,
        TreeKind::KeyFences,
        GraphGeneration::new(9),
        root.reference(),
    )
    .unwrap();
    assert!(
        verify_fence_entry(&objects, another_context, entry, &Catalog, None, &mut r).is_err(),
        "an entry cannot be transplanted into another root context"
    );
    assert_eq!(fence.incarnation(), EntityId::Node(id));
    assert_eq!(fence.revision(), fields.installed_revision);
    assert!(!fence.is_deleted());
    assert!(fence.canonical().is_some());
    assert_eq!(
        fence
            .provenance()
            .fields_with_key(Some(key), &mut r)
            .unwrap(),
        fields
    );
    // Every repaired-checksum duplicated/reserved header field is correlated.
    for offset in [0, 2, 3, 4, 8, 24, 32, 88, 89, 95] {
        let mut malformed = value;
        malformed[offset] ^= 4;
        let candidate = insert_fence(
            &mut objects,
            root,
            probe,
            &malformed,
            generation,
            &mut scratch,
            &mut r,
        )
        .unwrap();
        let entry = lookup_fence_entry(&objects, candidate, probe, &mut r)
            .unwrap()
            .unwrap();
        assert!(
            verify_fence_entry(&objects, candidate, entry, &Catalog, None, &mut r).is_err(),
            "repaired fence field {offset}"
        );
    }
    // Root generation cannot authorize a newer descendant of an older leaf.
    let future = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(3),
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    let mut invalid = value;
    future.encode_into(&mut invalid[40..88]).unwrap();
    let old_leaf = insert_fence(
        &mut objects,
        root,
        probe,
        &invalid,
        generation,
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let newer_root = DirectoryRoot::from_reference(
        store,
        TreeKind::KeyFences,
        GraphGeneration::new(9),
        old_leaf.reference(),
    )
    .unwrap();
    let entry = lookup_fence_entry(&objects, newer_root, probe, &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(entry.creation_generation(), generation);
    assert!(
        verify_fence_entry(&objects, newer_root, entry, &Catalog, None, &mut r).is_err(),
        "new root does not relax old leaf generation"
    );
    // Independent review: COW of a neighboring key cannot legitimize an
    // already-invalid future link retained from the old containing leaf.
    let other_probe = FenceKey::new(
        EntityKind::Node,
        NamespaceId::new((1u64 << 61) + 3).unwrap(),
        "unrelated",
    )
    .unwrap();
    let other_key = ApplicationKey::new(EntityKind::Node, "app\0", "unrelated").unwrap();
    let other_fields = OperationFields {
        key: Some(other_key),
        incarnation: EntityId::Node(NodeId::new(id.get() + 1).unwrap()),
        original_generation: GraphGeneration::new(4),
        ..fields
    };
    let mut other_bytes = Vec::new();
    OperationProvenance::from_fields(Some(1), other_fields)
        .unwrap()
        .write_to(&mut other_bytes, &mut || Ok(()))
        .unwrap();
    let other_provenance = prepare_payload(
        &mut objects,
        store,
        GraphGeneration::new(4),
        BlockKind::OperationProvenance,
        &other_bytes,
        &mut r,
    )
    .unwrap();
    let other_value = prepare_fence(
        &objects,
        FenceInput {
            store,
            generation: GraphGeneration::new(4),
            key: other_probe,
            provenance: other_provenance,
            canonical: Some(canonical),
        },
        &Catalog,
        None,
        &mut r,
    )
    .unwrap();
    use zeppelin_embed::property_graph::storage::records::NativeDirectoryValues;
    use zeppelin_embed::property_graph::storage::tree::directory::{
        DirectoryMutation, insert_fence_checked,
    };
    let writes = objects.writes;
    assert!(
        insert_fence_checked(
            &mut objects,
            DirectoryMutation::new(
                old_leaf,
                GraphGeneration::new(4),
                NativeDirectoryValues::new(&Catalog, None)
            ),
            other_probe,
            &other_value,
            &mut scratch,
            &mut r,
        )
        .is_err(),
        "COW must reject an old leaf's future reference before copying it"
    );
    assert_eq!(
        objects.writes, writes,
        "invalid old leaf must not append a replacement"
    );
    let rewritten = insert_fence_checked(
        &mut objects,
        DirectoryMutation::new(
            root,
            GraphGeneration::new(4),
            NativeDirectoryValues::new(&Catalog, None),
        ),
        other_probe,
        &other_value,
        &mut scratch,
        &mut r,
    )
    .unwrap();
    for check in [other_probe, probe] {
        let entry = lookup_fence_entry(&objects, rewritten, check, &mut r)
            .unwrap()
            .unwrap();
        assert!(verify_fence_entry(&objects, rewritten, entry, &Catalog, None, &mut r).is_ok());
    }
    let deleted = OperationFields {
        operation: GraphOperation::StructuredDelete,
        requested_revision: GraphRevision::new(5).unwrap(),
        installed_revision: GraphRevision::new(5).unwrap(),
        expected: ExpectedGraphState::Entity(EntityId::Node(id)),
        delete_mode: Some(GraphDeleteMode::Detach),
        original_generation: GraphGeneration::new(7),
        ..fields
    };
    bytes.clear();
    OperationProvenance::from_fields(Some(1), deleted)
        .unwrap()
        .write_to(&mut bytes, &mut || Ok(()))
        .unwrap();
    let generation = GraphGeneration::new(7);
    let provenance = prepare_payload(
        &mut objects,
        store,
        generation,
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    let dead = prepare_fence(
        &objects,
        FenceInput {
            store,
            generation,
            key: probe,
            provenance,
            canonical: None,
        },
        &Catalog,
        None,
        &mut r,
    )
    .unwrap();
    assert_eq!(dead[3], 2);
    assert_eq!(&dead[88..], &[0; 56]);
    let root = insert_fence(
        &mut objects,
        root,
        probe,
        &dead,
        generation,
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let entry = lookup_fence_entry(&objects, root, probe, &mut r)
        .unwrap()
        .unwrap();
    let fence = verify_fence_entry(&objects, root, entry, &Catalog, None, &mut r).unwrap();
    assert!(fence.is_deleted());
    assert!(fence.canonical().is_none());
    assert_eq!(
        fence
            .provenance()
            .fields_with_key(Some(key), &mut r)
            .unwrap(),
        deleted
    );
    assert!(
        prepare_fence(
            &objects,
            FenceInput {
                store,
                generation,
                key: probe,
                provenance,
                canonical: Some(canonical)
            },
            &Catalog,
            None,
            &mut r
        )
        .is_err(),
        "deleted fences contain no live canonical image"
    );
}

#[test]
fn inventory_directory_preserves_exact_descriptors_without_granting_liveness() {
    use zeppelin_embed::property_graph::storage::inventory::{
        apply_inventory, verify_inventory_entry,
    };
    use zeppelin_embed::property_graph::storage::tree::directory::lookup_entry;
    use zeppelin_embed::property_graph::wal::{
        ArtifactDescriptor, BatchId, InventoryChange, InventoryState,
    };
    let mut objects = Objects::new();
    let store = objects.store;
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1 << 20).unwrap();
    let generation = GraphGeneration::new(9);
    let root = DirectoryRoot::empty(store, TreeKind::ObjectInventory, generation);
    let descriptor = ArtifactDescriptor {
        store,
        artifact: ArtifactId::new((1u128 << 120) + 9).unwrap(),
        generation: GraphGeneration::new(3),
        serial: (1u64 << 62) + 1,
        bytes: 104,
        family: 17,
        version: 1,
        checksum: 0xdead_beef_1234_5678,
    };
    let changed = InventoryChange {
        object: descriptor,
        state: InventoryState::Retained,
    };
    let first = apply_inventory(
        &mut objects,
        root,
        &[changed],
        generation,
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let key = descriptor.artifact.get().to_le_bytes();
    let entry = lookup_entry(&objects, first, &key, &mut r)
        .unwrap()
        .unwrap();
    let decoded = verify_inventory_entry(first, entry, &mut r).unwrap();
    assert_eq!(decoded.object, descriptor);
    assert_eq!(decoded.state, InventoryState::Retained);
    assert_eq!(
        entry.value().len(),
        88,
        "exact existing WAL descriptor+inventory state geometry"
    );
    assert_eq!(&entry.value()[..16], &store.get().to_le_bytes());
    assert_eq!(
        &entry.value()[16..32],
        &descriptor.artifact.get().to_le_bytes()
    );
    assert_eq!(&entry.value()[64..72], &[2, 0, 0, 0, 0, 0, 0, 0]);
    let intent = BatchId::new((1u128 << 119) + 17).unwrap();
    let pending = InventoryChange {
        state: InventoryState::ReclaimPending(intent),
        ..changed
    };
    let later = apply_inventory(
        &mut objects,
        first,
        &[pending],
        GraphGeneration::new(10),
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let entry = lookup_entry(&objects, later, &key, &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(
        verify_inventory_entry(later, entry, &mut r).unwrap().state,
        pending.state
    );
    let old = lookup_entry(&objects, first, &key, &mut r)
        .unwrap()
        .unwrap();
    assert_eq!(
        verify_inventory_entry(first, old, &mut r).unwrap().state,
        InventoryState::Retained
    );
    let original = old.value().to_vec();
    let writes = objects.writes;
    assert!(
        apply_inventory(
            &mut objects,
            later,
            &[pending, pending],
            GraphGeneration::new(11),
            &mut scratch,
            &mut r
        )
        .is_err()
    );
    assert_eq!(
        objects.writes, writes,
        "duplicate inventory input fails before COW writes"
    );
    let substituted = InventoryChange {
        object: ArtifactDescriptor {
            checksum: 7,
            ..descriptor
        },
        ..pending
    };
    assert!(
        apply_inventory(
            &mut objects,
            later,
            &[substituted],
            GraphGeneration::new(11),
            &mut scratch,
            &mut r
        )
        .is_err()
    );
    assert_eq!(
        objects.writes, writes,
        "state updates cannot rewrite immutable descriptors"
    );
    // Independent review: a future descriptor on an old leaf stays invalid
    // when a different immutable descriptor causes COW of that leaf.
    let mut future_value = original.clone();
    future_value[32..40].copy_from_slice(&10u64.to_le_bytes());
    let invalid_root = insert(
        &mut objects,
        first,
        &key,
        &future_value,
        generation,
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let invalid_entry = lookup_entry(&objects, invalid_root, &key, &mut r)
        .unwrap()
        .unwrap();
    assert!(verify_inventory_entry(invalid_root, invalid_entry, &mut r).is_err());
    let neighbor = InventoryChange {
        object: ArtifactDescriptor {
            artifact: ArtifactId::new(descriptor.artifact.get() + 1).unwrap(),
            ..descriptor
        },
        ..changed
    };
    let writes = objects.writes;
    assert!(
        apply_inventory(
            &mut objects,
            invalid_root,
            &[neighbor],
            GraphGeneration::new(11),
            &mut scratch,
            &mut r,
        )
        .is_err(),
        "inventory COW must reject an old leaf's future descriptor"
    );
    assert_eq!(
        objects.writes, writes,
        "invalid inventory leaf must not append"
    );
    let valid = apply_inventory(
        &mut objects,
        first,
        &[neighbor],
        GraphGeneration::new(11),
        &mut scratch,
        &mut r,
    )
    .unwrap();
    let retained = lookup_entry(&objects, valid, &key, &mut r)
        .unwrap()
        .unwrap();
    assert!(verify_inventory_entry(valid, retained, &mut r).is_ok());
    for offset in [0, 16, 32, 51, 52, 54, 64, 65, 71] {
        let mut corrupt = original.clone();
        corrupt[offset] ^= 0x80;
        let damaged = insert(
            &mut objects,
            first,
            &key,
            &corrupt,
            generation,
            &mut scratch,
            &mut r,
        )
        .unwrap();
        let entry = lookup_entry(&objects, damaged, &key, &mut r)
            .unwrap()
            .unwrap();
        assert!(
            verify_inventory_entry(damaged, entry, &mut r).is_err(),
            "inventory repaired field {offset}"
        );
    }
}

#[test]
fn sequential_provenance_preparation_matches_shared_codec_and_preserves_typed_errors() {
    use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
    use zeppelin_embed::property_graph::storage::memory::StorageMemory;
    use zeppelin_embed::property_graph::storage::records::{prepare_provenance, verify_provenance};
    use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
    use zeppelin_embed::property_graph::*;
    let namespace = "n".repeat(90_000);
    let key = format!("{}🦀\0", "k".repeat(120_000));
    let fields = OperationFields {
        operation: GraphOperation::StructuredCreate,
        key: Some(ApplicationKey::new(EntityKind::Node, &namespace, &key).unwrap()),
        requested_revision: GraphRevision::new(7).unwrap(),
        installed_revision: GraphRevision::new(7).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: EntityId::Node(NodeId::new((1u128 << 117) + 19).unwrap()),
        delete_mode: None,
        original_generation: GraphGeneration::new(1),
    };
    let provenance = OperationProvenance::from_fields(Some(1), fields).unwrap();
    let mut expected = Vec::new();
    provenance.write_to(&mut expected, &mut || Ok(())).unwrap();
    let mut objects = Objects::new();
    let store = objects.store;
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 20_000_000).unwrap();
    let before = memory.reserved_bytes();
    let reference = prepare_provenance(
        &mut objects,
        store,
        GraphGeneration::new(1),
        provenance,
        &memory,
        &mut r,
    )
    .unwrap();
    assert_eq!(
        memory.reserved_bytes(),
        before,
        "all bounded encoder scratch releases"
    );
    assert!(
        memory.peak_reserved_bytes() - before <= 70 * 1024,
        "scratch never scales with whole key bytes"
    );
    let source = PayloadSlice::new(&objects, store, GraphGeneration::new(1), reference);
    assert_eq!(
        source.compare_bytes(&expected, &mut r).unwrap(),
        std::cmp::Ordering::Equal
    );
    assert_eq!(
        verify_provenance(source, &mut r)
            .unwrap()
            .fields_with_key(fields.key, &mut r)
            .unwrap(),
        fields
    );
    struct Refused(Objects);
    impl BlockSource for Refused {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            r: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            self.0.resolve(reference, r)
        }
    }
    impl BlockSink for Refused {
        fn append(
            &mut self,
            _: BlockKind,
            _: GraphGeneration,
            _: &[u8],
            _: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            Err(TreeError::Work)
        }
    }
    let mut refused = Refused(Objects::new());
    assert!(
        matches!(
            prepare_provenance(
                &mut refused,
                store,
                GraphGeneration::new(1),
                provenance,
                &memory,
                &mut r
            ),
            Err(TreeError::Work)
        ),
        "io::Write transport preserves original typed storage cause"
    );
    assert_eq!(memory.reserved_bytes(), before);
}

#[test]
fn cow_does_not_legitimize_future_unselected_branch_child() {
    use zeppelin_embed::property_graph::storage::tree::{
        Cell, Key, PAGE_BYTES, PageHeader, encode_page,
    };
    let mut objects = Objects::new();
    let (_dir, _store, shared) = memory_fixture();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut r = TreeResources::new(&control, &shared, 10_000_000).unwrap();
    let mut scratch = TreeScratch::new(&shared, 1024 * 1024).unwrap();
    let mut bytes = vec![0; PAGE_BYTES];
    let mut leaves = Vec::new();
    for (id, gen_no) in [(1u128, 1u64), (9, 3)] {
        let generation = GraphGeneration::new(gen_no);
        encode_page(
            PageHeader {
                kind: TreeKind::Nodes,
                level: 0,
                generation,
            },
            &[Cell::Leaf {
                key: Key::Inline(&id.to_le_bytes()),
                value: b"valid",
            }],
            &mut bytes,
        )
        .unwrap();
        leaves.push(
            objects
                .append(BlockKind::TreePage, generation, &bytes, &mut r)
                .unwrap(),
        );
    }
    let generation = GraphGeneration::new(2);
    encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 1,
            generation,
        },
        &[
            Cell::Branch {
                upper: Some(Key::Inline(&8u128.to_le_bytes())),
                child: leaves[0],
            },
            Cell::Branch {
                upper: None,
                child: leaves[1],
            },
        ],
        &mut bytes,
    )
    .unwrap();
    let reference = objects
        .append(BlockKind::TreePage, generation, &bytes, &mut r)
        .unwrap();
    let root = DirectoryRoot::from_reference(
        objects.store,
        TreeKind::Nodes,
        GraphGeneration::new(4),
        Some(reference),
    )
    .unwrap();
    let mut output = [0; 16];
    assert!(lookup(&objects, root, &9u128.to_le_bytes(), &mut output, &mut r).is_err());
    let writes = objects.writes;
    assert!(
        insert(
            &mut objects,
            root,
            &1u128.to_le_bytes(),
            b"updated",
            GraphGeneration::new(4),
            &mut scratch,
            &mut r,
        )
        .is_err(),
        "COW must reject a future unselected child before copying its parent"
    );
    assert_eq!(
        objects.writes, writes,
        "old branch is validated before the first append"
    );
    assert!(
        remove(
            &mut objects,
            root,
            &1u128.to_le_bytes(),
            GraphGeneration::new(4),
            &mut scratch,
            &mut r
        )
        .is_err()
    );
    assert_eq!(
        objects.writes, writes,
        "removal must validate the same old branches"
    );
}

fn assert_native_cow_rejects_invalid_old_values(
    objects: &mut Objects,
    catalog: &impl zeppelin_embed::property_graph::storage::records::RecordCatalog<Objects>,
    entity: zeppelin_embed::property_graph::EntityId,
    valid: zeppelin_embed::property_graph::storage::payload::PayloadRef,
    invalid: zeppelin_embed::property_graph::storage::payload::PayloadRef,
    shared: &zeppelin_embed::property_graph::resources::GraphResources,
    r: &mut TreeResources<'_>,
) {
    use zeppelin_embed::property_graph::EntityId;
    use zeppelin_embed::property_graph::storage::records::NativeDirectoryValues;
    use zeppelin_embed::property_graph::storage::tree::directory::{
        DirectoryMutation, insert_checked, remove_checked,
    };
    let (kind, id) = match entity {
        EntityId::Node(id) => (TreeKind::Nodes, id.get()),
        EntityId::Relationship(id) => (TreeKind::Relationships, id.get()),
    };
    let mut scratch = TreeScratch::new(shared, 1024 * 1024).unwrap();
    let empty = DirectoryRoot::empty(objects.store, kind, GraphGeneration::new(2));
    let mut invalid_bytes = [0; 48];
    invalid.encode_into(&mut invalid_bytes).unwrap();
    let root = insert(
        objects,
        empty,
        &id.to_le_bytes(),
        &invalid_bytes,
        GraphGeneration::new(2),
        &mut scratch,
        r,
    )
    .unwrap();
    let mut valid_bytes = [0; 48];
    valid.encode_into(&mut valid_bytes).unwrap();
    let writes = objects.writes;
    assert!(
        insert_checked(
            objects,
            DirectoryMutation::new(
                root,
                GraphGeneration::new(4),
                NativeDirectoryValues::new(catalog, None)
            ),
            &id.to_le_bytes(),
            &valid_bytes,
            &mut scratch,
            r
        )
        .is_err(),
        "native COW must validate the complete original value before replacement"
    );
    assert!(
        remove_checked(
            objects,
            DirectoryMutation::new(
                root,
                GraphGeneration::new(4),
                NativeDirectoryValues::new(catalog, None)
            ),
            &id.to_le_bytes(),
            &mut scratch,
            r
        )
        .is_err(),
        "native removal must validate the complete original value"
    );
    assert_eq!(objects.writes, writes);
    let clean = insert_checked(
        objects,
        DirectoryMutation::new(
            empty,
            GraphGeneration::new(2),
            NativeDirectoryValues::new(catalog, None),
        ),
        &id.to_le_bytes(),
        &valid_bytes,
        &mut scratch,
        r,
    )
    .unwrap();
    assert!(
        remove_checked(
            objects,
            DirectoryMutation::new(
                clean,
                GraphGeneration::new(4),
                NativeDirectoryValues::new(catalog, None)
            ),
            &id.to_le_bytes(),
            &mut scratch,
            r
        )
        .unwrap()
        .reference()
        .is_none()
    );
}
