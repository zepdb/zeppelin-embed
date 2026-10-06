#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use crate::{
    allocation_audit::audit_engine_path,
    fts::tokenizer::{TokenizerConfig, TokenizerEpoch},
    lifecycle::{OpenOptions, Store},
};
struct Base(GraphInterpretation<'static>);
impl AdmittedBase for Base {
    fn identity(&self) -> BaseIdentity {
        BaseIdentity {
            store: StoreInstanceId::new(1).unwrap(),
            generation: GraphGeneration::new(0),
            fold: Default::default(),
            roots: None,
        }
    }
    fn high_waters(&self) -> HighWaters {
        HighWaters::default()
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        self.0
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
        panic!("create has no incident probe")
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
#[test]
fn staging_owns_actual_capacities_and_audit_detects_unattributed_allocations() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = resources::GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let base = Base(
        GraphInterpretation::new(TokenizerEpoch::of(&TokenizerConfig::text_default()), None)
            .unwrap(),
    );
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let mut props = [GraphProperty::new(
        GraphName::new("v").unwrap(),
        PropertyValue::new(PropertyData::F64(f64::from_bits(0x7ff8000000001234))).unwrap(),
    )];
    let image = CanonicalContents::node(&mut [], &mut props, Some("audit"), None).unwrap();
    let request = StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "audit", "node").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    };
    let (staged, audit) =
        audit_engine_path(|| stage_structured(&base, &[request], &memory, &mut |_| Ok(())));
    let staged = staged.unwrap();
    assert_eq!(
        audit.unattributed_bytes, 0,
        "staging allocation must have a charged owner"
    );
    assert!(audit.attributed_bytes > 0);
    assert!(audit.allocations > 0);
    assert_eq!(
        resources.reserved_bytes().unwrap() - baseline,
        memory.reserved_bytes() as u64
    );
    assert!(resources.peak_reserved_bytes().unwrap() >= resources.reserved_bytes().unwrap());
    let live = memory.reserved_bytes();
    drop(staged);
    assert_eq!(memory.reserved_bytes(), 0);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    let limited = WriteMemory::new(
        &resources,
        WriteLimits {
            writer_bytes: 1,
            ..WriteLimits::default()
        },
    )
    .unwrap();
    let (rejected, denied) =
        audit_engine_path(|| stage_structured(&base, &[request], &limited, &mut |_| Ok(())));
    assert!(matches!(rejected, Err(StageError::Limit)));
    assert_eq!(denied.allocations, 0);
    let (_, can_fire) = audit_engine_path(|| std::hint::black_box(Vec::<u8>::with_capacity(17)));
    assert_eq!(can_fire.unattributed_bytes, 17);
    println!(
        "ZE37 attributed_bytes={} allocations={} retained_capacity={live} unattributed_bytes=0 denied_allocations=0 can_fire_unattributed=17",
        audit.attributed_bytes, audit.allocations
    );
}

