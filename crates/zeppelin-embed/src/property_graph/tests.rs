#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn all_scalar_and_nonempty_list_kinds_retain_their_values_without_vectors() {
    assert!(matches!(
        PropertyValue::new(PropertyData::Bool(true)).unwrap().data(),
        PropertyData::Bool(true)
    ));
    assert!(matches!(
        PropertyValue::new(PropertyData::I64(i64::MIN))
            .unwrap()
            .data(),
        PropertyData::I64(i64::MIN)
    ));
    assert!(matches!(
        PropertyValue::new(PropertyData::Strings(&["猫", ""]))
            .unwrap()
            .data(),
        PropertyData::Strings(["猫", ""])
    ));
    assert!(matches!(
        PropertyValue::new(PropertyData::Bools(&[true, false]))
            .unwrap()
            .data(),
        PropertyData::Bools([true, false])
    ));
    assert!(matches!(
        PropertyValue::new(PropertyData::Integers(&[i64::MIN, i64::MAX]))
            .unwrap()
            .data(),
        PropertyData::Integers([i64::MIN, i64::MAX])
    ));
    let floats = [f64::from_bits(0x7ff8000000000042), -0.0];
    let PropertyData::Floats(values) = PropertyValue::new(PropertyData::Floats(&floats))
        .unwrap()
        .data()
    else {
        panic!("floating list lost its kind");
    };
    assert_eq!(values.first().unwrap().to_bits(), 0x7ff8000000000042);
    assert_eq!(values.last().unwrap().to_bits(), 0x8000000000000000);
}

#[test]
fn raw_name_admission_checks_byte_limit_before_utf8_work() {
    assert_eq!(
        GraphName::from_utf8(&vec![0xff; MAX_GRAPH_INPUT_BYTES + 1]),
        Err(DomainError::InputTooLarge)
    );
    assert_eq!(GraphName::from_utf8("é".as_bytes()).unwrap().as_str(), "é");
}

#[test]
fn batch_local_references_are_bounded_and_distinct_from_durable_ids() {
    with_local_refs(|scope| {
        let node = scope.node(0).unwrap();
        let rel = scope.relationship(16_383).unwrap();
        assert_eq!(node.index(), 0);
        assert_eq!(rel.index(), 16_383);
        assert!(matches!(NodeRef::Local(node), NodeRef::Local(_)));
        assert!(matches!(RelRef::Local(rel), RelRef::Local(_)));
        let id = NodeId::new(u128::MAX).unwrap();
        assert_eq!(NodeRef::Existing(id), NodeRef::Existing(id));
        let id = RelId::new(u128::MAX).unwrap();
        assert_eq!(RelRef::Existing(id), RelRef::Existing(id));
        assert_eq!(
            scope.node(16_384),
            Err(DomainError::LocalReferenceOutOfRange)
        );
        assert_eq!(
            scope.relationship(usize::MAX),
            Err(DomainError::LocalReferenceOutOfRange)
        );
    });
}

#[test]
fn scalar_bits_and_typed_lists_are_distinct_from_finite_vectors() {
    for bits in [
        0,
        1 << 63,
        1,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0042,
    ] {
        let value = PropertyValue::new(PropertyData::F64(f64::from_bits(bits))).unwrap();
        match value.data() {
            PropertyData::F64(value) => assert_eq!(value.to_bits(), bits),
            other => panic!("unexpected scalar {other:?}"),
        }
        assert_eq!(value.payload_bytes(), 8);
    }
    for data in [
        PropertyData::EmptyList { count: 0 },
        PropertyData::Strings(&[]),
        PropertyData::Bools(&[]),
        PropertyData::Integers(&[]),
        PropertyData::Floats(&[]),
    ] {
        let value = PropertyValue::new(data).unwrap();
        assert_eq!(value.list_len(), Some(0));
        assert_eq!(value.payload_bytes(), 0);
    }
    assert!(matches!(
        PropertyValue::new(PropertyData::Floats(&[]))
            .unwrap()
            .data(),
        PropertyData::Floats(_)
    ));
    assert!(matches!(
        PropertyValue::new(PropertyData::EmptyList { count: 0 })
            .unwrap()
            .data(),
        PropertyData::EmptyList { count: 0 }
    ));
    assert_eq!(
        PropertyValue::new(PropertyData::EmptyList { count: 1 }).unwrap_err(),
        DomainError::NonemptyUntypedList
    );
    let list = vec![i64::MIN; MAX_PROPERTY_LIST_ELEMENTS];
    let value = PropertyValue::new(PropertyData::Integers(&list)).unwrap();
    assert_eq!(value.list_len(), Some(524_288));
    assert_eq!(value.payload_bytes(), 4_194_304);
    assert_eq!(
        PropertyValue::new(PropertyData::Bools(&vec![
            true;
            MAX_PROPERTY_LIST_ELEMENTS + 1
        ]))
        .unwrap_err(),
        DomainError::ListTooLong
    );
    let large = "x".repeat(MAX_GRAPH_INPUT_BYTES);
    assert!(PropertyValue::new(PropertyData::String(&large)).is_ok());
    assert_eq!(
        PropertyValue::new(PropertyData::Strings(&[&large, "x"])).unwrap_err(),
        DomainError::InputTooLarge
    );
    let text = PropertyValue::new(PropertyData::String("\0é")).unwrap();
    assert_eq!(text.payload_bytes(), 3);
    assert_eq!(text.list_len(), None);
    let vector = [0.0_f32, -0.0, f32::MIN, f32::MAX];
    let validated = GraphVector::new(&vector, 4).unwrap();
    assert_eq!(validated.coordinates(), vector);
    assert_eq!(
        validated.coordinates().get(1).unwrap().to_bits(),
        0x8000_0000
    );
    assert_eq!(validated.payload_bytes(), 16);
    assert_eq!(
        GraphVector::new(&[], 0).unwrap_err(),
        DomainError::VectorDimensions
    );
    assert_eq!(
        GraphVector::new(&vector, 3).unwrap_err(),
        DomainError::VectorDimensions
    );
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            GraphVector::new(&[invalid], 1).unwrap_err(),
            DomainError::NonfiniteVector
        );
    }
}

