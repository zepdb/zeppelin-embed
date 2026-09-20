#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
use zeppelin_embed::fts::tokenizer::{TokenizerConfig, TokenizerEpoch};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::property_graph::{catalog::*, resources::GraphResources, staging::*, *};
struct Empty;
impl AdmittedBase for Empty {
    fn identity(&self) -> BaseIdentity {
        BaseIdentity {
            store: StoreInstanceId::new(1).unwrap(),
            generation: GraphGeneration::new(0),
            roots: None,
        }
    }
    fn high_waters(&self) -> HighWaters {
        HighWaters {
            node: 1u128 << 100,
            relationship: 1u128 << 110,
            ..HighWaters::default()
        }
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        GraphInterpretation::new(TokenizerEpoch::of(&TokenizerConfig::text_default()), None)
            .unwrap()
    }
    fn key(
        &self,
        _: ApplicationKey<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        Ok(BaseKeyState::NeverUsed)
    }
    fn entity(
        &self,
        _: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        Ok(None)
    }
    fn has_live_incident(
        &self,
        _: NodeId,
        _: &[RelId],
        _: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        panic!("empty create must not probe incidents")
    }
    fn property(
        &self,
        _: EntityId,
        _: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        Ok(None)
    }
    fn stored_text(&self, _: NodeId, _: &mut WriteControl<'_>) -> Result<Option<&str>, StageError> {
        Ok(None)
    }
    fn symbol(
        &self,
        _: SymbolKind,
        _: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        Ok(None)
    }
}

use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::storage::participant::{
    DirectoryBase, PreparationCatalog, prepare_directories,
};
use zeppelin_embed::property_graph::storage::{
    artifact::*,
    memory::StorageMemory,
    prepared::{PackLimits, PreparedObjects},
    records::*,
    stream::PayloadSlice,
    tree::{TreeKind, directory::*},
};
struct Missing;
impl BlockSource for Missing {
    fn resolve<'a>(
        &'a self,
        _: PhysicalRef,
        _: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        Err(TreeError::Missing)
    }
}
struct Catalog<'a> {
    base: BaseIdentity,
    symbols: &'a [SymbolEntry<'a>],
}

