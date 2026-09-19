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
        HighWaters::default()
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
fn key(kind: EntityKind, value: &str) -> ApplicationKey<'_> {
    ApplicationKey::new(kind, "test", value).unwrap()
}
fn create<'a, 'b>(
    kind: EntityKind,
    value: &'a str,
    image: WriteImage<'a, 'b>,
) -> StructuredWrite<'a, 'b> {
    StructuredWrite {
        key: key(kind, value),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(image),
    }
}
#[test]
fn invalid_endpoint_after_valid_node_exposes_no_ids_or_reservations() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut labels = [];
    let mut properties = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    with_local_refs(|refs| {
        let requests = [
            create(EntityKind::Node, "n", WriteImage::Node(&image)),
            create(
                EntityKind::Relationship,
                "r",
                WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Existing(NodeId::new(99).unwrap()),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                },
            ),
        ];
        assert!(
            matches!(
                stage_structured(&Empty, &requests, &memory, &mut |_| Ok(())),
                Err(StageError::Endpoint)
            ),
            "missing endpoint must reject the entire private batch"
        );
    });
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    let requests = [create(EntityKind::Node, "n", WriteImage::Node(&image))];
    let good = stage_structured(&Empty, &requests, &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(
        good.receipts()[0].entity,
        EntityId::Node(NodeId::new(1).unwrap())
    );
    assert_eq!(good.high_waters().node, 1);
    drop(good);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
struct Live<'a> {
    shape: EntityShape<'a>,
    fingerprint: CanonicalFingerprint,
    bytes: Vec<u8>,
    provenance: OperationProvenance<'a>,
    identity: BaseIdentity,
    high: HighWaters,
    view_override: std::cell::Cell<Option<BaseIdentity>>,
}
impl AdmittedBase for Live<'_> {
    fn identity(&self) -> BaseIdentity {
        self.view_override.get().unwrap_or(self.identity)
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
        if self.provenance.fields().key == Some(key) {
            Ok(BaseKeyState::Live(BaseEntity {
                view: self.identity,
                provenance: self.provenance,
                shape: self.shape,
                fingerprint: self.fingerprint,
                source: self,
                membership: Membership::default(),
            }))
        } else {
            Ok(BaseKeyState::NeverUsed)
        }
    }
    fn entity(
        &self,
        id: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        Ok(
            (self.provenance.fields().incarnation == id).then_some(BaseEntity {
                view: self.identity,
                provenance: self.provenance,
                shape: self.shape,
                fingerprint: self.fingerprint,
                source: self,
                membership: Membership::default(),
            }),
        )
    }
    fn has_live_incident(
        &self,
        _: NodeId,
        _: &[RelId],
        _: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        Ok(false)
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
fn live<'a>(image: &'a CanonicalContents<'a>) -> Live<'a> {
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    Live {
        shape: image.shape(),
        fingerprint: image.fingerprint(&mut || Ok(())).unwrap(),
        bytes,
        provenance: OperationProvenance::from_fields(
            Some(1),
            OperationFields {
                operation: GraphOperation::StructuredCreate,
                key: Some(key(EntityKind::Node, "old")),
                requested_revision: GraphRevision::new(4).unwrap(),
                installed_revision: GraphRevision::new(4).unwrap(),
                expected: ExpectedGraphState::Absent,
                incarnation: EntityId::Node(NodeId::new(9).unwrap()),
                delete_mode: None,
                original_generation: GraphGeneration::new(5),
            },
        )
        .unwrap(),
        identity: BaseIdentity {
            store: StoreInstanceId::new(1).unwrap(),
            generation: GraphGeneration::new(7),
            roots: Some(
                zeppelin_embed::property_graph::storage::artifact::ArtifactId::new(3).unwrap(),
            ),
        },
        view_override: std::cell::Cell::new(None),
        high: HighWaters {
            node: 9,
            ..HighWaters::default()
        },
    }
}
#[test]
fn mixed_replay_preserves_original_generation_and_uniform_duplicates_reject() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut labels = [];
    let mut properties = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let base = live(&image);
    let mut old = create(EntityKind::Node, "old", WriteImage::Node(&image));
    old.revision = GraphRevision::new(4).unwrap();
    let new = create(EntityKind::Node, "new", WriteImage::Node(&image));
    let staged = stage_structured(&base, &[old, new], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(
        staged.receipts()[0],
        ItemReceipt {
            entity: EntityId::Node(NodeId::new(9).unwrap()),
            revision: GraphRevision::new(4).unwrap(),
            generation: GraphGeneration::new(5),
            replayed: true
        }
    );
    assert_eq!(
        staged.receipts()[1].entity,
        EntityId::Node(NodeId::new(10).unwrap())
    );
    assert_eq!(staged.receipts()[1].generation, GraphGeneration::new(8));
    assert_eq!(staged.high_waters().node, 10);
    drop(staged);
    assert!(matches!(
        stage_structured(&base, &[old, old], &memory, &mut |_| Ok(())),
        Err(StageError::Lifecycle(KeyLifecycleError::DuplicateTarget))
    ));
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

impl CanonicalSource for Live<'_> {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        CanonicalSlice(&self.bytes).read_at(offset, output)
    }
}
#[test]
fn forward_local_endpoints_resolve_before_relationship_canonicalization() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut labels = [];
    let mut properties = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    with_local_refs(|refs| {
        let requests = [
            create(
                EntityKind::Relationship,
                "r",
                WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(1).unwrap()),
                    target: NodeRef::Local(refs.node(2).unwrap()),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                },
            ),
            create(EntityKind::Node, "a", WriteImage::Node(&image)),
            create(EntityKind::Node, "b", WriteImage::Node(&image)),
        ];
        let staged = stage_structured(&Empty, &requests, &memory, &mut |_| Ok(()))
            .expect("forward local references are valid");
        assert_eq!(
            staged
                .receipts()
                .iter()
                .map(|r| r.entity)
                .collect::<Vec<_>>(),
            vec![
                EntityId::Relationship(RelId::new(1).unwrap()),
                EntityId::Node(NodeId::new(1).unwrap()),
                EntityId::Node(NodeId::new(2).unwrap())
            ]
        );
        assert_eq!(staged.high_waters().relationship, 1);
        assert_eq!(staged.high_waters().node, 2);
    });
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
#[test]
fn receipt_limits_reject_before_returning_ids_including_replay_results() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(
        &resources,
        WriteLimits {
            result_rows: 0,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let mut labels = [];
    let mut properties = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let request = create(EntityKind::Node, "n", WriteImage::Node(&image));
    assert!(
        matches!(
            stage_structured(&Empty, &[request], &memory, &mut |_| Ok(())),
            Err(StageError::Limit)
        ),
        "zero result rows must reject before exposing an allocated identity"
    );
    let empty = stage_structured(&Empty, &[], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(empty.disposition(), BatchDisposition::NoOp);
    drop(empty);
    let base = live(&image);
    let mut replay = create(EntityKind::Node, "old", WriteImage::Node(&image));
    replay.revision = GraphRevision::new(4).unwrap();
    assert!(matches!(
        stage_structured(&base, &[replay], &memory, &mut |_| Ok(())),
        Err(StageError::Limit)
    ));
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

struct Registration {
    bytes: Vec<u8>,
    alive: std::rc::Rc<std::cell::Cell<usize>>,
}
impl ResultRegistration for Registration {
    fn capacity_bytes(&self) -> usize {
        self.bytes.capacity()
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.alive.set(self.alive.get() - 1);
    }
}
struct Materializer {
    layout: ResultLayout,
    alive: std::rc::Rc<std::cell::Cell<usize>>,
    fail: bool,
}
impl ResultMaterializer for Materializer {
    type Registration = Registration;
    fn layout(&mut self, _: usize, _: &mut WriteControl<'_>) -> Result<ResultLayout, StageError> {
        Ok(self.layout)
    }
    fn materialize(
        &mut self,
        receipts: &[ItemReceipt],
        core: &mut [u8],
        abi: &mut [u8],
        control: &mut WriteControl<'_>,
    ) -> Result<Registration, StageError> {
        control(WritePhase::CoreResult)?;
        core.fill(0x37);
        control(WritePhase::AbiResult)?;
        abi.fill(0x68);
        assert!(!receipts.is_empty());
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(self.layout.registry_bytes).unwrap();
        self.alive.set(self.alive.get() + 1);
        let registration = Registration {
            bytes,
            alive: self.alive.clone(),
        };
        if self.fail {
            return Err(StageError::Cancelled);
        }
        Ok(registration)
    }
}
#[test]
fn result_materialization_counts_overlapping_arenas_and_retains_real_registration() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(
        &resources,
        WriteLimits {
            result_bytes: 100,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let mut labels = [];
    let mut properties = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let request = create(EntityKind::Node, "n", WriteImage::Node(&image));
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 1,
            abi_bytes: 101,
            registry_bytes: 41,
        },
        alive: alive.clone(),
        fail: false,
    };
    assert!(
        matches!(
            stage_structured_with_results(&Empty, &[request], &memory, &mut adapter, &mut |_| Ok(
                ()
            )),
            Err(StageError::Limit)
        ),
        "the complete ABI arena must fit its result limit"
    );
    assert_eq!(alive.get(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    adapter.layout.abi_bytes = 100;
    adapter.layout.registry_bytes = 32;
    let staged =
        stage_structured_with_results(&Empty, &[request], &memory, &mut adapter, &mut |_| Ok(()))
            .unwrap();
    assert_eq!(staged.core_bytes(), [0x37]);
    assert_eq!(staged.abi_bytes(), [0x68; 100]);
    assert_eq!(alive.get(), 1);
    assert!(
        resources.reserved_bytes().unwrap()
            >= baseline + 1 + 100 + 32 + std::mem::size_of::<ItemReceipt>() as u64
    );
    drop(staged);
    assert_eq!(alive.get(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    adapter.fail = true;
    assert!(matches!(
        stage_structured_with_results(&Empty, &[request], &memory, &mut adapter, &mut |_| Ok(())),
        Err(StageError::Cancelled)
    ));
    assert_eq!(alive.get(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn changed_names_intern_lazily_while_exact_replay_ignores_exhausted_allocators() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut labels = [GraphName::new("L").unwrap(), GraphName::new("L").unwrap()];
    let mut properties = [GraphProperty::new(
        GraphName::new("p").unwrap(),
        PropertyValue::new(PropertyData::I64(3)).unwrap(),
    )];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let request = create(EntityKind::Node, "new", WriteImage::Node(&image));
    let staged = stage_structured(&Empty, &[request], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(
        staged.symbols().len(),
        3,
        "only namespace, one distinct label and property name are allocated"
    );
    assert_eq!(
        staged.high_waters().symbols,
        SymbolHighWaters {
            label: 1,
            property: 1,
            namespace: 1,
            relationship_type: 0
        }
    );
    assert_eq!(staged.deltas().len(), 1);
    let mut expected = Vec::new();
    image.write_to(&mut expected, &mut || Ok(())).unwrap();
    assert_eq!(staged.deltas()[0].canonical(), Some(expected.as_slice()));
    drop(staged);
    let mut base = live(&image);
    base.identity.generation = GraphGeneration::new(u64::MAX);
    base.high.node = u128::MAX;
    base.high.symbols = SymbolHighWaters {
        label: u64::MAX,
        property: u64::MAX,
        namespace: u64::MAX,
        relationship_type: u64::MAX,
    };
    let mut replay = create(EntityKind::Node, "old", WriteImage::Node(&image));
    replay.revision = GraphRevision::new(4).unwrap();
    let staged = stage_structured(&base, &[replay], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(staged.disposition(), BatchDisposition::Replayed);
    assert!(staged.deltas().is_empty());
    assert!(staged.symbols().is_empty());
    assert_eq!(staged.high_waters(), base.high);
    assert_eq!(staged.receipts()[0].generation, GraphGeneration::new(5));
    drop(staged);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn pending_access_observes_same_clause_changes_absence_empty_text_and_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut labels = [];
    let mut properties = [];
    let original = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let base = live(&original);
    let mut l1 = [];
    let mut p1 = [GraphProperty::new(
        GraphName::new("x").unwrap(),
        PropertyValue::new(PropertyData::I64(41)).unwrap(),
    )];
    let one = CanonicalContents::node(&mut l1, &mut p1, Some(""), None).unwrap();
    let mut l2 = [];
    let mut p2 = [GraphProperty::new(
        GraphName::new("x").unwrap(),
        PropertyValue::new(PropertyData::I64(42)).unwrap(),
    )];
    let two = CanonicalContents::node(&mut l2, &mut p2, None, None).unwrap();
    let mut overlay = GraphBatchReadView::new(&base, &memory, 1, &mut |_| Ok(())).unwrap();
    let node = NodeRef::Existing(NodeId::new(9).unwrap());
    let target = BatchEntityRef::Node(node);
    let name = GraphName::new("x").unwrap();
    assert!(
        overlay
            .property(target, name, &mut |_| Ok(()))
            .unwrap()
            .is_none()
    );
    overlay
        .replace(target, WriteImage::Node(&one), &mut |_| Ok(()))
        .unwrap();
    assert!(
        matches!(
            overlay
                .property(target, name, &mut |_| Ok(()))
                .unwrap()
                .map(|v| v.data()),
            Some(PropertyData::I64(41))
        ),
        "same-clause reads must use the pending image"
    );
    assert_eq!(
        overlay.stored_text(node, &mut |_| Ok(())).unwrap(),
        Some("")
    );
    overlay
        .replace(target, WriteImage::Node(&two), &mut |_| Ok(()))
        .unwrap();
    assert!(matches!(
        overlay
            .property(target, name, &mut |_| Ok(()))
            .unwrap()
            .map(|v| v.data()),
        Some(PropertyData::I64(42))
    ));
    assert_eq!(overlay.stored_text(node, &mut |_| Ok(())).unwrap(), None);
    overlay
        .delete(target, GraphDeleteMode::Detach, &mut |_| Ok(()))
        .unwrap();
    assert!(matches!(
        overlay.property(target, name, &mut |_| Ok(())),
        Err(StageError::DeletedEntity)
    ));
    assert!(matches!(
        overlay.stored_text(node, &mut |_| Ok(())),
        Err(StageError::DeletedEntity)
    ));
    assert_eq!(overlay.counters().property_lookups, 4);
    assert_eq!(overlay.counters().text_lookups, 3);
    assert_eq!(overlay.counters().value_bytes, 16);
    drop(overlay);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn cypher_finalization_advances_once_and_consumed_local_ids_survive_net_empty_changes() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut l = [];
    let mut p = [];
    let original = CanonicalContents::node(&mut l, &mut p, None, None).unwrap();
    let base = live(&original);
    let mut l1 = [];
    let mut p1 = [GraphProperty::new(
        GraphName::new("x").unwrap(),
        PropertyValue::new(PropertyData::I64(1)).unwrap(),
    )];
    let one = CanonicalContents::node(&mut l1, &mut p1, None, None).unwrap();
    let target = BatchEntityRef::Node(NodeRef::Existing(NodeId::new(9).unwrap()));
    let mut overlay = GraphBatchReadView::new(&base, &memory, 1, &mut |_| Ok(())).unwrap();
    overlay
        .replace(target, WriteImage::Node(&one), &mut |_| Ok(()))
        .unwrap();
    overlay
        .replace(target, WriteImage::Node(&original), &mut |_| Ok(()))
        .unwrap();
    let unchanged = overlay
        .finish(&mut |_| Ok(()))
        .expect("final equality is NoOp");
    assert_eq!(unchanged.disposition(), BatchDisposition::NoOp);
    assert!(unchanged.deltas().is_empty());
    drop(unchanged);
    let mut overlay = GraphBatchReadView::new(&base, &memory, 1, &mut |_| Ok(())).unwrap();
    overlay
        .replace(target, WriteImage::Node(&original), &mut |_| Ok(()))
        .unwrap();
    overlay
        .replace(target, WriteImage::Node(&one), &mut |_| Ok(()))
        .unwrap();
    let changed = overlay.finish(&mut |_| Ok(())).unwrap();
    assert_eq!(changed.disposition(), BatchDisposition::Changed);
    assert_eq!(changed.deltas().len(), 1);
    let fields = changed.deltas()[0].provenance().fields();
    assert_eq!(fields.operation, GraphOperation::CypherEdit);
    assert_eq!(fields.installed_revision, GraphRevision::new(5).unwrap());
    assert_eq!(fields.original_generation, GraphGeneration::new(8));
    drop(changed);
    with_local_refs(|refs| {
        let mut overlay = GraphBatchReadView::new(&Empty, &memory, 1, &mut |_| Ok(())).unwrap();
        let node = BatchEntityRef::Node(NodeRef::Local(refs.node(0).unwrap()));
        overlay
            .create(node, WriteImage::Node(&one), &mut |_| Ok(()))
            .unwrap();
        overlay
            .delete(node, GraphDeleteMode::Detach, &mut |_| Ok(()))
            .unwrap();
        let consumed = overlay.finish(&mut |_| Ok(())).unwrap();
        assert_eq!(consumed.disposition(), BatchDisposition::Changed);
        assert_eq!(consumed.high_waters().node, 1);
        assert!(consumed.deltas().is_empty());
        assert!(consumed.symbols().is_empty());
        assert!(consumed.receipts().is_empty());
    });
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn every_revision_precondition_is_classified_before_any_private_identity_assignment() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut l = [];
    let mut p = [];
    let image = CanonicalContents::node(&mut l, &mut p, None, None).unwrap();
    let base = live(&image);
    let good = create(EntityKind::Node, "new", WriteImage::Node(&image));
    let bad = StructuredWrite {
        key: key(EntityKind::Node, "old"),
        revision: GraphRevision::new(3).unwrap(),
        operation: StructuredOperation::Put(EntityId::Node(NodeId::new(9).unwrap())),
        image: Some(WriteImage::Node(&image)),
    };
    let mut assignments = 0;
    assert!(matches!(
        stage_structured(&base, &[good, bad], &memory, &mut |phase| {
            if phase == WritePhase::Identity {
                assignments += 1;
            }
            Ok(())
        }),
        Err(StageError::Lifecycle(KeyLifecycleError::Stale { .. }))
    ));
    assert_eq!(
        assignments, 0,
        "a later stale item must reject before any new-ID assignment"
    );
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
#[test]
fn fresh_relationship_fields_and_exact_input_framing_are_validated_before_private_ids() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut l = [];
    let mut p = [];
    let image = CanonicalContents::node(&mut l, &mut p, None, None).unwrap();
    let property = GraphProperty::new(
        GraphName::new("x").unwrap(),
        PropertyValue::new(PropertyData::I64(1)).unwrap(),
    );
    let duplicate = [property, property];
    with_local_refs(|refs| {
        let node = NodeRef::Local(refs.node(0).unwrap());
        let first = create(EntityKind::Node, "n", WriteImage::Node(&image));
        let bad = create(
            EntityKind::Relationship,
            "r",
            WriteImage::Relationship {
                source: node,
                target: node,
                relationship_type: GraphName::new("R").unwrap(),
                properties: &duplicate,
            },
        );
        let mut assigned = 0;
        assert!(matches!(
            stage_structured(&Empty, &[first, bad], &memory, &mut |phase| {
                if phase == WritePhase::Identity {
                    assigned += 1;
                }
                Ok(())
            }),
            Err(StageError::Canonical(CanonicalError::DuplicateProperty))
        ));
        assert_eq!(
            assigned, 0,
            "ordinary relationship property validation precedes private IDs"
        );
        assert_eq!(resources.reserved_bytes().unwrap(), baseline);
        let good = create(
            EntityKind::Relationship,
            "r",
            WriteImage::Relationship {
                source: node,
                target: node,
                relationship_type: GraphName::new("R").unwrap(),
                properties: &duplicate[..1],
            },
        );
        let staged = stage_structured(&Empty, &[first, good], &memory, &mut |_| Ok(())).unwrap();
        assert_eq!(
            staged.receipts()[0].entity,
            EntityId::Node(NodeId::new(1).unwrap())
        );
        assert_eq!(
            staged.receipts()[1].entity,
            EntityId::Relationship(RelId::new(1).unwrap())
        );
        let mut properties = [property];
        let expected = CanonicalContents::relationship(
            NodeId::new(1).unwrap(),
            NodeId::new(1).unwrap(),
            GraphName::new("R").unwrap(),
            &mut properties,
        )
        .unwrap();
        let mut bytes = Vec::new();
        expected.write_to(&mut bytes, &mut || Ok(())).unwrap();
        assert_eq!(staged.deltas()[1].canonical(), Some(bytes.as_slice()));
    });
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    // Empty node frame =25 bytes; complete create evidence for key test/n =73.
    let exact = WriteMemory::new(
        &resources,
        WriteLimits {
            input_bytes: 98,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let request = create(EntityKind::Node, "n", WriteImage::Node(&image));
    let staged = stage_structured(&Empty, &[request], &exact, &mut |_| Ok(())).unwrap();
    assert_eq!(staged.deltas()[0].canonical().unwrap().len(), 25);
    assert_eq!(staged.deltas()[0].provenance().encoded_len(), 73);
    drop(staged);
    let tight = WriteMemory::new(
        &resources,
        WriteLimits {
            input_bytes: 97,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let mut assigned = 0;
    assert!(matches!(
        stage_structured(&Empty, &[request], &tight, &mut |phase| {
            if phase == WritePhase::Identity {
                assigned += 1;
            }
            Ok(())
        }),
        Err(StageError::Limit)
    ));
    assert_eq!(
        assigned, 0,
        "aggregate framing admission precedes private IDs"
    );
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
fn cancelled(error: &StageError) -> bool {
    matches!(
        error,
        StageError::Cancelled
            | StageError::Canonical(CanonicalError::Cancelled)
            | StageError::Lifecycle(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
            | StageError::Catalog(CatalogError::Cancelled)
    )
}
#[test]
fn every_private_checkpoint_can_cancel_without_ids_or_retained_allocations() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let text = "text".repeat(50_000);
    let mut l = [];
    let mut p = [];
    let image = CanonicalContents::node(&mut l, &mut p, Some(&text), None).unwrap();
    let request = create(EntityKind::Node, "n", WriteImage::Node(&image));
    let mut phases = Vec::new();
    let good = stage_structured(&Empty, &[request], &memory, &mut |phase| {
        phases.push(phase);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        good.deltas()[0].membership(),
        (
            Membership::default(),
            Membership {
                text: true,
                vector: false
            }
        )
    );
    drop(good);
    assert!(phases.contains(&WritePhase::Identity));
    assert!(phases.contains(&WritePhase::Canonical));
    assert!(phases.contains(&WritePhase::CoreResult));
    assert!(phases.contains(&WritePhase::Finalize));
    for cutoff in 1..=phases.len() {
        let mut seen = 0;
        let result = stage_structured(&Empty, &[request], &memory, &mut |_| {
            seen += 1;
            if seen == cutoff {
                Err(StageError::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(&result,Err(error) if cancelled(error)),
            "cutoff {cutoff} must fire at its intended checkpoint"
        );
        drop(result);
        assert_eq!(memory.reserved_bytes(), 0);
        assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    }
    let mut seen = 0;
    let clean = stage_structured(&Empty, &[request], &memory, &mut |_| {
        seen += 1;
        if seen == phases.len() + 1 {
            Err(StageError::Cancelled)
        } else {
            Ok(())
        }
    })
    .unwrap();
    assert_eq!(seen, phases.len());
    assert_eq!(
        clean.receipts()[0].entity,
        EntityId::Node(NodeId::new(1).unwrap())
    );
    drop(clean);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    eprintln!(
        "ZE37 cancellation checkpoints={} text_bytes={} every_cut_rejected_and_released=true",
        phases.len(),
        text.len()
    );
}
#[test]
fn full_width_high_waters_and_revision_generation_overflow_never_wrap() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut l = [];
    let mut p = [];
    let image = CanonicalContents::node(&mut l, &mut p, None, None).unwrap();
    let mut base = live(&image);
    base.high.node = (1u128 << 127) + 7;
    let request = create(EntityKind::Node, "new", WriteImage::Node(&image));
    let staged = stage_structured(&base, &[request], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(
        staged.receipts()[0].entity,
        EntityId::Node(NodeId::new((1u128 << 127) + 8).unwrap())
    );
    drop(staged);
    base.high.node = u128::MAX;
    assert!(matches!(
        stage_structured(&base, &[request], &memory, &mut |_| Ok(())),
        Err(StageError::IdentityOverflow)
    ));
    base.high.node = 9;
    base.identity.generation = GraphGeneration::new(u64::MAX);
    let mut assigned = 0;
    assert!(matches!(
        stage_structured(&base, &[request], &memory, &mut |phase| {
            if phase == WritePhase::Identity {
                assigned += 1;
            }
            Ok(())
        }),
        Err(StageError::Lifecycle(KeyLifecycleError::GenerationOverflow))
    ));
    assert_eq!(assigned, 0);
    base.identity.generation = GraphGeneration::new(7);
    let fields = base.provenance.fields();
    base.provenance = OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            requested_revision: GraphRevision::new(u64::MAX).unwrap(),
            installed_revision: GraphRevision::new(u64::MAX).unwrap(),
            ..fields
        },
    )
    .unwrap();
    let mut l1 = [];
    let mut p1 = [GraphProperty::new(
        GraphName::new("x").unwrap(),
        PropertyValue::new(PropertyData::I64(1)).unwrap(),
    )];
    let changed = CanonicalContents::node(&mut l1, &mut p1, None, None).unwrap();
    let mut overlay = GraphBatchReadView::new(&base, &memory, 1, &mut |_| Ok(())).unwrap();
    overlay
        .replace(
            BatchEntityRef::Node(NodeRef::Existing(NodeId::new(9).unwrap())),
            WriteImage::Node(&changed),
            &mut |_| Ok(()),
        )
        .unwrap();
    assert!(matches!(
        overlay.finish(&mut |_| Ok(())),
        Err(StageError::Lifecycle(KeyLifecycleError::RevisionOverflow))
    ));
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn result_scope_adoption_moves_sole_charge_and_rolls_back_each_partial_handoff() {
    use zeppelin_embed::property_graph::resources::GraphReservation;
    struct Adopted {
        charge: GraphReservation,
        registry_alive: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl Drop for Adopted {
        fn drop(&mut self) {
            assert_eq!(
                self.registry_alive.get(),
                0,
                "backing owner must drop before adopted capacity"
            );
            assert!(self.charge.bytes() > 0);
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let request = create(EntityKind::Node, "n", WriteImage::Node(&image));
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 11,
            abi_bytes: 17,
            registry_bytes: 23,
        },
        alive: alive.clone(),
        fail: false,
    };
    for fail_at in 0..=3 {
        let staged =
            stage_structured_with_results(&Empty, &[request], &memory, &mut adapter, &mut |_| {
                Ok(())
            })
            .unwrap();
        let writer_before = memory.reserved_bytes();
        let shared_before = resources.reserved_bytes().unwrap();
        let mut calls = 0;
        let adopted = staged.adopt_result_memory(
            &Empty,
            &mut |charge: GraphReservation| {
                assert_eq!(memory.reserved_bytes(), writer_before);
                assert_eq!(
                    resources.reserved_bytes().unwrap(),
                    shared_before,
                    "transfer must neither double-charge nor release backing"
                );
                let index = calls;
                calls += 1;
                if index == fail_at {
                    Err((StageError::Limit, charge))
                } else {
                    Ok(Adopted {
                        charge,
                        registry_alive: alive.clone(),
                    })
                }
            },
            &mut |_| Ok(()),
        );
        if fail_at < 3 {
            assert!(matches!(adopted, Err(StageError::Limit)));
        } else {
            let result = adopted.unwrap();
            assert_eq!(result.core_bytes(), [0x37; 11]);
            assert_eq!(result.abi_bytes(), [0x68; 17]);
            assert_eq!(memory.reserved_bytes(), writer_before);
            assert_eq!(resources.reserved_bytes().unwrap(), shared_before);
            assert_eq!(alive.get(), 1);
            drop(result);
        }
        assert_eq!(calls, (fail_at + 1).min(3));
        assert_eq!(alive.get(), 0);
        assert_eq!(memory.reserved_bytes(), 0);
        assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    }
}

#[test]
fn staged_results_adopt_actual_query_memory_without_duplicate_shared_charges() {
    use zeppelin_embed::property_graph::query::resources::{QueryMemory, QuerySharedReservation};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let request = create(EntityKind::Node, "n", WriteImage::Node(&image));
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 11,
            abi_bytes: 17,
            registry_bytes: 23,
        },
        alive: alive.clone(),
        fail: false,
    };
    let owner_bytes = std::mem::size_of::<QuerySharedReservation<'_, '_>>();
    let query_bytes = std::mem::size_of::<QueryMemory<'_>>();
    let capacities = [11, 17, 23];
    for fail_at in 0..=3 {
        let limit = if fail_at < 3 {
            query_bytes + capacities[..=fail_at].iter().sum::<usize>() + (fail_at + 1) * owner_bytes
                - 1
        } else {
            24 * 1024 * 1024
        };
        let query = QueryMemory::new(&resources, limit).unwrap();
        let query_baseline = query.reserved_bytes();
        let shared_baseline = resources.reserved_bytes().unwrap();
        let staged =
            stage_structured_with_results(&Empty, &[request], &memory, &mut adapter, &mut |_| {
                Ok(())
            })
            .unwrap();
        let writer_before = memory.reserved_bytes();
        let shared_before = resources.reserved_bytes().unwrap();
        let mut calls = 0;
        let adopted = staged.adopt_result_memory(
            &Empty,
            &mut |charge| {
                assert_eq!(memory.reserved_bytes(), writer_before);
                calls += 1;
                query
                    .adopt_shared(charge)
                    .map_err(|(_, original)| (StageError::Limit, original))
            },
            &mut |_| Ok(()),
        );
        if fail_at < 3 {
            assert!(matches!(adopted, Err(StageError::Limit)));
            assert_eq!(
                calls,
                fail_at + 1,
                "query limit must reject at the selected ownership transfer"
            );
            drop(adopted);
        } else {
            let result = adopted.unwrap();
            assert_eq!(calls, 3);
            assert_eq!(result.core_bytes(), [0x37; 11]);
            assert_eq!(result.abi_bytes(), [0x68; 17]);
            assert_eq!(result.registration().capacity_bytes(), 23);
            assert_eq!(
                query.reserved_bytes(),
                query_baseline + 51 + 3 * owner_bytes
            );
            assert_eq!(
                resources.reserved_bytes().unwrap(),
                shared_before + (3 * owner_bytes) as u64,
                "the real query owner adds control charges, never a second backing charge"
            );
            assert_eq!(memory.reserved_bytes(), writer_before);
            assert_eq!(alive.get(), 1);
            drop(result);
        }
        assert_eq!(alive.get(), 0);
        assert_eq!(memory.reserved_bytes(), 0);
        assert_eq!(query.reserved_bytes(), query_baseline);
        assert_eq!(resources.reserved_bytes().unwrap(), shared_baseline);
        drop(query);
        assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    }
}

#[test]
fn cypher_outputs_require_complete_precommit_materialization_even_for_noop() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let base = live(&image);
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 11,
            abi_bytes: 17,
            registry_bytes: 23,
        },
        alive: alive.clone(),
        fail: true,
    };
    for failure in [true, false] {
        adapter.fail = failure;
        let mut overlay = GraphBatchReadView::new(&base, &memory, 1, &mut |_| Ok(())).unwrap();
        overlay
            .replace(
                BatchEntityRef::Node(NodeRef::Existing(NodeId::new(9).unwrap())),
                WriteImage::Node(&image),
                &mut |_| Ok(()),
            )
            .unwrap();
        let output = overlay.finish_with_results(&mut adapter, &mut |_| Ok(()));
        if failure {
            assert!(matches!(output, Err(StageError::Cancelled)));
        } else {
            let output = output.unwrap();
            assert_eq!(output.batch().disposition(), BatchDisposition::NoOp);
            assert_eq!(
                output.batch().receipts()[0].generation,
                GraphGeneration::new(5)
            );
            assert_eq!(alive.get(), 1);
            drop(output);
        }
        assert_eq!(alive.get(), 0);
        assert_eq!(memory.reserved_bytes(), 0);
        assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    }
}

struct Hub<'a> {
    node: Live<'a>,
    relationship: Live<'a>,
    incidents: Vec<(RelId, bool)>,
    probes: std::cell::Cell<usize>,
}
impl AdmittedBase for Hub<'_> {
    fn identity(&self) -> BaseIdentity {
        self.node.identity()
    }
    fn high_waters(&self) -> HighWaters {
        HighWaters {
            relationship: 12,
            symbols: SymbolHighWaters {
                namespace: 1,
                ..SymbolHighWaters::default()
            },
            ..self.node.high_waters()
        }
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        self.node.interpretation()
    }
    fn key(
        &self,
        key: ApplicationKey<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        let mut state = if key.kind() == EntityKind::Node {
            self.node.key(key, control)?
        } else {
            self.relationship.key(key, control)?
        };
        if let BaseKeyState::Live(ref mut entity) = state
            && key.kind() == EntityKind::Node
        {
            entity.membership = Membership {
                text: true,
                vector: true,
            };
        }
        Ok(state)
    }
    fn entity(
        &self,
        id: EntityId,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        let mut entity = if id.kind() == EntityKind::Node {
            self.node.entity(id, control)?
        } else {
            self.relationship.entity(id, control)?
        };
        if let Some(ref mut entity) = entity
            && id.kind() == EntityKind::Node
        {
            entity.membership = Membership {
                text: true,
                vector: true,
            };
        }
        Ok(entity)
    }
    fn has_live_incident(
        &self,
        node: NodeId,
        removed: &[RelId],
        control: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        assert_eq!(node.get(), 9);
        for (id, both_alive) in &self.incidents {
            control(WritePhase::Incident)?;
            self.probes.set(self.probes.get() + 1);
            if *both_alive && !removed.contains(id) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn property(
        &self,
        id: EntityId,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        self.node.property(id, name, control)
    }
    fn stored_text(
        &self,
        id: NodeId,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<&str>, StageError> {
        self.node.stored_text(id, control)
    }
    fn symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        if kind == SymbolKind::Namespace && name.as_str() == "test" {
            Ok(Some(Symbol::new(kind, 1).unwrap()))
        } else {
            self.node.symbol(kind, name, control)
        }
    }
}
fn hub<'a>(node: &'a CanonicalContents<'a>, rel: &'a CanonicalContents<'a>) -> Hub<'a> {
    let mut relationship = live(rel);
    relationship.provenance = OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            key: Some(key(EntityKind::Relationship, "edge")),
            incarnation: EntityId::Relationship(RelId::new(12).unwrap()),
            ..relationship.provenance.fields()
        },
    )
    .unwrap();
    Hub {
        node: live(node),
        relationship,
        incidents: vec![(RelId::new(12).unwrap(), true)],
        probes: std::cell::Cell::new(0),
    }
}
#[test]
fn restrict_preconditions_precede_cypher_private_ids_and_detach_never_enumerates_incidents() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let rel = CanonicalContents::relationship(
        NodeId::new(9).unwrap(),
        NodeId::new(9).unwrap(),
        GraphName::new("R").unwrap(),
        &mut [],
    )
    .unwrap();
    let mut base = hub(&node, &rel);
    with_local_refs(|refs| {
        let mut overlay = GraphBatchReadView::new(&base, &memory, 2, &mut |_| Ok(())).unwrap();
        overlay
            .create(
                BatchEntityRef::Node(NodeRef::Local(refs.node(0).unwrap())),
                WriteImage::Node(&node),
                &mut |_| Ok(()),
            )
            .unwrap();
        overlay
            .delete(
                BatchEntityRef::Node(NodeRef::Existing(NodeId::new(9).unwrap())),
                GraphDeleteMode::Restrict,
                &mut |_| Ok(()),
            )
            .unwrap();
        let mut identities = 0;
        assert!(matches!(
            overlay.finish(&mut |phase| {
                if phase == WritePhase::Identity {
                    identities += 1;
                }
                Ok(())
            }),
            Err(StageError::IncidentRelationship)
        ));
        assert_eq!(
            identities, 0,
            "Restrict validation must precede fresh identity assignment"
        );
    });
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    base.incidents = (1..=32769)
        .map(|n| (RelId::new(n).unwrap(), true))
        .collect();
    base.probes.set(0);
    let request = StructuredWrite {
        key: key(EntityKind::Node, "old"),
        revision: GraphRevision::new(5).unwrap(),
        operation: StructuredOperation::Delete(
            EntityId::Node(NodeId::new(9).unwrap()),
            GraphDeleteMode::Detach,
        ),
        image: None,
    };
    let staged = stage_structured(&base, &[request], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(staged.deltas().len(), 1);
    assert_eq!(
        staged.deltas()[0].membership(),
        (
            Membership {
                text: true,
                vector: true
            },
            Membership::default()
        )
    );
    assert!(staged.deltas()[0].canonical().is_none());
    assert_eq!(staged.high_waters(), base.high_waters());
    assert_eq!(
        base.probes.get(),
        0,
        "DETACH must never request incident enumeration"
    );
    drop(staged);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    base.incidents = vec![
        (RelId::new(11).unwrap(), false),
        (RelId::new(12).unwrap(), true),
    ];
    let request = StructuredWrite {
        operation: StructuredOperation::Delete(
            EntityId::Node(NodeId::new(9).unwrap()),
            GraphDeleteMode::Restrict,
        ),
        ..request
    };
    assert!(matches!(
        stage_structured(&base, &[request], &memory, &mut |_| Ok(())),
        Err(StageError::IncidentRelationship)
    ));
    let edge = StructuredWrite {
        key: key(EntityKind::Relationship, "edge"),
        revision: GraphRevision::new(5).unwrap(),
        operation: StructuredOperation::Delete(
            EntityId::Relationship(RelId::new(12).unwrap()),
            GraphDeleteMode::Restrict,
        ),
        image: None,
    };
    let staged = stage_structured(&base, &[request, edge], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(staged.deltas().len(), 2);
    drop(staged);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn cypher_classifies_all_revisions_before_selecting_changed_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let rel = CanonicalContents::relationship(
        NodeId::new(9).unwrap(),
        NodeId::new(9).unwrap(),
        GraphName::new("R").unwrap(),
        &mut [],
    )
    .unwrap();
    let mut base = hub(&node, &rel);
    base.node.identity.generation = GraphGeneration::new(u64::MAX);
    base.relationship.identity = base.node.identity;
    base.relationship.provenance = OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            requested_revision: GraphRevision::new(u64::MAX).unwrap(),
            installed_revision: GraphRevision::new(u64::MAX).unwrap(),
            ..base.relationship.provenance.fields()
        },
    )
    .unwrap();
    let mut overlay = GraphBatchReadView::new(&base, &memory, 2, &mut |_| Ok(())).unwrap();
    overlay
        .delete(
            BatchEntityRef::Node(NodeRef::Existing(NodeId::new(9).unwrap())),
            GraphDeleteMode::Detach,
            &mut |_| Ok(()),
        )
        .unwrap();
    overlay
        .delete(
            BatchEntityRef::Relationship(RelRef::Existing(RelId::new(12).unwrap())),
            GraphDeleteMode::Restrict,
            &mut |_| Ok(()),
        )
        .unwrap();
    assert!(
        matches!(
            overlay.finish(&mut |_| Ok(())),
            Err(StageError::Lifecycle(KeyLifecycleError::RevisionOverflow))
        ),
        "all revision preconditions precede changed-generation selection"
    );
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn admitted_high_waters_must_cover_every_live_incarnation_before_allocating() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut base = live(&node);
    base.high.node = 8;
    let mut old = create(EntityKind::Node, "old", WriteImage::Node(&node));
    old.revision = GraphRevision::new(4).unwrap();
    let new = create(EntityKind::Node, "new", WriteImage::Node(&node));
    let mut identities = 0;
    assert!(
        matches!(
            stage_structured(&base, &[new, old], &memory, &mut |phase| {
                if phase == WritePhase::Identity {
                    identities += 1;
                }
                Ok(())
            }),
            Err(StageError::ViewMismatch)
        ),
        "incoherent allocation fence must not alias a retained incarnation"
    );
    assert_eq!(identities, 0);
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    base.high.node = 9;
    let clean = stage_structured(&base, &[new, old], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(
        clean.receipts()[0].entity,
        EntityId::Node(NodeId::new(10).unwrap())
    );
}

#[test]
fn final_view_drift_discards_materialized_abi_and_registration_without_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let base = live(&node);
    let request = create(EntityKind::Node, "new", WriteImage::Node(&node));
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 7,
            abi_bytes: 9,
            registry_bytes: 11,
        },
        alive: alive.clone(),
        fail: false,
    };
    let mut final_checks = 0;
    let result =
        stage_structured_with_results(&base, &[request], &memory, &mut adapter, &mut |phase| {
            if phase == WritePhase::Finalize {
                final_checks += 1;
                if final_checks == 2 {
                    assert_eq!(alive.get(), 1);
                    base.view_override.set(Some(BaseIdentity {
                        generation: GraphGeneration::new(8),
                        ..base.identity
                    }));
                }
            }
            Ok(())
        });
    assert!(matches!(result, Err(StageError::ViewMismatch)));
    assert_eq!(final_checks, 2);
    assert_eq!(alive.get(), 0);
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    base.view_override.set(None);
    let result =
        stage_structured_with_results(&base, &[request], &memory, &mut adapter, &mut |_| Ok(()))
            .unwrap();
    assert_eq!(
        result.batch().receipts()[0].entity,
        EntityId::Node(NodeId::new(10).unwrap())
    );
    drop(result);
    base.view_override.set(Some(BaseIdentity {
        store: StoreInstanceId::new(2).unwrap(),
        ..base.identity
    }));
    let mut replay = create(EntityKind::Node, "old", WriteImage::Node(&node));
    replay.revision = GraphRevision::new(4).unwrap();
    assert!(
        matches!(
            stage_structured(&base, &[replay], &memory, &mut |_| Ok(())),
            Err(StageError::ViewMismatch)
        ),
        "a record from another admitted store/root is rejected"
    );
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn writer_adoption_checks_real_shared_owner_and_returns_failed_charge_intact() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let store_a = Store::open(
        a.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let store_b = Store::open(
        b.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let owner_a = GraphResources::from_store(&store_a).unwrap();
    let owner_b = GraphResources::from_store(&store_b).unwrap();
    let base_a = owner_a.reserved_bytes().unwrap();
    let base_b = owner_b.reserved_bytes().unwrap();
    let memory = WriteMemory::new(
        &owner_a,
        WriteLimits {
            writer_bytes: 17,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let foreign = owner_b.reserve(17).unwrap();
    let returned = match memory.adopt(foreign) {
        Err((WriteAdoptionError::ForeignOwner, charge)) => charge,
        _ => panic!("foreign accounting owner accepted"),
    };
    assert_eq!(returned.bytes(), 17);
    assert_eq!(owner_b.reserved_bytes().unwrap(), base_b + 17);
    drop(returned);
    let large = owner_a.reserve(18).unwrap();
    let returned = match memory.adopt(large) {
        Err((WriteAdoptionError::Limit, charge)) => charge,
        _ => panic!("writer-local cap bypassed"),
    };
    assert_eq!(returned.bytes(), 18);
    assert_eq!(memory.reserved_bytes(), 0);
    drop(returned);
    let charge = owner_a.reserve(17).unwrap();
    let adopted = memory
        .adopt(charge)
        .unwrap_or_else(|(e, _)| panic!("{e:?}"));
    assert_eq!(adopted.bytes(), 17);
    assert_eq!(memory.reserved_bytes(), 17);
    assert_eq!(owner_a.reserved_bytes().unwrap(), base_a + 17);
    drop(adopted);
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(owner_a.reserved_bytes().unwrap(), base_a);
    assert_eq!(owner_b.reserved_bytes().unwrap(), base_b);
}

#[test]
fn exact_replay_reads_original_bytes_and_rejects_truncated_base_source() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut properties = [GraphProperty::new(
        GraphName::new("v").unwrap(),
        PropertyValue::new(PropertyData::F64(f64::from_bits(0x8000000000000000))).unwrap(),
    )];
    let image = CanonicalContents::node(&mut [], &mut properties, None, None).unwrap();
    let mut base = live(&image);
    let mut replay = create(EntityKind::Node, "old", WriteImage::Node(&image));
    replay.revision = GraphRevision::new(4).unwrap();
    let original = base.bytes.clone();
    base.bytes[33] ^= 1;
    assert!(
        matches!(
            stage_structured(&base, &[replay], &memory, &mut |_| Ok(())),
            Err(StageError::Lifecycle(KeyLifecycleError::RevisionConflict))
        ),
        "equal cached hash is only a precheck"
    );
    base.bytes = original.clone();
    base.bytes.pop();
    assert!(matches!(
        stage_structured(&base, &[replay], &memory, &mut |_| Ok(())),
        Err(StageError::Lifecycle(KeyLifecycleError::Canonical(
            CanonicalError::Io(_)
        )))
    ));
    base.bytes = original;
    let clean = stage_structured(&base, &[replay], &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(clean.disposition(), BatchDisposition::Replayed);
    drop(clean);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn cypher_fresh_payload_interpretation_and_duplicate_fields_precede_all_private_ids() {
    use zeppelin_embed::epoch::*;
    let tower = EmbeddingTower {
        model_id: "doc".into(),
        model_version: "v1".into(),
        weights_digest: vec![1],
        dims: 2,
        normalization: Normalization::L2,
        prompt_prefix: "".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let plain = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let embedded = CanonicalContents::node(
        &mut [],
        &mut [],
        None,
        Some(CanonicalEmbedding::new(&tower, &[1.0, 2.0]).unwrap()),
    )
    .unwrap();
    with_local_refs(|refs| {
        let mut overlay = GraphBatchReadView::new(&Empty, &memory, 2, &mut |_| Ok(())).unwrap();
        overlay
            .create(
                BatchEntityRef::Node(NodeRef::Local(refs.node(0).unwrap())),
                WriteImage::Node(&plain),
                &mut |_| Ok(()),
            )
            .unwrap();
        overlay
            .create(
                BatchEntityRef::Node(NodeRef::Local(refs.node(1).unwrap())),
                WriteImage::Node(&embedded),
                &mut |_| Ok(()),
            )
            .unwrap();
        let mut assigned = 0;
        assert!(matches!(
            overlay.finish(&mut |p| {
                if p == WritePhase::Identity {
                    assigned += 1;
                }
                Ok(())
            }),
            Err(StageError::Catalog(_))
        ));
        assert_eq!(
            assigned, 0,
            "fresh node interpretation precedes all private IDs"
        );
    });
    with_local_refs(|refs| {
        let prop = GraphProperty::new(
            GraphName::new("x").unwrap(),
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        );
        let duplicate = [prop, prop];
        let mut overlay = GraphBatchReadView::new(&Empty, &memory, 2, &mut |_| Ok(())).unwrap();
        let node = NodeRef::Local(refs.node(0).unwrap());
        overlay
            .create(
                BatchEntityRef::Node(node),
                WriteImage::Node(&plain),
                &mut |_| Ok(()),
            )
            .unwrap();
        overlay
            .create(
                BatchEntityRef::Relationship(RelRef::Local(refs.relationship(0).unwrap())),
                WriteImage::Relationship {
                    source: node,
                    target: node,
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &duplicate,
                },
                &mut |_| Ok(()),
            )
            .unwrap();
        let mut assigned = 0;
        assert!(matches!(
            overlay.finish(&mut |p| {
                if p == WritePhase::Identity {
                    assigned += 1;
                }
                Ok(())
            }),
            Err(StageError::Canonical(CanonicalError::DuplicateProperty))
        ));
        assert_eq!(
            assigned, 0,
            "fresh relationship properties precede all private IDs"
        );
    });
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}

#[test]
fn restrict_integrity_precedes_generation_overflow_for_structured_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let rel = CanonicalContents::relationship(
        NodeId::new(9).unwrap(),
        NodeId::new(9).unwrap(),
        GraphName::new("R").unwrap(),
        &mut [],
    )
    .unwrap();
    let mut base = hub(&node, &rel);
    base.node.identity.generation = GraphGeneration::new(u64::MAX);
    base.relationship.identity = base.node.identity;
    let request = StructuredWrite {
        key: key(EntityKind::Node, "old"),
        revision: GraphRevision::new(5).unwrap(),
        operation: StructuredOperation::Delete(
            EntityId::Node(NodeId::new(9).unwrap()),
            GraphDeleteMode::Restrict,
        ),
        image: None,
    };
    assert!(
        matches!(
            stage_structured(&base, &[request], &memory, &mut |_| Ok(())),
            Err(StageError::IncidentRelationship)
        ),
        "Restrict precondition precedes target generation"
    );
}

fn result_layout_preflight_case(cypher: bool, max_generation: bool) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut base = live(&node);
    if max_generation {
        base.identity.generation = GraphGeneration::new(u64::MAX);
    }
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 4 * 1024 * 1024 + 1,
            abi_bytes: 0,
            registry_bytes: 0,
        },
        alive: alive.clone(),
        fail: false,
    };
    let mut identities = 0;
    let result = if cypher {
        with_local_refs(|refs| {
            let mut overlay = GraphBatchReadView::new(&base, &memory, 1, &mut |_| Ok(())).unwrap();
            overlay
                .create(
                    BatchEntityRef::Node(NodeRef::Local(refs.node(0).unwrap())),
                    WriteImage::Node(&node),
                    &mut |_| Ok(()),
                )
                .unwrap();
            overlay.finish_with_results(&mut adapter, &mut |p| {
                if p == WritePhase::Identity {
                    identities += 1;
                }
                Ok(())
            })
        })
    } else {
        stage_structured_with_results(
            &base,
            &[create(EntityKind::Node, "new", WriteImage::Node(&node))],
            &memory,
            &mut adapter,
            &mut |p| {
                if p == WritePhase::Identity {
                    identities += 1;
                }
                Ok(())
            },
        )
    };
    match result {
        Err(error) => assert!(
            matches!(error, StageError::Limit),
            "result layout must reject before generation selection: {error:?}"
        ),
        Ok(_) => panic!("over-cap result layout accepted"),
    }
    assert_eq!(
        identities, 0,
        "result layout validation must precede private IDs"
    );
    assert_eq!(alive.get(), 0);
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    adapter.layout.core_bytes = 1;
    let paired = stage_structured_with_results(
        &base,
        &[create(EntityKind::Node, "new", WriteImage::Node(&node))],
        &memory,
        &mut adapter,
        &mut |_| Ok(()),
    );
    if max_generation {
        assert!(matches!(
            paired,
            Err(StageError::Lifecycle(KeyLifecycleError::GenerationOverflow))
        ));
    } else {
        drop(paired.unwrap());
    }
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
#[test]
fn structured_result_layout_is_validated_before_private_ids() {
    result_layout_preflight_case(false, false)
}
#[test]
fn structured_result_layout_limit_precedes_generation_overflow() {
    result_layout_preflight_case(false, true)
}
#[test]
fn cypher_result_layout_is_validated_before_private_ids() {
    result_layout_preflight_case(true, false)
}
#[test]
fn cypher_result_layout_limit_precedes_generation_overflow() {
    result_layout_preflight_case(true, true)
}

#[test]
fn result_registry_capacity_fits_writer_envelope_before_private_ids() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let alive = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut adapter = Materializer {
        layout: ResultLayout {
            rows: 1,
            core_bytes: 0,
            abi_bytes: 0,
            registry_bytes: 64 * 1024 * 1024,
        },
        alive: alive.clone(),
        fail: false,
    };
    let mut identities = 0;
    assert!(matches!(
        stage_structured_with_results(
            &Empty,
            &[create(EntityKind::Node, "new", WriteImage::Node(&node))],
            &memory,
            &mut adapter,
            &mut |p| {
                if p == WritePhase::Identity {
                    identities += 1;
                }
                Ok(())
            }
        ),
        Err(StageError::Limit)
    ));
    assert_eq!(
        identities, 0,
        "known registry/control capacity must fit the writer envelope before private IDs"
    );
    assert_eq!(alive.get(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