#[test]
fn preflight_provenance_sizing_matches_frozen_v1_lengths_and_encoder_refusals() {
    use super::super::provenance::measure_operation_framing;
    // Hand-worked v1 lengths: unkeyed absent/entity/deletion =51/68/59;
    // key namespace n\0 plus key λ adds21 bytes, including both length words.
    for kind in [EntityKind::Node, EntityKind::Relationship] {
        let id = match kind {
            EntityKind::Node => EntityId::Node(NodeId::new((1u128 << 127) + 9).unwrap()),
            EntityKind::Relationship => {
                EntityId::Relationship(RelId::new((1u128 << 127) + 9).unwrap())
            }
        };
        for keyed in [false, true] {
            let key = keyed.then(|| ApplicationKey::new(kind, "n\0", "λ").unwrap());
            for (expected, plain, keyed_length) in [
                (ExpectedGraphState::Absent, 51, 72),
                (ExpectedGraphState::Entity(id), 68, 89),
                (
                    ExpectedGraphState::Deletion(GraphRevision::new(7).unwrap()),
                    59,
                    80,
                ),
            ] {
                for operation in [
                    GraphOperation::StructuredCreate,
                    GraphOperation::StructuredPut,
                    GraphOperation::StructuredDelete,
                    GraphOperation::StructuredRecreate,
                    GraphOperation::CypherEdit,
                ] {
                    let expected_length = if keyed { keyed_length } else { plain };
                    let fields = OperationFields {
                        operation,
                        key,
                        requested_revision: GraphRevision::new(u64::MAX).unwrap(),
                        installed_revision: GraphRevision::new(u64::MAX).unwrap(),
                        expected,
                        incarnation: id,
                        delete_mode: Some(GraphDeleteMode::Detach),
                        original_generation: GraphGeneration::new(u64::MAX),
                    };
                    let actual =
                        OperationProvenance::from_fields_with_control(Some(1), fields, &mut || {
                            Ok(())
                        })
                        .unwrap();
                    let mut bytes = Vec::new();
                    actual.write_to(&mut bytes, &mut || Ok(())).unwrap();
                    assert_eq!(
                        measure_operation_framing(key, expected, &mut || Ok(())).unwrap(),
                        expected_length
                    );
                    assert_eq!(actual.encoded_len(), expected_length);
                    assert_eq!(bytes.len() as u64, expected_length);
                    assert!(matches!(
                        measure_operation_framing(key, expected, &mut || Err(
                            CanonicalError::Cancelled
                        )),
                        Err(CanonicalError::Cancelled)
                    ));
                    assert!(matches!(
                        OperationProvenance::from_fields_with_control(
                            Some(1),
                            fields,
                            &mut || Err(CanonicalError::Cancelled)
                        ),
                        Err(CanonicalError::Cancelled)
                    ));
                }
            }
        }
    }
    let huge = "x".repeat(MAX_GRAPH_INPUT_BYTES - 1);
    let key = ApplicationKey::new(EntityKind::Node, "n", &huge).unwrap();
    assert!(matches!(
        measure_operation_framing(Some(key), ExpectedGraphState::Absent, &mut || Ok(())),
        Err(CanonicalError::InputTooLarge)
    ));
    assert!(matches!(
        OperationProvenance::from_fields_with_control(
            Some(1),
            OperationFields {
                operation: GraphOperation::StructuredCreate,
                key: Some(key),
                requested_revision: GraphRevision::new(1).unwrap(),
                installed_revision: GraphRevision::new(1).unwrap(),
                expected: ExpectedGraphState::Absent,
                incarnation: EntityId::Node(NodeId::new(1).unwrap()),
                delete_mode: None,
                original_generation: GraphGeneration::new(1)
            },
            &mut || Ok(())
        ),
        Err(CanonicalError::InputTooLarge)
    ));
}

#[test]
fn prepared_relationship_length_never_materializes_unresolved_endpoint_zeroes() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = resources::GraphResources::from_store(&store).unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let memory = WriteMemory::new(&resources, WriteLimits::default()).unwrap();
    let base = Base(
        GraphInterpretation::new(TokenizerEpoch::of(&TokenizerConfig::text_default()), None)
            .unwrap(),
    );
    let source = NodeId::new((1u128 << 127) + 1).unwrap();
    let target = NodeId::new(u128::MAX).unwrap();
    let name = GraphName::new("R").unwrap();
    let image = WriteImage::Relationship {
        source: NodeRef::Existing(source),
        target: NodeRef::Existing(target),
        relationship_type: name,
        properties: &[],
    };
    let prepared = encode::PreparedImage::new(image, &base, &memory, &mut |_| Ok(())).unwrap();
    assert_eq!(prepared.len(), 58);
    assert!(matches!(
        prepared.encode(None, &mut |_| Ok(())),
        Err(StageError::Endpoint)
    ));
    let prepared = encode::PreparedImage::new(image, &base, &memory, &mut |_| Ok(())).unwrap();
    let encoded = prepared
        .encode(Some((source, target)), &mut |_| Ok(()))
        .unwrap();
    let canonical = CanonicalContents::relationship(source, target, name, &mut []).unwrap();
    let mut expected = Vec::new();
    canonical.write_to(&mut expected, &mut || Ok(())).unwrap();
    assert_eq!(&*encoded.bytes, expected);
    assert_eq!(encoded.bytes.len(), 58);
    drop(encoded);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