#[test]
fn authentic_native_graph_candidate_pairs_same_batch_self_and_parallel_edges() {
    use zeppelin_embed::property_graph::storage::adjacency::{
        Direction, NativeGraphBase, NativeGraphReader, RangeScratch, prepare_native_graph,
        validate_range,
    };
    use zeppelin_embed::property_graph::wal::{
        ArtifactDescriptor, CommitState, ReferenceList, RequiredRef, WalGraphRoots,
    };
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
    let base = Empty.identity();
    let roots = GraphRoots::from_references(base.store, base.generation, [None; 8]).unwrap();
    // Like the existing ZE43 tests, this is an explicit retained-base/catalog
    // fixture. It does not claim a GraphStore lease or bootstrap publication.
    let catalog_id = ArtifactId::new(99).unwrap();
    let committed = CommitState {
        store: base.store,
        generation: base.generation,
        sequence: 100,
        graph: WalGraphRoots::default(),
        catalog: RequiredRef {
            object: ArtifactDescriptor {
                store: base.store,
                artifact: catalog_id,
                generation: base.generation,
                serial: 1,
                bytes: 200,
                family: 17,
                version: 1,
                checksum: 0,
            },
            block: PhysicalRef {
                artifact: catalog_id,
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
    };
    let catalog = Catalog { base, symbols: &[] };
    let mut objects = packed(&Missing, 1, &memory, &mut r);
    let noop = stage_structured(&Empty, &[], &writer, &mut |_| Ok(())).unwrap();
    let candidate = prepare_native_graph(
        &mut objects,
        &noop,
        NativeGraphBase {
            directories: DirectoryBase {
                identity: base,
                roots,
            },
            committed,
        },
        &catalog,
        None,
        &memory,
        &mut r,
    )
    .unwrap();
    assert_eq!(candidate.roots(), roots);
    assert_eq!(candidate.sequence(), 100);
    assert_eq!(candidate.expected_sequence(), 100);
    assert_eq!(candidate.expected_roots(), committed.graph);
    assert!(objects.is_empty());
    drop(candidate);
    let mut labels = [GraphName::new("NewLabel").unwrap()];
    let node = CanonicalContents::node(&mut labels, &mut [], None, None).unwrap();
    with_local_refs(|refs| {
        let node_write = |key| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&node)),
        };
        let rel_write = |key, target| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).unwrap()),
                target: NodeRef::Local(refs.node(target).unwrap()),
                relationship_type: GraphName::new("NewType").unwrap(),
                properties: &[],
            }),
        };
        let requests = [
            node_write("a"),
            node_write("b"),
            rel_write("ab1", 1),
            rel_write("self", 0),
            rel_write("ab2", 1),
        ];
        let batch = stage_structured(&Empty, &requests, &writer, &mut |_| Ok(())).unwrap();
        let candidate = prepare_native_graph(
            &mut objects,
            &batch,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base,
                    roots,
                },
                committed,
            },
            &catalog,
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        assert_eq!(candidate.sequence(), 101);
        assert_eq!(candidate.roots().generation(), GraphGeneration::new(1));
        assert_eq!(candidate.expected_base(), base);
        let node_a = (1u128 << 100) + 1;
        let node_b = (1u128 << 100) + 2;
        let rel = 1u128 << 110;
        let mut scratch = RangeScratch::for_prepare(&memory, &mut r).unwrap();
        for (kind, expected) in [
            (
                TreeKind::OutRanges,
                vec![
                    (node_a, 1, rel + 1, node_b),
                    (node_a, 1, rel + 2, node_a),
                    (node_a, 1, rel + 3, node_b),
                ],
            ),
            (
                TreeKind::InRanges,
                vec![
                    (node_a, 1, rel + 2, node_a),
                    (node_b, 1, rel + 1, node_a),
                    (node_b, 1, rel + 3, node_a),
                ],
            ),
        ] {
            let root = candidate.roots().directory(kind).unwrap();
            let mut cursor = DirectoryCursor::seek(&objects, root, None, &mut r).unwrap();
            let mut actual = Vec::new();
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
                for edge in range.edges() {
                    actual.push((
                        range.descriptor().key().node.get(),
                        range.descriptor().key().rel_type.get(),
                        edge.rel.get(),
                        edge.neighbor.get(),
                    ));
                }
            }
            assert_eq!(actual, expected, "actual producer direction {kind:?}");
        }
        let visible_catalog = Catalog {
            base,
            symbols: batch.symbols(),
        };
        let reader = NativeGraphReader::new(
            &objects,
            candidate.roots(),
            candidate.sequence(),
            &visible_catalog,
            None,
        );
        assert_eq!(reader.relationship_count(&mut r).unwrap(), 3);
        assert_eq!(
            reader
                .degree(
                    NodeId::new(node_a).unwrap(),
                    Direction::Out,
                    None,
                    &mut scratch,
                    &mut r
                )
                .unwrap(),
            3
        );
        assert_eq!(
            reader
                .degree(
                    NodeId::new(node_a).unwrap(),
                    Direction::In,
                    None,
                    &mut scratch,
                    &mut r
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reader
                .degree(
                    NodeId::new(node_b).unwrap(),
                    Direction::In,
                    None,
                    &mut scratch,
                    &mut r
                )
                .unwrap(),
            2
        );
        assert!(
            reader
                .relationship(RelId::new(rel + 2).unwrap(), &mut r)
                .unwrap()
                .is_some()
        );

        let first = RelId::new(rel + 1).unwrap();
        let mut output = [reader.relationship(first, &mut r).unwrap().unwrap(); 1];
        assert_eq!(reader.scan_relationships(zeppelin_embed::property_graph::storage::adjacency::RelationshipRange {lower: first, upper: zeppelin_embed::property_graph::storage::adjacency::UpperBound::Exclusive(first)}, &mut output, &mut r).unwrap(), 0, "empty read intervals are valid even though persisted ranges must be nonempty");

        // Independent root review: every public component read uses the same
        // numeric range and live-endpoint rules, before consuming capacity.
        use zeppelin_embed::property_graph::storage::adjacency::{
            AdjacencyQuery, AdjacencyRow, Edge, RelationshipRange, RelationshipRow, UpperBound,
        };
        let a = NodeId::new(node_a).unwrap();
        let b = NodeId::new(node_b).unwrap();
        let ids = [1, 2, 3].map(|n| RelId::new(rel + n).unwrap());
        let interval = RelationshipRange {
            lower: ids[1],
            upper: UpperBound::Exclusive(ids[2]),
        };
        let row = RelationshipRow {
            rel: ids[0],
            source: a,
            target: b,
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let mut rows = [row; 2];
        assert_eq!(
            reader
                .scan_relationships(interval, &mut rows, &mut r)
                .unwrap(),
            1
        );
        assert_eq!(rows[0].rel, ids[1]);
        assert_eq!(rows[0].target, a);
        let query = AdjacencyQuery {
            node: a,
            direction: Direction::Out,
            relationship_type: Some(RelTypeId::new(1).unwrap()),
            relationships: interval,
        };
        let mut edges = [AdjacencyRow {
            relationship_type: row.relationship_type,
            edge: Edge {
                rel: ids[0],
                neighbor: b,
            },
        }; 1];
        assert_eq!(
            reader
                .expand(query, &mut edges, &mut scratch, &mut r)
                .unwrap(),
            1
        );
        assert_eq!(
            edges[0].edge,
            Edge {
                rel: ids[1],
                neighbor: a
            }
        );
        assert!(
            !reader
                .has_live_incident(a, &ids, &mut scratch, &mut r)
                .unwrap()
        );
        assert!(
            reader
                .has_live_incident(a, &ids[..2], &mut scratch, &mut r)
                .unwrap()
        );
        assert!(matches!(
            reader.has_live_incident(a, &[ids[1], ids[0]], &mut scratch, &mut r),
            Err(TreeError::Invalid(_))
        ));

        // A missing required far-endpoint record must be corruption in every
        // edge-facing read; it cannot become a hidden/tombstoned edge.
        struct MissingReference<'a, S> {
            source: &'a S,
            missing: PhysicalRef,
        }
        impl<S: BlockSource> BlockSource for MissingReference<'_, S> {
            fn resolve<'b>(
                &'b self,
                reference: PhysicalRef,
                r: &mut TreeResources<'_>,
            ) -> Result<FramedBlock<'b>, TreeError> {
                if reference == self.missing {
                    Err(TreeError::Missing)
                } else {
                    self.source.resolve(reference, r)
                }
            }
        }
        let absent = record_ref(&objects, candidate.roots(), EntityId::Node(b), &mut r)
            .unwrap()
            .reference();
        {
            let damaged = MissingReference {
                source: &objects,
                missing: absent,
            };
            let corrupt = NativeGraphReader::new(
                &damaged,
                candidate.roots(),
                candidate.sequence(),
                &visible_catalog,
                None,
            );
            let all = RelationshipRange {
                lower: ids[0],
                upper: UpperBound::Infinity,
            };
            assert!(matches!(
                corrupt.relationship(ids[0], &mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.scan_relationships(all, &mut rows[..1], &mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.relationship_count(&mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.expand(
                    AdjacencyQuery {
                        relationships: all,
                        ..query
                    },
                    &mut edges,
                    &mut scratch,
                    &mut r
                ),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.degree(a, Direction::Out, None, &mut scratch, &mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.has_live_incident(a, &[], &mut scratch, &mut r),
                Err(TreeError::Missing)
            ));
            assert!(
                corrupt.relationship(ids[1], &mut r).unwrap().is_some(),
                "unrelated self-loop still resolves through the same source"
            );
        }

        // Bind a no-op participant to real produced root/object metadata. This
        // remains the explicit retained-base fixture, not GraphStore admission.
        objects.finish(&mut r).unwrap();
        let fixture = Fixture::empty();
        let next = fixture.after(&batch);
        let no_changes = stage_structured(&next, &[], &writer, &mut |_| Ok(())).unwrap();
        let next_catalog = Catalog {
            base: next.identity,
            symbols: &next.symbols,
        };
        let mut wal_roots = WalGraphRoots::default();
        let mut max_serial = 1;
        for (slot, reference) in candidate.roots().references().into_iter().enumerate() {
            if let Some(reference) = reference {
                let artifact = (0..objects.len())
                    .map(|i| objects.artifact(i).unwrap())
                    .find(|object| object.identity().artifact == reference.artifact)
                    .unwrap();
                let identity = artifact.identity();
                let bytes = artifact.bytes();
                max_serial = max_serial.max(identity.creation_serial);
                wal_roots.slots[slot] = Some(RequiredRef {
                    object: ArtifactDescriptor {
                        store: identity.store,
                        artifact: identity.artifact,
                        generation: identity.generation,
                        serial: identity.creation_serial,
                        bytes: bytes.len().try_into().unwrap(),
                        family: 17,
                        version: 1,
                        checksum: u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap()),
                    },
                    block: reference,
                });
            }
        }
        let mut actual_state = committed;
        actual_state.generation = next.identity.generation;
        actual_state.sequence = candidate.sequence();
        actual_state.graph = wal_roots;
        actual_state.high_waters.creation_serial = max_serial;
        let source_roots = candidate.roots();
        let actual_base = NativeGraphBase {
            directories: DirectoryBase {
                identity: next.identity,
                roots: source_roots,
            },
            committed: actual_state,
        };
        {
            let mut scratch_objects = packed(&objects, 2, &memory, &mut r);
            let no_op = prepare_native_graph(
                &mut scratch_objects,
                &no_changes,
                actual_base,
                &next_catalog,
                None,
                &memory,
                &mut r,
            )
            .unwrap();
            assert_eq!(no_op.sequence(), 101);
            assert_eq!(no_op.roots(), source_roots);
            assert_eq!(no_op.expected_roots(), wal_roots);
            assert!(scratch_objects.is_empty());
        }
        // Every corruption fails before creating any new private artifact.
        for case in 0..11 {
            let mut state = actual_state;
            let required = state.graph.slots[0].as_mut().unwrap();
            match case {
                0 => required.object.store = StoreInstanceId::new(2).unwrap(),
                1 => required.object.generation = GraphGeneration::new(2),
                2 => required.object.serial = max_serial + 1,
                3 => required.object.family = 18,
                4 => required.object.version = 2,
                5 => required.object.bytes = 104,
                6 => required.object.artifact = ArtifactId::new(987_654).unwrap(),
                7 => required.object.generation = GraphGeneration::new(0),
                8 => required.object.serial = 1,
                9 => state.graph.slots.swap(0, 1),
                10 => state.graph.slots[0] = None,
                _ => unreachable!(),
            }
            let mut scratch_objects = packed(&objects, 2, &memory, &mut r);
            let result = prepare_native_graph(
                &mut scratch_objects,
                &no_changes,
                NativeGraphBase {
                    committed: state,
                    ..actual_base
                },
                &next_catalog,
                None,
                &memory,
                &mut r,
            );
            assert!(
                result.is_err(),
                "WAL/source binding corruption {case} was admitted"
            );
            assert!(
                scratch_objects.is_empty(),
                "failed base binding wrote artifacts {case}"
            );
        }

        // Even matching root/WAL metadata cannot substitute a partial physical
        // reference: the immutable source must recognize every exact field.
        for offset_changed in [true, false] {
            let mut state = actual_state;
            let required = state.graph.slots[0].as_mut().unwrap();
            if offset_changed {
                required.block.offset += 1;
            } else {
                required.block.length += 1;
            }
            let mut references = source_roots.references();
            references[0] = Some(required.block);
            let altered_roots = GraphRoots::from_references(
                source_roots.store(),
                source_roots.generation(),
                references,
            )
            .unwrap();
            let mut scratch_objects = packed(&objects, 2, &memory, &mut r);
            let result = prepare_native_graph(
                &mut scratch_objects,
                &no_changes,
                NativeGraphBase {
                    directories: DirectoryBase {
                        roots: altered_roots,
                        ..actual_base.directories
                    },
                    committed: state,
                },
                &next_catalog,
                None,
                &memory,
                &mut r,
            );
            assert!(
                result.is_err(),
                "altered full reference (offset={offset_changed}) must fail"
            );
            assert!(scratch_objects.is_empty());
        }

        // A logically absent endpoint entry is also corruption, even while
        // every referenced artifact exists and its framing is valid.
        {
            let mut missing_objects = packed(&objects, 2, &memory, &mut r);
            let mut tree = TreeScratch::for_prepare(&memory).unwrap();
            let missing_nodes = remove(
                &mut missing_objects,
                source_roots.directory(TreeKind::Nodes).unwrap(),
                &b.get().to_le_bytes(),
                GraphGeneration::new(2),
                &mut tree,
                &mut r,
            )
            .unwrap();
            let mut missing_roots = source_roots
                .for_generation(GraphGeneration::new(2))
                .unwrap();
            missing_roots.replace(missing_nodes).unwrap();
            let corrupt =
                NativeGraphReader::new(&missing_objects, missing_roots, 102, &next_catalog, None);
            let all = RelationshipRange {
                lower: ids[0],
                upper: UpperBound::Infinity,
            };
            assert!(
                matches!(
                    corrupt.relationship(ids[0], &mut r),
                    Err(TreeError::Missing)
                ),
                "a missing node entry must not hide an extant relationship"
            );
            assert!(matches!(
                corrupt.scan_relationships(all, &mut rows[..1], &mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.relationship_count(&mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.expand(
                    AdjacencyQuery {
                        relationships: all,
                        ..query
                    },
                    &mut edges,
                    &mut scratch,
                    &mut r
                ),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.degree(a, Direction::Out, None, &mut scratch, &mut r),
                Err(TreeError::Missing)
            ));
            assert!(matches!(
                corrupt.has_live_incident(a, &[], &mut scratch, &mut r),
                Err(TreeError::Missing)
            ));
        }
        {
            let deletion = [StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Delete(EntityId::Node(b), GraphDeleteMode::Detach),
                image: None,
            }];
            let staged = stage_structured(&next, &deletion, &writer, &mut |_| Ok(())).unwrap();
            assert_eq!(next.incidents.get(), 0, "DETACH must not enumerate");
            let mut tombstone_objects = packed(&objects, 2, &memory, &mut r);
            let detached = prepare_native_graph(
                &mut tombstone_objects,
                &staged,
                actual_base,
                &next_catalog,
                None,
                &memory,
                &mut r,
            )
            .unwrap();
            for kind in [
                TreeKind::Relationships,
                TreeKind::OutRanges,
                TreeKind::InRanges,
            ] {
                assert_eq!(
                    detached.roots().directory(kind).unwrap().reference(),
                    source_roots.directory(kind).unwrap().reference(),
                    "DETACH preserves raw {kind:?}"
                );
            }
            let filtered = NativeGraphReader::new(
                &tombstone_objects,
                detached.roots(),
                detached.sequence(),
                &next_catalog,
                None,
            );
            let all = RelationshipRange {
                lower: ids[0],
                upper: UpperBound::Infinity,
            };
            assert!(filtered.relationship(ids[0], &mut r).unwrap().is_none());
            assert_eq!(filtered.relationship_count(&mut r).unwrap(), 1);
            assert_eq!(
                filtered
                    .scan_relationships(all, &mut rows[..1], &mut r)
                    .unwrap(),
                1
            );
            assert_eq!(
                rows[0].rel, ids[1],
                "dead first row must not consume capacity"
            );
            assert_eq!(
                filtered
                    .expand(
                        AdjacencyQuery {
                            relationships: all,
                            ..query
                        },
                        &mut edges,
                        &mut scratch,
                        &mut r
                    )
                    .unwrap(),
                1
            );
            assert_eq!(edges[0].edge.rel, ids[1]);
            assert_eq!(
                filtered
                    .degree(a, Direction::Out, None, &mut scratch, &mut r)
                    .unwrap(),
                1
            );
            assert_eq!(
                filtered
                    .degree(b, Direction::In, None, &mut scratch, &mut r)
                    .unwrap(),
                0
            );
            assert!(
                !filtered
                    .has_live_incident(b, &[], &mut scratch, &mut r)
                    .unwrap()
            );
            assert_eq!(
                NativeGraphReader::new(&objects, source_roots, 101, &next_catalog, None)
                    .relationship_count(&mut r)
                    .unwrap(),
                3,
                "old retained roots remain live"
            );
        }

        let empty = RelationshipRange {
            lower: ids[1],
            upper: UpperBound::Exclusive(ids[1]),
        };
        let old_reader = NativeGraphReader::new(&objects, source_roots, 101, &next_catalog, None);
        assert_eq!(
            old_reader
                .scan_relationships(empty, &mut rows, &mut r)
                .unwrap(),
            0,
            "empty half-open scan interval"
        );
        assert_eq!(
            old_reader
                .expand(
                    AdjacencyQuery {
                        relationships: empty,
                        ..query
                    },
                    &mut edges,
                    &mut scratch,
                    &mut r
                )
                .unwrap(),
            0,
            "empty half-open expand interval"
        );
        let reversed = RelationshipRange {
            lower: ids[1],
            upper: UpperBound::Exclusive(ids[0]),
        };
        assert!(matches!(
            old_reader.scan_relationships(reversed, &mut rows, &mut r),
            Err(TreeError::Invalid(_))
        ));
        assert!(matches!(
            old_reader.expand(
                AdjacencyQuery {
                    relationships: reversed,
                    ..query
                },
                &mut edges,
                &mut scratch,
                &mut r
            ),
            Err(TreeError::Invalid(_))
        ));
        assert!(memory.peak_reserved_bytes() <= 32 * 1024 * 1024);
    });
}
impl<S: BlockSource> RecordCatalog<S> for Catalog<'_> {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        for entry in self.symbols {
            r.step(1)?;
            if entry.symbol.kind() == kind
                && name
                    .compare_bytes(entry.name.as_str().as_bytes(), r)?
                    .is_eq()
            {
                return Ok(entry.symbol);
            }
        }
        Err(TreeError::Invalid("unknown test catalog name"))
    }
}
impl<S: BlockSource> PreparationCatalog<S> for Catalog<'_> {
    fn base_identity(&self) -> BaseIdentity {
        self.base
    }
}
#[test]
fn authentic_staged_batch_prepares_native_directories_inside_one_memory_owner() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let shared_before = shared.reserved_bytes().unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 30_000_000).unwrap();
    let mut labels = [
        GraphName::new("Second").unwrap(),
        GraphName::new("First").unwrap(),
    ];
    let mut properties = [GraphProperty::new(
        GraphName::new("bits").unwrap(),
        PropertyValue::new(PropertyData::F64(f64::from_bits(0x7ff8_0000_0000_4321))).unwrap(),
    )];
    let image = CanonicalContents::node(&mut labels, &mut properties, Some(""), None).unwrap();
    with_local_refs(|refs| {
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "n").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "r").unwrap(),
                revision: GraphRevision::new(3).unwrap(),
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
        let roots = GraphRoots::from_references(base.store, base.generation, [None; 8]).unwrap();
        let mut serial = 0;
        let identity = || {
            serial += 1;
            Ok(ArtifactIdentity {
                store: base.store,
                artifact: ArtifactId::new(serial as u128).unwrap(),
                generation: GraphGeneration::new(1),
                creation_serial: serial,
            })
        };
        let mut objects = PreparedObjects::new(
            &Missing,
            identity,
            base.store,
            GraphGeneration::new(1),
            PackLimits {
                artifact_bytes: 512 * 1024,
                blocks: 256,
            },
            &memory,
            &mut r,
        )
        .unwrap();
        let catalog = Catalog { base, symbols: &[] };
        let wrong = DirectoryBase {
            identity: BaseIdentity {
                roots: Some(ArtifactId::new(77).unwrap()),
                ..base
            },
            roots,
        };
        assert!(
            prepare_directories(&mut objects, &batch, wrong, &catalog, None, &memory, &mut r)
                .is_err()
        );
        assert_eq!(
            objects.len(),
            0,
            "wrong exact base token fails before any private append"
        );
        let foreign_writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let foreign = StorageMemory::new(&foreign_writer, &control, 32 * 1024 * 1024).unwrap();
        let mut foreign_r = TreeResources::for_prepare(&foreign, 30_000_000).unwrap();
        assert!(
            prepare_directories(
                &mut objects,
                &batch,
                DirectoryBase {
                    identity: base,
                    roots
                },
                &catalog,
                None,
                &foreign,
                &mut foreign_r
            )
            .is_err()
        );
        assert_eq!(
            objects.len(),
            0,
            "same aggregate store is insufficient without same writer owner"
        );
        let candidate = prepare_directories(
            &mut objects,
            &batch,
            DirectoryBase {
                identity: base,
                roots,
            },
            &catalog,
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        assert_eq!(candidate.expected_base(), base);
        assert_eq!(candidate.roots().generation(), GraphGeneration::new(1));
        let complete_catalog = Catalog {
            base,
            symbols: batch.symbols(),
        };
        for (index, kind) in [(0, TreeKind::Nodes), (1, TreeKind::Relationships)] {
            let id = batch.receipts()[index].entity;
            let raw = match id {
                EntityId::Node(id) => id.get(),
                EntityId::Relationship(id) => id.get(),
            };
            let root = candidate.roots().directory(kind).unwrap();
            let entry = lookup_entry(&objects, root, &raw.to_le_bytes(), &mut r)
                .unwrap()
                .unwrap();
            let reference =
                zeppelin_embed::property_graph::storage::payload::PayloadRef::decode(entry.value())
                    .unwrap();
            let record = verify_record(
                PayloadSlice::new(&objects, base.store, entry.creation_generation(), reference),
                id,
                &complete_catalog,
                None,
                &mut r,
            )
            .unwrap();
            assert_eq!(record.revision(), batch.receipts()[index].revision);
            assert_eq!(
                record
                    .provenance()
                    .fields_with_key(Some(requests[index].key), &mut r)
                    .unwrap(),
                batch.deltas()[index].provenance().fields()
            );
        }
        for (kind, count) in [
            (TreeKind::Nodes, 1),
            (TreeKind::Relationships, 1),
            (TreeKind::Labels, 2),
            (TreeKind::RelationshipTypes, 1),
            (TreeKind::KeyFences, 2),
            (TreeKind::OutRanges, 0),
            (TreeKind::InRanges, 0),
        ] {
            let count_found = verify_directory(
                &objects,
                candidate.roots().directory(kind).unwrap(),
                &mut r,
                &mut |entry, _| {
                    if matches!(kind, TreeKind::Labels | TreeKind::RelationshipTypes) {
                        assert!(entry.value().is_empty());
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(count_found, count, "native directory {kind:?}");
        }
        assert_eq!(
            shared.reserved_bytes().unwrap() - shared_before,
            (writer.reserved_bytes() + foreign_writer.reserved_bytes()) as u64,
            "authentic single aggregate charges all overlapping owners"
        );
        assert!(memory.peak_reserved_bytes() <= 32 * 1024 * 1024);
        objects.finish(&mut r).unwrap();
        assert!(objects.artifact(0).is_ok());
    });
}

#[derive(Clone)]
struct FixtureEntry<'a> {
    provenance: OperationProvenance<'a>,
    shape: Option<EntityShape<'a>>,
    canonical: Option<Vec<u8>>,
    membership: Membership,
}
impl CanonicalSource for FixtureEntry<'_> {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        let bytes = self
            .canonical
            .as_ref()
            .ok_or(std::io::ErrorKind::InvalidData)?;
        let bytes = bytes
            .get(offset as usize..)
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let n = bytes.len().min(output.len());
        output[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }
}
impl FixtureEntry<'_> {
    fn live(&self, view: BaseIdentity) -> Option<BaseEntity<'_>> {
        let bytes = self.canonical.as_ref()?;
        Some(BaseEntity {
            view,
            provenance: self.provenance,
            shape: self.shape?,
            fingerprint: CanonicalFingerprint::new(
                bytes.len() as u64,
                xxhash_rust::xxh3::xxh3_64(bytes),
            )
            .unwrap(),
            source: self,
            membership: self.membership,
        })
    }
}
struct Fixture<'a> {
    identity: BaseIdentity,
    high: HighWaters,
    entries: Vec<FixtureEntry<'a>>,
    symbols: Vec<SymbolEntry<'a>>,
    incidents: std::cell::Cell<usize>,
}
impl<'a> Fixture<'a> {
    fn empty() -> Self {
        Self {
            identity: Empty.identity(),
            high: Empty.high_waters(),
            entries: Vec::new(),
            symbols: Vec::new(),
            incidents: std::cell::Cell::new(0),
        }
    }
    fn after<'b>(&'b self, batch: &'b StagedBatch<'b>) -> Fixture<'b> {
        let mut entries = self.entries.clone();
        for delta in batch.deltas() {
            let entry = FixtureEntry {
                provenance: delta.provenance(),
                shape: delta.shape(),
                canonical: delta.canonical().map(<[u8]>::to_vec),
                membership: delta.membership().1,
            };
            if let Some(old) = entries.iter_mut().find(|old| {
                old.provenance.fields().incarnation == entry.provenance.fields().incarnation
            }) {
                *old = entry;
            } else {
                entries.push(entry);
            }
        }
        let mut symbols = self.symbols.clone();
        symbols.extend_from_slice(batch.symbols());
        let generation = GraphGeneration::new(self.identity.generation.get() + 1);
        Fixture {
            identity: BaseIdentity {
                generation,
                roots: Some(ArtifactId::new(50_000 + generation.get() as u128).unwrap()),
                ..self.identity
            },
            high: batch.high_waters(),
            entries,
            symbols,
            incidents: std::cell::Cell::new(0),
        }
    }
}
impl AdmittedBase for Fixture<'_> {
    fn identity(&self) -> BaseIdentity {
        self.identity
    }
    fn high_waters(&self) -> HighWaters {
        self.high
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        Empty.interpretation()
    }
    fn key(
        &self,
        key: ApplicationKey<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        Ok(
            match self
                .entries
                .iter()
                .rev()
                .find(|entry| entry.provenance.fields().key == Some(key))
            {
                Some(entry) => match entry.live(self.identity) {
                    Some(live) => BaseKeyState::Live(live),
                    None => BaseKeyState::Deleted(self.identity, entry.provenance),
                },
                None => BaseKeyState::NeverUsed,
            },
        )
    }
    fn entity(
        &self,
        id: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        Ok(self
            .entries
            .iter()
            .find(|entry| entry.provenance.fields().incarnation == id)
            .and_then(|entry| entry.live(self.identity)))
    }
    fn has_live_incident(
        &self,
        node: NodeId,
        removed: &[RelId],
        _: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        self.incidents.set(self.incidents.get() + 1);
        let live = |id: NodeId| {
            self.entries.iter().any(|entry| {
                entry.provenance.fields().incarnation == EntityId::Node(id)
                    && entry.canonical.is_some()
            })
        };
        Ok(self.entries.iter().any(|entry| {
            match (entry.provenance.fields().incarnation, entry.shape) {
                (
                    EntityId::Relationship(id),
                    Some(EntityShape::Relationship { source, target, .. }),
                ) => {
                    !removed.contains(&id)
                        && live(source)
                        && live(target)
                        && (source == node || target == node)
                }
                _ => false,
            }
        }))
    }
    fn property(
        &self,
        _: EntityId,
        _: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        Ok(None)
    }
    fn stored_text(&self, _: NodeId, _: &mut WriteControl<'_>) -> Result<Option<&str>, StageError> {
        Ok(None)
    }
    fn symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        Ok(self
            .symbols
            .iter()
            .find(|entry| entry.symbol.kind() == kind && entry.name == name)
            .map(|entry| entry.symbol))
    }
}
fn packed<'a, 'b, S: BlockSource>(
    base: &'b S,
    generation: u64,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> PreparedObjects<'a, 'b, S, impl FnMut() -> Result<ArtifactIdentity, TreeError> + use<S>> {
    let mut serial = generation * 1000;
    PreparedObjects::new(
        base,
        move || {
            serial += 1;
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
fn record_ref(
    source: &impl BlockSource,
    roots: GraphRoots,
    entity: EntityId,
    r: &mut TreeResources<'_>,
) -> Option<zeppelin_embed::property_graph::storage::payload::PayloadRef> {
    let (kind, id) = match entity {
        EntityId::Node(id) => (TreeKind::Nodes, id.get()),
        EntityId::Relationship(id) => (TreeKind::Relationships, id.get()),
    };
    lookup_entry(source, roots.directory(kind).unwrap(), &id.to_le_bytes(), r)
        .unwrap()
        .map(|entry| {
            zeppelin_embed::property_graph::storage::payload::PayloadRef::decode(entry.value())
                .unwrap()
        })
}
fn rows(
    source: &impl BlockSource,
    roots: GraphRoots,
    kind: TreeKind,
    r: &mut TreeResources<'_>,
) -> u64 {
    verify_directory(
        source,
        roots.directory(kind).unwrap(),
        r,
        &mut |_, _| Ok(()),
    )
    .unwrap()
}
#[test]
fn staged_updates_detach_delete_and_recreate_preserve_old_roots_and_permanent_fences() {
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
    let mut first_labels = [GraphName::new("L1").unwrap(), GraphName::new("L2").unwrap()];
    let first_image =
        CanonicalContents::node(&mut first_labels, &mut [], Some("old"), None).unwrap();
    let mut second_labels = [GraphName::new("L2").unwrap()];
    let second_image = CanonicalContents::node(&mut second_labels, &mut [], None, None).unwrap();
    let mut next_labels = [GraphName::new("L2").unwrap(), GraphName::new("L3").unwrap()];
    let next_image = CanonicalContents::node(&mut next_labels, &mut [], Some("new"), None).unwrap();
    let key_a = ApplicationKey::new(EntityKind::Node, "app", "a").unwrap();
    let key_b = ApplicationKey::new(EntityKind::Node, "app", "b").unwrap();
    let key_r = ApplicationKey::new(EntityKind::Relationship, "app", "r").unwrap();
    let empty = Fixture::empty();
    with_local_refs(|refs| {
        let requests = [
            StructuredWrite {
                key: key_a,
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first_image)),
            },
            StructuredWrite {
                key: key_b,
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second_image)),
            },
            StructuredWrite {
                key: key_r,
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                }),
            },
        ];
        let batch1 = stage_structured(&empty, &requests, &writer, &mut |_| Ok(())).unwrap();
        let base1 = empty.after(&batch1);
        let a = batch1.receipts()[0].entity;
        let b = batch1.receipts()[1].entity;
        let rel = batch1.receipts()[2].entity;
        let roots0 =
            GraphRoots::from_references(empty.identity.store, GraphGeneration::new(0), [None; 8])
                .unwrap();
        let mut objects1 = packed(&Missing, 1, &memory, &mut r);
        let candidate1 = prepare_directories(
            &mut objects1,
            &batch1,
            DirectoryBase {
                identity: empty.identity,
                roots: roots0,
            },
            &Catalog {
                base: empty.identity,
                symbols: &empty.symbols,
            },
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        let roots1 = candidate1.roots();
        objects1.finish(&mut r).unwrap();
        let untouched_b = record_ref(&objects1, roots1, b, &mut r).unwrap();
        let untouched_rel = record_ref(&objects1, roots1, rel, &mut r).unwrap();
        let requests2 = [StructuredWrite {
            key: key_a,
            revision: GraphRevision::new(2).unwrap(),
            operation: StructuredOperation::Put(a),
            image: Some(WriteImage::Node(&next_image)),
        }];
        let batch2 = stage_structured(&base1, &requests2, &writer, &mut |_| Ok(())).unwrap();
        let base2 = base1.after(&batch2);
        let mut objects2 = packed(&objects1, 2, &memory, &mut r);
        let candidate2 = prepare_directories(
            &mut objects2,
            &batch2,
            DirectoryBase {
                identity: base1.identity,
                roots: roots1,
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
        let roots2 = candidate2.roots();
        objects2.finish(&mut r).unwrap();
        assert_eq!(record_ref(&objects2, roots2, b, &mut r), Some(untouched_b));
        assert_eq!(
            record_ref(&objects2, roots2, rel, &mut r),
            Some(untouched_rel)
        );
        assert_eq!(rows(&objects2, roots2, TreeKind::Labels, &mut r), 3);
        let labels = roots2.directory(TreeKind::Labels).unwrap();
        let a_id = match a {
            EntityId::Node(id) => id,
            _ => unreachable!(),
        };
        for (name, present) in [("L1", false), ("L2", true), ("L3", true)] {
            let symbol = base2
                .symbols
                .iter()
                .find(|entry| entry.name.as_str() == name)
                .unwrap()
                .symbol
                .get();
            let mut key = symbol.to_le_bytes().to_vec();
            key.extend_from_slice(&a_id.get().to_le_bytes());
            assert_eq!(
                lookup_entry(&objects2, labels, &key, &mut r)
                    .unwrap()
                    .is_some(),
                present,
                "membership {name}"
            );
        }
        let requests3 = [StructuredWrite {
            key: key_a,
            revision: GraphRevision::new(3).unwrap(),
            operation: StructuredOperation::Delete(a, GraphDeleteMode::Detach),
            image: None,
        }];
        let batch3 = stage_structured(&base2, &requests3, &writer, &mut |_| Ok(())).unwrap();
        assert_eq!(
            base2.incidents.get(),
            0,
            "DETACH does not enumerate incident relationships"
        );
        let base3 = base2.after(&batch3);
        let mut objects3 = packed(&objects2, 3, &memory, &mut r);
        let candidate3 = prepare_directories(
            &mut objects3,
            &batch3,
            DirectoryBase {
                identity: base2.identity,
                roots: roots2,
            },
            &Catalog {
                base: base2.identity,
                symbols: &base2.symbols,
            },
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        let roots3 = candidate3.roots();
        objects3.finish(&mut r).unwrap();
        assert_eq!(
            record_ref(&objects3, roots3, rel, &mut r),
            Some(untouched_rel),
            "lazy DETACH retains raw relationship record"
        );
        assert_eq!(
            roots3
                .directory(TreeKind::RelationshipTypes)
                .unwrap()
                .reference(),
            roots2
                .directory(TreeKind::RelationshipTypes)
                .unwrap()
                .reference()
        );
        assert_eq!(rows(&objects3, roots3, TreeKind::Labels, &mut r), 1);
        let dead = record_ref(&objects3, roots3, a, &mut r).unwrap();
        let tombstone = verify_node_tombstone(
            PayloadSlice::new(&objects3, base3.identity.store, roots3.generation(), dead),
            a_id,
            &mut r,
        )
        .unwrap();
        assert_eq!(
            tombstone
                .provenance()
                .fields_with_key(Some(key_a), &mut r)
                .unwrap(),
            batch3.deltas()[0].provenance().fields()
        );
        let old = record_ref(&objects3, roots1, a, &mut r).unwrap();
        let record = verify_record(
            PayloadSlice::new(&objects3, base3.identity.store, roots1.generation(), old),
            a,
            &Catalog {
                base: empty.identity,
                symbols: &base1.symbols,
            },
            None,
            &mut r,
        )
        .unwrap();
        assert_eq!(record.revision().get(), 1);
        assert_eq!(
            record
                .canonical()
                .stored_text()
                .unwrap()
                .compare_bytes(b"old", &mut r)
                .unwrap(),
            std::cmp::Ordering::Equal
        );
        let requests4 = [
            StructuredWrite {
                key: key_r,
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Delete(rel, GraphDeleteMode::Restrict),
                image: None,
            },
            StructuredWrite {
                key: key_b,
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Delete(b, GraphDeleteMode::Restrict),
                image: None,
            },
        ];
        let batch4 = stage_structured(&base3, &requests4, &writer, &mut |_| Ok(())).unwrap();
        let base4 = base3.after(&batch4);
        let mut objects4 = packed(&objects3, 4, &memory, &mut r);
        let candidate4 = prepare_directories(
            &mut objects4,
            &batch4,
            DirectoryBase {
                identity: base3.identity,
                roots: roots3,
            },
            &Catalog {
                base: base3.identity,
                symbols: &base3.symbols,
            },
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        let roots4 = candidate4.roots();
        objects4.finish(&mut r).unwrap();
        assert_eq!(rows(&objects4, roots4, TreeKind::Relationships, &mut r), 0);
        assert_eq!(rows(&objects4, roots4, TreeKind::Labels, &mut r), 0);
        assert_eq!(
            rows(&objects4, roots4, TreeKind::RelationshipTypes, &mut r),
            0
        );
        assert_eq!(
            rows(&objects4, roots4, TreeKind::KeyFences, &mut r),
            3,
            "all-live deletion retains permanent keyed evidence"
        );
        let fence_root = roots4.directory(TreeKind::KeyFences).unwrap();
        verify_directory(&objects4, fence_root, &mut r, &mut |entry, r| {
            let fence = verify_fence_entry(
                &objects4,
                fence_root,
                entry,
                &Catalog {
                    base: base4.identity,
                    symbols: &base4.symbols,
                },
                None,
                r,
            )?;
            assert!(fence.is_deleted());
            assert!(fence.canonical_bytes().is_none());
            Ok(())
        })
        .unwrap();
        let requests5 = [StructuredWrite {
            key: key_a,
            revision: GraphRevision::new(4).unwrap(),
            operation: StructuredOperation::Recreate(GraphRevision::new(3).unwrap()),
            image: Some(WriteImage::Node(&first_image)),
        }];
        let batch5 = stage_structured(&base4, &requests5, &writer, &mut |_| Ok(())).unwrap();
        let mut objects5 = packed(&objects4, 5, &memory, &mut r);
        let candidate5 = prepare_directories(
            &mut objects5,
            &batch5,
            DirectoryBase {
                identity: base4.identity,
                roots: roots4,
            },
            &Catalog {
                base: base4.identity,
                symbols: &base4.symbols,
            },
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        let roots5 = candidate5.roots();
        objects5.finish(&mut r).unwrap();
        let fresh = batch5.receipts()[0].entity;
        assert_ne!(fresh, a, "recreation never recycles an old incarnation");
        assert!(record_ref(&objects5, roots5, fresh, &mut r).is_some());
        assert_eq!(
            record_ref(&objects5, roots5, a, &mut r),
            Some(dead),
            "physical tombstone retained until later incident sweep"
        );
        assert_eq!(rows(&objects5, roots5, TreeKind::KeyFences, &mut r), 3);
        assert_eq!(
            rows(&objects5, roots4, TreeKind::Labels, &mut r),
            0,
            "old generation is still all-deleted after recreation"
        );
        // Fresh physical files and a source containing only newly read buffers.
        // This component test does not claim fsync/WAL/public GraphStore recovery.
        let files = tempfile::tempdir().unwrap();
        let count =
            objects1.len() + objects2.len() + objects3.len() + objects4.len() + objects5.len();
        let mut reopened =
            zeppelin_embed::property_graph::storage::memory::StorageBuffer::new(&memory, count)
                .unwrap();
        reopen_objects(&objects1, files.path(), &mut reopened, &memory, &mut r);
        reopen_objects(&objects2, files.path(), &mut reopened, &memory, &mut r);
        reopen_objects(&objects3, files.path(), &mut reopened, &memory, &mut r);
        reopen_objects(&objects4, files.path(), &mut reopened, &memory, &mut r);
        reopen_objects(&objects5, files.path(), &mut reopened, &memory, &mut r);
        let reopened = Reopened(reopened);
        drop(objects5);
        drop(objects4);
        drop(objects3);
        drop(objects2);
        drop(objects1);
        assert_eq!(rows(&reopened, roots1, TreeKind::Relationships, &mut r), 1);
        assert_eq!(rows(&reopened, roots4, TreeKind::Relationships, &mut r), 0);
        assert_eq!(rows(&reopened, roots4, TreeKind::Labels, &mut r), 0);
        assert_eq!(rows(&reopened, roots5, TreeKind::Labels, &mut r), 2);
        assert!(record_ref(&reopened, roots5, fresh, &mut r).is_some());
        let old = record_ref(&reopened, roots1, a, &mut r).unwrap();
        let old_record = verify_record(
            PayloadSlice::new(&reopened, base1.identity.store, roots1.generation(), old),
            a,
            &Catalog {
                base: empty.identity,
                symbols: &base1.symbols,
            },
            None,
            &mut r,
        )
        .unwrap();
        assert_eq!(
            old_record
                .canonical()
                .stored_text()
                .unwrap()
                .compare_bytes(b"old", &mut r)
                .unwrap(),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            old_record.provenance().original_generation(),
            GraphGeneration::new(1)
        );
        assert!(memory.peak_reserved_bytes() <= 32 * 1024 * 1024);
    });
}

struct Reopened<'a>(
    zeppelin_embed::property_graph::storage::memory::StorageBuffer<
        'a,
        zeppelin_embed::property_graph::storage::artifact::OwnedArtifact<'a>,
    >,
);
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
fn reopen_objects<'a, S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>>(
    objects: &PreparedObjects<'_, '_, S, F>,
    path: &std::path::Path,
    output: &mut zeppelin_embed::property_graph::storage::memory::StorageBuffer<
        'a,
        zeppelin_embed::property_graph::storage::artifact::OwnedArtifact<'a>,
    >,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) {
    for index in 0..objects.len() {
        let object = objects.artifact(index).unwrap();
        let file = path.join(format!("{}.graph", object.identity().artifact.get()));
        std::fs::write(&file, object.bytes()).unwrap();
        let admitted = zeppelin_embed::property_graph::storage::artifact::OwnedArtifact::read_from(
            &mut std::fs::File::open(file).unwrap(),
            object.bytes().len(),
            (object.identity().store, object.identity().artifact),
            memory,
            r,
        )
        .unwrap();
        output.push(admitted).unwrap();
    }
}

#[cfg(feature = "allocation-audit")]
#[test]
fn native_participant_accounts_all_allocations_and_releases_each_failed_attempt() {
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
    let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
    let mut labels = [GraphName::new("L").unwrap()];
    let mut props = [GraphProperty::new(
        GraphName::new("k").unwrap(),
        PropertyValue::new(PropertyData::I64(i64::MIN)).unwrap(),
    )];
    let image = CanonicalContents::node(&mut labels, &mut props, Some(""), None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "n").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let batch = stage_structured(&Empty, &requests, &writer, &mut |_| Ok(())).unwrap();
    let base = DirectoryBase {
        identity: Empty.identity(),
        roots: GraphRoots::from_references(
            Empty.identity().store,
            GraphGeneration::new(0),
            [None; 8],
        )
        .unwrap(),
    };
    let catalog = Catalog {
        base: Empty.identity(),
        symbols: &[],
    };
    let before = memory.reserved_bytes();
    let allocations = {
        let mut objects = packed(&Missing, 1, &memory, &mut r);
        let (candidate, audit) = audit_engine_path(|| {
            prepare_directories(&mut objects, &batch, base, &catalog, None, &memory, &mut r)
        });
        let candidate = candidate.unwrap();
        assert_eq!(
            audit.unattributed_bytes, 0,
            "entire actual native participant needs charged backing"
        );
        assert!(audit.allocations > 0);
        assert!(audit.attributed_bytes > 0);
        assert_eq!(
            rows(&objects, candidate.roots(), TreeKind::Nodes, &mut r),
            1
        );
        audit.allocations
    };
    assert_eq!(memory.reserved_bytes(), before);
    for ordinal in 1..=allocations {
        {
            let mut objects = packed(&Missing, 1, &memory, &mut r);
            let (result, fires) = fail_attributed_allocation(ordinal, || {
                prepare_directories(&mut objects, &batch, base, &catalog, None, &memory, &mut r)
            });
            assert_eq!(
                fires, 1,
                "actual allocation refusal {ordinal}/{allocations} must fire"
            );
            assert!(
                matches!(result, Err(TreeError::Memory)),
                "no candidate after refused allocation {ordinal}"
            );
            drop(result);
            for identity in objects.abort_inventory() {
                assert_eq!(identity.store, base.identity.store);
                assert_eq!(identity.generation, GraphGeneration::new(1));
            }
            assert!(objects.artifact(0).is_err());
        }
        assert_eq!(
            memory.reserved_bytes(),
            before,
            "allocation failure {ordinal} releases every owner after abort backing drops"
        );
    }
    let mut clean = packed(&Missing, 1, &memory, &mut r);
    let candidate =
        prepare_directories(&mut clean, &batch, base, &catalog, None, &memory, &mut r).unwrap();
    clean.finish(&mut r).unwrap();
    assert_eq!(rows(&clean, candidate.roots(), TreeKind::Nodes, &mut r), 1);
    println!(
        "ZE43 native allocations={allocations} refusal_fires={allocations} unattributed_bytes=0 participant_peak={}",
        memory.peak_reserved_bytes()
    );
}

#[test]
fn native_index_preparation_sorts_chunk_spanning_arrays_in_the_shared_participant() {
    use zeppelin_embed::property_graph::storage::payload::prepare_payload;
    const COUNT: usize = 2048;
    struct ReverseCatalog;
    impl<S: BlockSource> RecordCatalog<S> for ReverseCatalog {
        fn resolve(
            &self,
            kind: SymbolKind,
            name: PayloadSlice<'_, S>,
            r: &mut TreeResources<'_>,
        ) -> Result<Symbol, TreeError> {
            let mut bytes = [0; 5];
            if name.len() != 5 || name.read_at(0, &mut bytes, r)? != 5 || bytes[0] != b'n' {
                return Err(TreeError::Invalid("fixture symbol name"));
            }
            let ordinal = std::str::from_utf8(&bytes[1..])
                .unwrap()
                .parse::<usize>()
                .unwrap();
            Symbol::new(kind, (COUNT - ordinal) as u64)
                .map_err(|_| TreeError::Invalid("fixture symbol"))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let baseline = shared.reserved_bytes().unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    {
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let mut r = TreeResources::for_prepare(&memory, 200_000_000).unwrap();
        let names: Vec<_> = (0..COUNT).map(|index| format!("n{index:04}")).collect();
        let mut labels: Vec<_> = names
            .iter()
            .rev()
            .map(|name| GraphName::new(name).unwrap())
            .collect();
        let mut properties: Vec<_> = names
            .iter()
            .enumerate()
            .rev()
            .map(|(index, name)| {
                GraphProperty::new(
                    GraphName::new(name).unwrap(),
                    PropertyValue::new(PropertyData::I64(index as i64)).unwrap(),
                )
            })
            .collect();
        let canonical = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
        let mut bytes = Vec::new();
        canonical.write_to(&mut bytes, &mut || Ok(())).unwrap();
        assert!(bytes.len() > 65_536);
        let mut objects = packed(&Missing, 1, &memory, &mut r);
        let canonical_ref = prepare_payload(
            &mut objects,
            Empty.identity().store,
            GraphGeneration::new(1),
            BlockKind::CanonicalImage,
            &bytes,
            &mut r,
        )
        .unwrap();
        let id = NodeId::new((1u128 << 120) + 31).unwrap();
        let provenance = OperationProvenance::from_fields(
            Some(1),
            OperationFields {
                operation: GraphOperation::CypherEdit,
                key: None,
                requested_revision: GraphRevision::new(1).unwrap(),
                installed_revision: GraphRevision::new(1).unwrap(),
                expected: ExpectedGraphState::Absent,
                incarnation: EntityId::Node(id),
                delete_mode: None,
                original_generation: GraphGeneration::new(1),
            },
        )
        .unwrap();
        let provenance_ref = prepare_provenance(
            &mut objects,
            Empty.identity().store,
            GraphGeneration::new(1),
            provenance,
            &memory,
            &mut r,
        )
        .unwrap();
        let before = memory.reserved_bytes();
        let reference = prepare_record(
            &mut objects,
            RecordInput {
                store: Empty.identity().store,
                generation: GraphGeneration::new(1),
                entity: EntityId::Node(id),
                canonical: canonical_ref,
                provenance: provenance_ref,
            },
            &ReverseCatalog,
            None,
            &memory,
            &mut r,
        )
        .unwrap();
        assert!(reference.len() > 65_536);
        assert_eq!(
            memory.reserved_bytes(),
            before,
            "charged temporary index arrays released while pack remains"
        );
        assert!(memory.peak_reserved_bytes() >= before + COUNT * (8 + 24));
        let view = verify_record(
            PayloadSlice::new(
                &objects,
                Empty.identity().store,
                GraphGeneration::new(1),
                reference,
            ),
            EntityId::Node(id),
            &ReverseCatalog,
            None,
            &mut r,
        )
        .unwrap();
        assert_eq!(
            view.canonical_bytes()
                .compare_bytes(&bytes, &mut r)
                .unwrap(),
            std::cmp::Ordering::Equal
        );
        for index in 0..COUNT {
            assert_eq!(
                view.label(index as u32, &mut r).unwrap().get(),
                index as u64 + 1
            );
            let value = view
                .property(PropertyKeyId::new(index as u64 + 1).unwrap(), &mut r)
                .unwrap()
                .unwrap();
            let mut encoded = [0; 9];
            assert_eq!(value.read_at(0, &mut encoded, &mut r).unwrap(), 9);
            assert_eq!(encoded[0], 3);
            assert_eq!(
                i64::from_le_bytes(encoded[1..].try_into().unwrap()),
                (COUNT - index - 1) as i64
            );
        }
        println!(
            "ZE43 chunk-spanning indexes labels={COUNT} properties={COUNT} canonical_bytes={} native_bytes={} participant_peak={} work={}",
            bytes.len(),
            reference.len(),
            memory.peak_reserved_bytes(),
            r.work()
        );
    }
    assert_eq!(writer.reserved_bytes(), 0);
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
}

#[path = "graph_storage_prepare/native_adjacency.rs"]
mod native_adjacency;