#[test]
fn application_keys_and_metadata_preserve_kind_and_exact_utf8() {
    let node = ApplicationKey::new(EntityKind::Node, "source", "é").unwrap();
    let rel = ApplicationKey::new(EntityKind::Relationship, "source", "é").unwrap();
    assert_ne!(node, rel);
    assert_ne!(
        node,
        ApplicationKey::new(EntityKind::Node, "Source", "é").unwrap()
    );
    assert_ne!(
        node,
        ApplicationKey::new(EntityKind::Node, "source", "e\u{301}").unwrap()
    );
    assert_eq!(node.namespace().as_str(), "source");
    assert_eq!(node.key().as_str(), "é");
    assert_eq!(node.kind(), EntityKind::Node);
    assert_eq!(GraphName::from_utf8(&[0xff]), Err(DomainError::InvalidUtf8));
    assert_eq!(GraphName::new("\0").unwrap().as_str(), "\0");
    assert_eq!(GraphName::new("").unwrap().as_str(), "");
    assert_eq!(
        GraphName::new(&"a".repeat(MAX_GRAPH_INPUT_BYTES + 1)),
        Err(DomainError::InputTooLarge)
    );
    let id = EntityId::Node(NodeId::new((1_u128 << 100) + 7).unwrap());
    let revision = GraphRevision::new(4).unwrap();
    let generation = GraphGeneration::new(u64::MAX);
    let metadata = EntityMetadata::new(id, Some(node), revision, generation).unwrap();
    assert_eq!(metadata.id(), id);
    assert_eq!(metadata.key(), Some(node));
    assert_eq!(metadata.revision(), revision);
    assert_eq!(metadata.last_change_generation().get(), u64::MAX);
    assert_eq!(
        EntityMetadata::new(id, Some(rel), revision, generation),
        Err(DomainError::EntityKindMismatch)
    );
    assert!(EntityMetadata::new(id, None, revision, GraphGeneration::new(0)).is_ok());
}

#[test]
fn full_width_identity_and_checked_revisions() {
    assert_eq!(NodeId::new(0), Err(DomainError::ZeroIdentity));
    assert_eq!(RelId::new(0), Err(DomainError::ZeroIdentity));
    assert_eq!(StoreInstanceId::new(0), Err(DomainError::ZeroIdentity));
    for bits in [1, 1_u128 << 64, (1_u128 << 64) + 1, u128::MAX] {
        assert_eq!(NodeId::new(bits).unwrap().get(), bits);
        assert_eq!(RelId::new(bits).unwrap().get(), bits);
        assert_eq!(StoreInstanceId::new(bits).unwrap().get(), bits);
    }
    assert_ne!(NodeId::new(1), NodeId::new((1_u128 << 64) + 1));
    assert_eq!(GraphRevision::new(0), Err(DomainError::ZeroRevision));
    assert_eq!(
        GraphRevision::new(41)
            .unwrap()
            .checked_next()
            .unwrap()
            .get(),
        42
    );
    assert_eq!(
        GraphRevision::new(u64::MAX).unwrap().checked_next(),
        Err(DomainError::RevisionOverflow)
    );
}
