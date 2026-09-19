#![allow(clippy::expect_used)]
use super::*;

#[test]
fn full_target_sort_and_exact_retry_have_zero_allocator_calls() {
    let mut targets: Vec<_> = (1..=MAX_GRAPH_CHANGES as u128)
        .rev()
        .map(|value| {
            BatchTarget::new(
                None,
                Some(EntityId::Node(
                    super::super::NodeId::new((1_u128 << 100) | value).expect("id"),
                )),
            )
            .expect("target")
        })
        .collect();
    let name = "x".repeat(200_000);
    let key = ApplicationKey::new(EntityKind::Node, "namespace", &name).expect("key");
    let mut labels = [];
    let mut properties = [];
    let image = super::super::CanonicalContents::node(
        &mut labels,
        &mut properties,
        Some("original text"),
        None,
    )
    .expect("contents");
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).expect("bytes");
    let fp = image.fingerprint(&mut || Ok(())).expect("fingerprint");
    let mut left = bytes.as_slice();
    let mut right = bytes.as_slice();
    let id = EntityId::Node(super::super::NodeId::new(1).expect("id"));
    let revision = GraphRevision::new(1).expect("revision");
    let provenance = OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            operation: GraphOperation::StructuredCreate,
            key: Some(key),
            requested_revision: revision,
            installed_revision: revision,
            expected: ExpectedGraphState::Absent,
            incarnation: id,
            delete_mode: None,
            original_generation: GraphGeneration::new(1),
        },
    )
    .expect("provenance");
    let ((sorted, replay), audit) = crate::allocation_audit::audit_engine_path(|| {
        let sorted = validate_distinct_targets(&mut targets, &mut || Ok(()));
        let replay = classify_key(
            key,
            KeyState::Live(CurrentEntity {
                provenance,
                contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
            }),
            KeyRequest::Create {
                revision,
                contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
            },
            &mut [0; 17],
            &mut || Ok(()),
        );
        (sorted, replay)
    });
    assert!(sorted.is_ok());
    assert_eq!(
        replay.expect("exact repeat"),
        KeyDecision::Replay(provenance)
    );
    assert_eq!(audit.allocations, 0);
    assert_eq!(audit.unattributed_bytes, 0);
    assert_eq!(audit.attributed_bytes, 0);
    let target = BatchTarget::new(Some(key), Some(id)).expect("target");
    let (duplicate, audit) = crate::allocation_audit::audit_engine_path(|| {
        validate_distinct_targets(&mut [target, target], &mut || Ok(()))
    });
    assert!(matches!(duplicate, Err(KeyLifecycleError::DuplicateTarget)));
    assert_eq!(audit.allocations, 0);
    let (control, positive) =
        crate::allocation_audit::audit_engine_path(|| std::hint::black_box(vec![0_u8; 17]));
    assert_eq!(control.len(), 17);
    assert_eq!(positive.allocations, 1);
    assert_eq!(positive.unattributed_bytes, 17);
    eprintln!(
        "targets={} key_bytes={} success_allocator_calls=0 duplicate_allocator_calls=0 positive_control_bytes=17",
        targets.len(),
        name.len()
    );
}
