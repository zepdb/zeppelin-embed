#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::io::Cursor;
use zeppelin_embed::property_graph::*;

fn key() -> ApplicationKey<'static> {
    ApplicationKey::new(EntityKind::Node, "source", "chunk").expect("key")
}
fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).expect("revision")
}
fn node(value: u128) -> EntityId {
    EntityId::Node(NodeId::new(value).expect("node"))
}

fn image(value: i64) -> (Vec<u8>, CanonicalFingerprint) {
    let mut properties = [GraphProperty::new(
        GraphName::new("value").expect("name"),
        PropertyValue::new(PropertyData::I64(value)).expect("value"),
    )];
    let mut labels = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).expect("image");
    assert_eq!(image.shape(), EntityShape::Node);
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).expect("bytes");
    (
        bytes,
        image.fingerprint(&mut || Ok(())).expect("fingerprint"),
    )
}

fn disposition(result: &Result<KeyDecision<'_>, KeyLifecycleError>) -> &'static str {
    match result {
        Ok(KeyDecision::Change(_)) => "change",
        Ok(KeyDecision::Replay(_)) => "replay",
        Ok(KeyDecision::NoOp) => "noop",
        Err(KeyLifecycleError::MissingKey) => "missing",
        Err(KeyLifecycleError::Stale { .. }) => "stale",
        Err(KeyLifecycleError::RevisionConflict) => "conflict",
        Err(KeyLifecycleError::AlreadyExists) => "exists",
        Err(KeyLifecycleError::DeletedKey) => "deleted",
        Err(KeyLifecycleError::NotDeleted) => "not-deleted",
        Err(KeyLifecycleError::IncarnationConflict) => "incarnation",
        Err(KeyLifecycleError::DeletionRevisionConflict) => "deletion-revision",
        Err(error) => panic!("unexpected disposition: {error}"),
    }
}

#[test]
fn every_key_transition_row_has_explicit_replay_conflict_and_change_outcomes() {
    #[derive(Clone, Copy, Debug)]
    enum S {
        Never,
        Create,
        Put,
        Recreate,
        Deleted,
        Cypher,
    }
    #[derive(Clone, Copy, Debug)]
    enum R {
        Create,
        Put,
        Delete,
        Recreate,
    }
    // State at revision7, request kind/revision, expected ID, deletion revision,
    // changed bytes, delete mode, and independent expected disposition.
    let rows = [
        (
            S::Never,
            R::Create,
            1,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "change",
        ),
        (
            S::Never,
            R::Put,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "missing",
        ),
        (
            S::Never,
            R::Delete,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "missing",
        ),
        (
            S::Never,
            R::Recreate,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "missing",
        ),
        (
            S::Create,
            R::Create,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Create,
            R::Create,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "replay",
        ),
        (
            S::Create,
            R::Create,
            7,
            9,
            7,
            true,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Create,
            R::Create,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "exists",
        ),
        (
            S::Create,
            R::Put,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Create,
            R::Put,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Create,
            R::Put,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "change",
        ),
        (
            S::Create,
            R::Delete,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Create,
            R::Delete,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Create,
            R::Delete,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "change",
        ),
        (
            S::Create,
            R::Recreate,
            6,
            9,
            6,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Create,
            R::Recreate,
            7,
            9,
            6,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Create,
            R::Recreate,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "not-deleted",
        ),
        (
            S::Put,
            R::Put,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "replay",
        ),
        (
            S::Put,
            R::Put,
            7,
            9,
            7,
            true,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Put,
            R::Put,
            6,
            10,
            7,
            false,
            GraphDeleteMode::Detach,
            "incarnation",
        ),
        (
            S::Put,
            R::Put,
            1000,
            10,
            7,
            false,
            GraphDeleteMode::Detach,
            "incarnation",
        ),
        (
            S::Recreate,
            R::Recreate,
            7,
            9,
            6,
            false,
            GraphDeleteMode::Detach,
            "replay",
        ),
        (
            S::Recreate,
            R::Recreate,
            7,
            9,
            5,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Recreate,
            R::Recreate,
            7,
            9,
            6,
            true,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Recreate,
            R::Put,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "change",
        ),
        (
            S::Deleted,
            R::Create,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Deleted,
            R::Create,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Deleted,
            R::Create,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "deleted",
        ),
        (
            S::Deleted,
            R::Put,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Deleted,
            R::Put,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Deleted,
            R::Put,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "deleted",
        ),
        (
            S::Deleted,
            R::Delete,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Deleted,
            R::Delete,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "replay",
        ),
        (
            S::Deleted,
            R::Delete,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Restrict,
            "conflict",
        ),
        (
            S::Deleted,
            R::Delete,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "deleted",
        ),
        (
            S::Deleted,
            R::Delete,
            1000,
            10,
            7,
            false,
            GraphDeleteMode::Detach,
            "incarnation",
        ),
        (
            S::Deleted,
            R::Recreate,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Deleted,
            R::Recreate,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Deleted,
            R::Recreate,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "change",
        ),
        (
            S::Deleted,
            R::Recreate,
            8,
            9,
            6,
            false,
            GraphDeleteMode::Detach,
            "deletion-revision",
        ),
        (
            S::Cypher,
            R::Create,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Cypher,
            R::Put,
            6,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "stale",
        ),
        (
            S::Cypher,
            R::Put,
            7,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "conflict",
        ),
        (
            S::Cypher,
            R::Put,
            8,
            9,
            7,
            false,
            GraphDeleteMode::Detach,
            "change",
        ),
    ];
    for (state, operation, rev, expected_id, deleted_rev, changed, mode, expected) in rows {
        let (base, fp) = image(1);
        let (next, next_fp) = image(if changed { 2 } else { 1 });
        let mut left = Cursor::new(&base);
        let mut right = Cursor::new(&next);
        let provenance = match state {
            S::Never | S::Create => installed(
                GraphOperation::StructuredCreate,
                7,
                ExpectedGraphState::Absent,
                node(9),
                None,
                10,
            ),
            S::Put => installed(
                GraphOperation::StructuredPut,
                7,
                ExpectedGraphState::Entity(node(9)),
                node(9),
                None,
                10,
            ),
            S::Recreate => installed(
                GraphOperation::StructuredRecreate,
                7,
                ExpectedGraphState::Deletion(revision(6)),
                node(9),
                None,
                10,
            ),
            S::Deleted => installed(
                GraphOperation::StructuredDelete,
                7,
                ExpectedGraphState::Entity(node(9)),
                node(9),
                Some(GraphDeleteMode::Detach),
                10,
            ),
            S::Cypher => installed(
                GraphOperation::CypherEdit,
                7,
                ExpectedGraphState::Entity(node(9)),
                node(9),
                None,
                10,
            ),
        };
        let admitted = match state {
            S::Never => KeyState::NeverUsed,
            S::Deleted => KeyState::Deleted(provenance),
            _ => KeyState::Live(CurrentEntity {
                provenance,
                contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
            }),
        };
        let contents = CanonicalRecord::from_validated(EntityShape::Node, next_fp, &mut right);
        let request = match operation {
            R::Create => KeyRequest::Create {
                revision: revision(rev),
                contents,
            },
            R::Put => KeyRequest::Put {
                revision: revision(rev),
                expected: node(expected_id),
                contents,
            },
            R::Delete => KeyRequest::Delete {
                revision: revision(rev),
                expected: node(expected_id),
                mode,
            },
            R::Recreate => KeyRequest::Recreate {
                revision: revision(rev),
                deleted_revision: revision(deleted_rev),
                contents,
            },
        };
        let result = classify_key(key(), admitted, request, &mut [0; 17], &mut || Ok(()));
        assert_eq!(
            disposition(&result),
            expected,
            "{state:?} {operation:?} revision={rev} expected_id={expected_id} deleted={deleted_rev}"
        );
        if let Ok(KeyDecision::Replay(replayed)) = result {
            assert_eq!(replayed, provenance);
        }
        if let Err(KeyLifecycleError::Stale { current }) = result {
            assert_eq!(current, revision(7));
        }
    }
}

#[test]
fn never_used_key_create_stages_identity_and_explicit_provenance() {
    let (bytes, fingerprint) = image(42);
    let mut source = Cursor::new(bytes);
    let request = KeyRequest::Create {
        revision: revision(7),
        contents: CanonicalRecord::from_validated(EntityShape::Node, fingerprint, &mut source),
    };
    let decision = classify_key(
        key(),
        KeyState::NeverUsed,
        request,
        &mut [0; 64],
        &mut || Ok(()),
    )
    .expect("new create");
    let KeyDecision::Change(change) = decision else {
        panic!("new create must change")
    };
    assert_eq!(change.existing_incarnation(), None);
    assert!(!change.is_deletion());
    let provenance = change
        .install(
            node((1_u128 << 64) + 1),
            GraphGeneration::new(9),
            &mut || Ok(()),
        )
        .expect("private allocation finalization");
    assert_eq!(
        provenance.fields(),
        OperationFields {
            operation: GraphOperation::StructuredCreate,
            key: Some(key()),
            requested_revision: revision(7),
            installed_revision: revision(7),
            expected: ExpectedGraphState::Absent,
            incarnation: node((1_u128 << 64) + 1),
            delete_mode: None,
            original_generation: GraphGeneration::new(9),
        }
    );
}

fn installed(
    operation: GraphOperation,
    rev: u64,
    expected: ExpectedGraphState,
    id: EntityId,
    mode: Option<GraphDeleteMode>,
    generation: u64,
) -> OperationProvenance<'static> {
    OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            operation,
            key: Some(key()),
            requested_revision: revision(rev),
            installed_revision: revision(rev),
            expected,
            incarnation: id,
            delete_mode: mode,
            original_generation: GraphGeneration::new(generation),
        },
    )
    .expect("installed record")
}

#[test]
fn exact_create_retry_preserves_original_outcome_and_rejects_equal_hash_drift() {
    let installed = installed(
        GraphOperation::StructuredCreate,
        7,
        ExpectedGraphState::Absent,
        node(9),
        None,
        10,
    );
    let (base, fingerprint) = image(1);
    for (candidate, expect_replay) in [(image(1).0, true), (image(2).0, false)] {
        let mut left = Cursor::new(&base);
        let mut right = Cursor::new(&candidate);
        let state = KeyState::Live(CurrentEntity {
            provenance: installed,
            contents: CanonicalRecord::from_validated(EntityShape::Node, fingerprint, &mut left),
        });
        // Deliberately equal hash/length, with potentially different real bytes.
        let request = KeyRequest::Create {
            revision: revision(7),
            contents: CanonicalRecord::from_validated(EntityShape::Node, fingerprint, &mut right),
        };
        let decision = classify_key(key(), state, request, &mut [0; 9], &mut || Ok(()));
        if expect_replay {
            let KeyDecision::Replay(provenance) = decision.expect("exact retry") else {
                panic!("expected replay")
            };
            assert_eq!(provenance, installed);
        } else {
            assert!(matches!(decision, Err(KeyLifecycleError::RevisionConflict)));
        }
    }
}

#[test]
fn put_delete_recreate_preserve_fences_and_reject_old_incarnations() {
    let original = installed(
        GraphOperation::StructuredCreate,
        7,
        ExpectedGraphState::Absent,
        node(9),
        None,
        10,
    );
    let (base, fp) = image(1);
    let (changed, changed_fp) = image(2);
    let mut left = Cursor::new(&base);
    let mut right = Cursor::new(&changed);
    let current = CurrentEntity {
        provenance: original,
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
    };
    let request = KeyRequest::Put {
        revision: revision(8),
        expected: node(9),
        contents: CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut right),
    };
    let KeyDecision::Change(put) = classify_key(
        key(),
        KeyState::Live(current),
        request,
        &mut [0; 64],
        &mut || Ok(()),
    )
    .expect("put") else {
        panic!("change")
    };
    assert_eq!(put.existing_incarnation(), Some(node(9)));
    let replaced = put
        .install(node(9), GraphGeneration::new(11), &mut || Ok(()))
        .expect("preserve ID");
    assert_eq!(replaced.fields().operation, GraphOperation::StructuredPut);
    assert_eq!(
        replaced.fields().expected,
        ExpectedGraphState::Entity(node(9))
    );
    let mut left = Cursor::new(&changed);
    let current = CurrentEntity {
        provenance: replaced,
        contents: CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut left),
    };
    let request = KeyRequest::Delete {
        revision: revision(9),
        expected: node(9),
        mode: GraphDeleteMode::Detach,
    };
    let KeyDecision::Change(delete) = classify_key(
        key(),
        KeyState::Live(current),
        request,
        &mut [0; 64],
        &mut || Ok(()),
    )
    .expect("delete") else {
        panic!("change")
    };
    assert!(delete.is_deletion());
    let fence = delete
        .install(node(9), GraphGeneration::new(12), &mut || Ok(()))
        .expect("fence");
    let retry = KeyRequest::Delete {
        revision: revision(9),
        expected: node(9),
        mode: GraphDeleteMode::Detach,
    };
    let KeyDecision::Replay(replayed) = classify_key(
        key(),
        KeyState::Deleted(fence),
        retry,
        &mut [0; 64],
        &mut || Ok(()),
    )
    .expect("delete retry") else {
        panic!("replay")
    };
    assert_eq!(replayed, fence);
    let mut right = Cursor::new(&base);
    let request = KeyRequest::Recreate {
        revision: revision(10),
        deleted_revision: revision(9),
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
    };
    let KeyDecision::Change(recreate) = classify_key(
        key(),
        KeyState::Deleted(fence),
        request,
        &mut [0; 64],
        &mut || Ok(()),
    )
    .expect("explicit recreate") else {
        panic!("change")
    };
    assert_eq!(recreate.existing_incarnation(), None);
    assert!(matches!(
        recreate.install(node(9), GraphGeneration::new(13), &mut || Ok(())),
        Err(KeyLifecycleError::InvalidInstalledIdentity)
    ));
    let new = recreate
        .install(
            node((1_u128 << 64) + 9),
            GraphGeneration::new(13),
            &mut || Ok(()),
        )
        .expect("new full-width incarnation");
    for request_rev in [9, 10, 1000] {
        let mut left = Cursor::new(&base);
        let current = CurrentEntity {
            provenance: new,
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
        };
        let late = KeyRequest::Delete {
            revision: revision(request_rev),
            expected: node(9),
            mode: GraphDeleteMode::Detach,
        };
        assert!(
            matches!(
                classify_key(
                    key(),
                    KeyState::Live(current),
                    late,
                    &mut [0; 64],
                    &mut || Ok(())
                ),
                Err(KeyLifecycleError::IncarnationConflict)
            ),
            "old delete at revision {request_rev}"
        );
    }
}

#[test]
fn cypher_final_state_changes_advance_once_and_never_create_retry_receipts() {
    let original = installed(
        GraphOperation::StructuredPut,
        7,
        ExpectedGraphState::Entity(node(9)),
        node(9),
        None,
        10,
    );
    let (base, fp) = image(1);
    let (changed, changed_fp) = image(2);
    let mut left = Cursor::new(&base);
    let mut right = Cursor::new(&changed);
    let current = CurrentEntity {
        provenance: original,
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
    };
    let final_image = CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut right);
    let KeyDecision::Change(edit) = classify_cypher(
        Some(current),
        CypherEdit::Put(final_image),
        &mut [0; 64],
        &mut || Ok(()),
    )
    .expect("one final edit") else {
        panic!("changed")
    };
    let edited = edit
        .install(node(9), GraphGeneration::new(11), &mut || Ok(()))
        .expect("install");
    assert_eq!(edited.fields().installed_revision, revision(8));
    assert_eq!(edited.fields().operation, GraphOperation::CypherEdit);
    assert_eq!(
        edited.fields().expected,
        ExpectedGraphState::Entity(node(9))
    );
    let mut left = Cursor::new(&changed);
    let mut right = Cursor::new(&changed);
    let current = CurrentEntity {
        provenance: edited,
        contents: CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut left),
    };
    let final_image = CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut right);
    assert_eq!(
        classify_cypher(
            Some(current),
            CypherEdit::Put(final_image),
            &mut [0; 64],
            &mut || Ok(())
        )
        .expect("same final contents"),
        KeyDecision::NoOp
    );
    let mut left = Cursor::new(&changed);
    let mut right = Cursor::new(&changed);
    let current = CurrentEntity {
        provenance: edited,
        contents: CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut left),
    };
    let request = KeyRequest::Put {
        revision: revision(8),
        expected: node(9),
        contents: CanonicalRecord::from_validated(EntityShape::Node, changed_fp, &mut right),
    };
    assert!(
        matches!(
            classify_key(
                key(),
                KeyState::Live(current),
                request,
                &mut [0; 64],
                &mut || Ok(())
            ),
            Err(KeyLifecycleError::RevisionConflict)
        ),
        "same contents cannot relabel a Cypher install as structured retry"
    );
    for same in [true, false] {
        let max = installed(
            GraphOperation::StructuredPut,
            u64::MAX,
            ExpectedGraphState::Entity(node(9)),
            node(9),
            None,
            10,
        );
        let mut left = Cursor::new(&base);
        let mut right = Cursor::new(if same { &base } else { &changed });
        let current = CurrentEntity {
            provenance: max,
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
        };
        let final_image = CanonicalRecord::from_validated(
            EntityShape::Node,
            if same { fp } else { changed_fp },
            &mut right,
        );
        let result = classify_cypher(
            Some(current),
            CypherEdit::Put(final_image),
            &mut [0; 64],
            &mut || Ok(()),
        );
        if same {
            assert_eq!(result.expect("no increment needed"), KeyDecision::NoOp);
        } else {
            assert!(matches!(result, Err(KeyLifecycleError::RevisionOverflow)));
        }
    }
    assert_eq!(
        classify_cypher(
            None,
            CypherEdit::Delete(GraphDeleteMode::Detach),
            &mut [],
            &mut || Ok(())
        )
        .expect("missing/null target"),
        KeyDecision::NoOp
    );
}

#[test]
fn structured_batches_reject_every_repeated_key_or_entity_target() {
    let first = BatchTarget::new(Some(key()), Some(node(1))).expect("target");
    assert!(
        matches!(
            validate_distinct_targets(&mut [first, first], &mut || Ok(())),
            Err(KeyLifecycleError::DuplicateTarget)
        ),
        "identical duplicate is invalid too"
    );
    let same_key = BatchTarget::new(Some(key()), Some(node(2))).expect("same key");
    assert!(matches!(
        validate_distinct_targets(&mut [first, same_key], &mut || Ok(())),
        Err(KeyLifecycleError::DuplicateTarget)
    ));
    let other_key = ApplicationKey::new(EntityKind::Node, "source", "other").expect("other key");
    let same_entity = BatchTarget::new(Some(other_key), Some(node(1))).expect("same entity");
    assert!(matches!(
        validate_distinct_targets(&mut [first, same_entity], &mut || Ok(())),
        Err(KeyLifecycleError::DuplicateTarget)
    ));
    let relationship_key =
        ApplicationKey::new(EntityKind::Relationship, "source", "chunk").expect("other domain");
    let relationship = BatchTarget::new(
        Some(relationship_key),
        Some(EntityId::Relationship(RelId::new(1).expect("rel"))),
    )
    .expect("other identity domain");
    validate_distinct_targets(&mut [first, relationship], &mut || Ok(()))
        .expect("full domain separation");
    assert!(matches!(
        BatchTarget::new(None, None),
        Err(KeyLifecycleError::MissingTarget)
    ));
    assert!(matches!(
        BatchTarget::new(Some(relationship_key), Some(node(1))),
        Err(KeyLifecycleError::KindMismatch)
    ));
    assert!(matches!(
        validate_distinct_targets(&mut vec![first; MAX_GRAPH_CHANGES + 1], &mut || Ok(())),
        Err(KeyLifecycleError::TooManyTargets)
    ));
    assert!(matches!(
        validate_distinct_targets(&mut [first], &mut || Err(CanonicalError::Cancelled)),
        Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
    ));
}

#[test]
fn batch_disposition_preserves_per_item_replay_generations_and_real_durable_changes() {
    let first = installed(
        GraphOperation::StructuredCreate,
        1,
        ExpectedGraphState::Absent,
        node(1),
        None,
        2,
    );
    let second = installed(
        GraphOperation::StructuredPut,
        2,
        ExpectedGraphState::Entity(node(2)),
        node(2),
        None,
        9,
    );
    let replays = [KeyDecision::Replay(first), KeyDecision::Replay(second)];
    let summary = summarize_key_batch(GraphGeneration::new(20), &replays, false, &mut || Ok(()))
        .expect("all replays");
    assert_eq!(summary.disposition, BatchDisposition::Replayed);
    assert_eq!(summary.changed_generation, None);
    assert_eq!(summary.replayed_items, 2);
    assert_eq!(
        replays,
        [KeyDecision::Replay(first), KeyDecision::Replay(second)]
    );
    assert_eq!(
        summarize_key_batch(GraphGeneration::new(u64::MAX), &replays, false, &mut || Ok(
            ()
        ))
        .expect("replay never increments")
        .changed_generation,
        None
    );
    let noop = summarize_key_batch(
        GraphGeneration::new(20),
        &[KeyDecision::NoOp],
        false,
        &mut || Ok(()),
    )
    .expect("effect-free");
    assert_eq!(noop.disposition, BatchDisposition::NoOp);
    assert_eq!(noop.admitted_generation, GraphGeneration::new(20));
    let consumed_ids = summarize_key_batch(
        GraphGeneration::new(20),
        &[KeyDecision::NoOp],
        true,
        &mut || Ok(()),
    )
    .expect("create then delete retained allocator change");
    assert_eq!(consumed_ids.disposition, BatchDisposition::Changed);
    assert_eq!(
        consumed_ids.changed_generation,
        Some(GraphGeneration::new(21))
    );
    let (bytes, fp) = image(1);
    let mut source = Cursor::new(bytes);
    let create = KeyRequest::Create {
        revision: revision(1),
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut source),
    };
    let changed =
        classify_key(key(), KeyState::NeverUsed, create, &mut [], &mut || Ok(())).expect("create");
    let mixed = summarize_key_batch(
        GraphGeneration::new(20),
        &[replays[0], changed, replays[1]],
        false,
        &mut || Ok(()),
    )
    .expect("mixed distinct targets");
    assert_eq!(mixed.disposition, BatchDisposition::Changed);
    assert_eq!(mixed.changed_items, 1);
    assert_eq!(mixed.replayed_items, 2);
    assert_eq!(mixed.changed_generation, Some(GraphGeneration::new(21)));
    assert!(matches!(
        summarize_key_batch(
            GraphGeneration::new(u64::MAX),
            &[changed],
            false,
            &mut || Ok(())
        ),
        Err(KeyLifecycleError::GenerationOverflow)
    ));
}

#[test]
fn relationship_puts_preserve_full_directed_endpoints_and_exact_type() {
    let relation_key = ApplicationKey::new(EntityKind::Relationship, "ns", "edge").expect("key");
    let id = EntityId::Relationship(RelId::new((1_u128 << 64) + 9).expect("ID"));
    let prov = OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            operation: GraphOperation::StructuredCreate,
            key: Some(relation_key),
            requested_revision: revision(7),
            installed_revision: revision(7),
            expected: ExpectedGraphState::Absent,
            incarnation: id,
            delete_mode: None,
            original_generation: GraphGeneration::new(10),
        },
    )
    .expect("provenance");
    let source = NodeId::new((1_u128 << 64) + 1).expect("source");
    let target = NodeId::new(1).expect("target");
    let make = |source, target, kind: &'static str, value| {
        let mut props = [GraphProperty::new(
            GraphName::new("p").expect("name"),
            PropertyValue::new(PropertyData::I64(value)).expect("value"),
        )];
        let record = CanonicalContents::relationship(
            source,
            target,
            GraphName::new(kind).expect("type"),
            &mut props,
        )
        .expect("record");
        let mut bytes = Vec::new();
        record.write_to(&mut bytes, &mut || Ok(())).expect("bytes");
        let shape = EntityShape::Relationship {
            source,
            target,
            relationship_type: GraphName::new(kind).expect("type"),
        };
        assert_eq!(record.shape(), shape);
        (
            bytes,
            record.fingerprint(&mut || Ok(())).expect("fingerprint"),
            shape,
        )
    };
    let (base, fp, shape) = make(source, target, "TYPE", 1);
    assert_eq!(shape.kind(), EntityKind::Relationship);
    for (s, t, kind, allowed) in [
        (source, target, "TYPE", true),
        (target, source, "TYPE", false),
        (target, target, "TYPE", false),
        (source, target, "type", false),
    ] {
        let (next, next_fp, next_shape) = make(s, t, kind, 2);
        let mut left = Cursor::new(&base);
        let mut right = Cursor::new(&next);
        let state = KeyState::Live(CurrentEntity {
            provenance: prov,
            contents: CanonicalRecord::from_validated(shape, fp, &mut left),
        });
        let request = KeyRequest::Put {
            revision: revision(8),
            expected: id,
            contents: CanonicalRecord::from_validated(next_shape, next_fp, &mut right),
        };
        let result = classify_key(relation_key, state, request, &mut [0; 64], &mut || Ok(()));
        if allowed {
            let KeyDecision::Change(change) = result.expect("property replacement") else {
                panic!("change")
            };
            assert_eq!(
                change
                    .install(id, GraphGeneration::new(11), &mut || Ok(()))
                    .expect("identity retained")
                    .fields()
                    .incarnation,
                id
            );
        } else {
            assert!(matches!(
                result,
                Err(KeyLifecycleError::RelationshipIdentityChange)
            ));
        }
    }
}

#[test]
fn invalid_provenance_kinds_and_private_finalization_fail_loudly() {
    let base = installed(
        GraphOperation::StructuredPut,
        7,
        ExpectedGraphState::Entity(node(9)),
        node(9),
        None,
        10,
    );
    let (data, fp) = image(1);
    for mutation in 0..7 {
        let mut fields = base.fields();
        match mutation {
            0 => fields.key = None,
            1 => {
                fields.key =
                    Some(ApplicationKey::new(EntityKind::Node, "other", "key").expect("other key"))
            }
            2 => fields.requested_revision = revision(6),
            3 => fields.original_generation = GraphGeneration::new(0),
            4 => fields.delete_mode = Some(GraphDeleteMode::Restrict),
            5 => fields.operation = GraphOperation::StructuredDelete,
            _ => fields.expected = ExpectedGraphState::Absent,
        }
        let malformed = OperationProvenance::from_fields(Some(1), fields)
            .expect("domain-valid, lifecycle-invalid");
        let mut left = Cursor::new(&data);
        let mut right = Cursor::new(&data);
        let state = KeyState::Live(CurrentEntity {
            provenance: malformed,
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
        });
        let request = KeyRequest::Put {
            revision: revision(8),
            expected: node(9),
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
        };
        assert!(
            matches!(
                classify_key(key(), state, request, &mut [0; 64], &mut || Ok(())),
                Err(KeyLifecycleError::InvalidState)
            ),
            "invalid field {mutation}"
        );
    }
    let mut source = Cursor::new(&data);
    let request = KeyRequest::Create {
        revision: revision(7),
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut source),
    };
    let KeyDecision::Change(create) =
        classify_key(key(), KeyState::NeverUsed, request, &mut [], &mut || Ok(())).expect("create")
    else {
        panic!("change")
    };
    assert!(matches!(
        create.install(node(1), GraphGeneration::new(0), &mut || Ok(())),
        Err(KeyLifecycleError::InvalidGeneration)
    ));
    assert!(matches!(
        create.install(
            EntityId::Relationship(RelId::new(1).expect("rel")),
            GraphGeneration::new(1),
            &mut || Ok(())
        ),
        Err(KeyLifecycleError::KindMismatch)
    ));
    let mut left = Cursor::new(&data);
    let mut right = Cursor::new(&data);
    let state = KeyState::Live(CurrentEntity {
        provenance: base,
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
    });
    let request = KeyRequest::Put {
        revision: revision(8),
        expected: node(9),
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
    };
    let KeyDecision::Change(put) =
        classify_key(key(), state, request, &mut [], &mut || Ok(())).expect("put")
    else {
        panic!("change")
    };
    assert!(matches!(
        put.install(node(10), GraphGeneration::new(11), &mut || Ok(())),
        Err(KeyLifecycleError::InvalidInstalledIdentity)
    ));
    assert!(matches!(
        put.install(node(9), GraphGeneration::new(10), &mut || Ok(())),
        Err(KeyLifecycleError::InvalidGeneration)
    ));
    let wrong_kind = KeyRequest::Delete {
        revision: revision(8),
        expected: EntityId::Relationship(RelId::new(9).expect("rel")),
        mode: GraphDeleteMode::Restrict,
    };
    assert!(matches!(
        classify_key(key(), KeyState::NeverUsed, wrong_kind, &mut [], &mut || Ok(
            ()
        )),
        Err(KeyLifecycleError::KindMismatch)
    ));
    let huge = "x".repeat(MAX_GRAPH_INPUT_BYTES);
    let target = BatchTarget::new(
        Some(ApplicationKey::new(EntityKind::Node, "", &huge).expect("raw key")),
        None,
    )
    .expect("target");
    assert!(matches!(
        validate_distinct_targets(&mut [target], &mut || Ok(())),
        Err(KeyLifecycleError::InputTooLarge)
    ));
    assert!(matches!(
        summarize_key_batch(
            GraphGeneration::new(0),
            &vec![KeyDecision::NoOp; MAX_GRAPH_CHANGES + 1],
            false,
            &mut || Ok(())
        ),
        Err(KeyLifecycleError::TooManyTargets)
    ));
}

#[test]
fn key_comparison_errors_and_cancellation_never_become_replay() {
    use std::error::Error;
    let prov = installed(
        GraphOperation::StructuredCreate,
        7,
        ExpectedGraphState::Absent,
        node(9),
        None,
        10,
    );
    let (data, fp) = image(1);
    for truncate in [false, true] {
        let mut left = Cursor::new(if truncate {
            &data[..data.len() - 1]
        } else {
            data.as_slice()
        });
        let mut right = Cursor::new(&data);
        let state = KeyState::Live(CurrentEntity {
            provenance: prov,
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
        });
        let request = KeyRequest::Create {
            revision: revision(7),
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
        };
        let mut scratch = [0; 9];
        let error = classify_key(
            key(),
            state,
            request,
            if truncate {
                &mut scratch[..]
            } else {
                &mut [][..]
            },
            &mut || Ok(()),
        )
        .expect_err("invalid source/scratch");
        assert!(error.source().is_some());
        assert!(!error.to_string().is_empty());
        if truncate {
            assert!(matches!(
                error,
                KeyLifecycleError::Canonical(CanonicalError::Io(_))
            ));
        } else {
            assert!(matches!(
                error,
                KeyLifecycleError::Canonical(CanonicalError::InvalidScratch)
            ));
        }
    }
    let mut observed = 0;
    for fail_at in 0..100 {
        let mut left = Cursor::new(&data);
        let mut right = Cursor::new(&data);
        let state = KeyState::Live(CurrentEntity {
            provenance: prov,
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
        });
        let request = KeyRequest::Create {
            revision: revision(7),
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
        };
        let mut visits = 0;
        let result = classify_key(key(), state, request, &mut [0; 9], &mut || {
            let n = visits;
            visits += 1;
            if n == fail_at {
                Err(CanonicalError::Cancelled)
            } else {
                Ok(())
            }
        });
        if matches!(result, Ok(KeyDecision::Replay(_))) {
            assert_eq!(visits, fail_at);
            break;
        }
        assert!(matches!(
            result,
            Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
        ));
        assert_eq!(visits, fail_at + 1);
        observed += 1;
    }
    assert!(
        observed > 2,
        "must interrupt inside exact comparison, not only admission"
    );
}

#[test]
fn cancellation_interrupts_target_sort_before_its_first_comparison_mutates_input() {
    let low = BatchTarget::new(
        Some(ApplicationKey::new(EntityKind::Node, "ns", "a").expect("key")),
        None,
    )
    .expect("target");
    let high = BatchTarget::new(
        Some(ApplicationKey::new(EntityKind::Node, "ns", "z").expect("key")),
        None,
    )
    .expect("target");
    let mut targets = [high, low];
    let mut calls = 0;
    let result = validate_distinct_targets(&mut targets, &mut || {
        calls += 1;
        // Admission and two descriptor checks have completed. Cancel at the
        // first sort work checkpoint, before a comparison can move descriptors.
        if calls == 4 {
            Err(CanonicalError::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(
        result,
        Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
    ));
    assert_eq!(
        targets,
        [high, low],
        "uncancellable sort moved the entire input before observing cancellation"
    );
    validate_distinct_targets(&mut targets, &mut || Ok(())).expect("clean sorting control");
}

#[test]
fn cancellation_interrupts_long_key_comparison_before_reporting_key_mismatch() {
    let long = "x".repeat(200_000);
    let other = long.clone() + "y";
    let attempted = ApplicationKey::new(EntityKind::Node, "ns", &long).expect("key");
    let stored = ApplicationKey::new(EntityKind::Node, "ns", &other).expect("stored key");
    let mut fields = installed(
        GraphOperation::StructuredCreate,
        7,
        ExpectedGraphState::Absent,
        node(9),
        None,
        10,
    )
    .fields();
    fields.key = Some(stored);
    let provenance = OperationProvenance::from_fields(Some(1), fields).expect("provenance");
    let (data, fp) = image(1);
    let mut left = Cursor::new(&data);
    let mut right = Cursor::new(&data);
    let state = KeyState::Live(CurrentEntity {
        provenance,
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left),
    });
    let request = KeyRequest::Create {
        revision: revision(7),
        contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right),
    };
    let mut calls = 0;
    let result = classify_key(attempted, state, request, &mut [0; 64], &mut || {
        calls += 1;
        if calls == 4 {
            Err(CanonicalError::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(
        matches!(
            result,
            Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
        ),
        "long key comparison returned without its chunk cancellation checkpoint: {result:?}"
    );
    assert_eq!(calls, 4);
}

#[test]
fn every_target_order_is_checked_and_cancellation_can_interrupt_each_stage() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;
    let mut seed = test_support::seeded_rng("graph_key_target_permutations");
    let mut runner = TestRunner::new(Config {
        cases: 128,
        source_file: Some(file!()),
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(seed.next_u64()),
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(0_u16..128, 0..256), |ids| {
            let unique = ids
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == ids.len();
            let names: Vec<_> = ids.iter().map(|id| format!("ns\0{id:03}é")).collect();
            let mut targets: Vec<_> = names
                .iter()
                .map(|name| {
                    BatchTarget::new(
                        Some(ApplicationKey::new(EntityKind::Node, "n", name).expect("key")),
                        None,
                    )
                    .expect("target")
                })
                .collect();
            let result = validate_distinct_targets(&mut targets, &mut || Ok(()));
            prop_assert_eq!(result.is_ok(), unique);
            if !unique {
                prop_assert!(matches!(result, Err(KeyLifecycleError::DuplicateTarget)));
            }
            Ok(())
        })
        .expect("independent set expectation");
    let mut clean: Vec<_> = (1..=31)
        .rev()
        .map(|id| BatchTarget::new(None, Some(node(id))).expect("target"))
        .collect();
    let original = clean.clone();
    let mut total = 0;
    validate_distinct_targets(&mut clean, &mut || {
        total += 1;
        Ok(())
    })
    .expect("distinct control");
    assert!(total > original.len() * 2, "checkpoints inside both sorts");
    for cancel in 1..=total {
        let mut targets = original.clone();
        let mut calls = 0;
        assert!(
            matches!(
                validate_distinct_targets(&mut targets, &mut || {
                    calls += 1;
                    if calls == cancel {
                        Err(CanonicalError::Cancelled)
                    } else {
                        Ok(())
                    }
                }),
                Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
            ),
            "checkpoint {cancel}/{total}"
        );
    }
    let decisions = vec![KeyDecision::NoOp; MAX_GRAPH_CHANGES];
    let mut calls = 0;
    assert!(matches!(
        summarize_key_batch(GraphGeneration::new(0), &decisions, false, &mut || {
            calls += 1;
            if calls == 257 {
                Err(CanonicalError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
    ));
    assert_eq!(calls, 257);
    assert_eq!(
        summarize_key_batch(GraphGeneration::new(0), &decisions, false, &mut || Ok(()))
            .expect("full batch")
            .disposition,
        BatchDisposition::NoOp
    );
}

#[path = "../src/test_support.rs"]
mod test_support;

#[test]
fn long_relationship_type_comparison_is_cancellable_and_exact() {
    let prefix = "t".repeat(200_000);
    let different = format!("{prefix}x");
    let shape = |name| EntityShape::Relationship {
        source: NodeId::new(1).expect("source"),
        target: NodeId::new(2).expect("target"),
        relationship_type: GraphName::new(name).expect("type"),
    };
    let rel_key = ApplicationKey::new(EntityKind::Relationship, "", "r").expect("key");
    let id = EntityId::Relationship(RelId::new(1).expect("id"));
    let provenance = OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            key: Some(rel_key),
            operation: GraphOperation::StructuredCreate,
            requested_revision: revision(1),
            expected: ExpectedGraphState::Absent,
            incarnation: id,
            installed_revision: revision(1),
            delete_mode: None,
            original_generation: GraphGeneration::new(1),
        },
    )
    .expect("provenance");
    let fp = CanonicalFingerprint::new(0, 0).expect("metadata");
    // Validated shape comparison must reject before touching either source.
    let mut calls = 0;
    let mut left_source = std::io::empty();
    let mut right_source = std::io::empty();
    let error = classify_key(
        rel_key,
        KeyState::Live(CurrentEntity {
            provenance,
            contents: CanonicalRecord::from_validated(shape(&prefix), fp, &mut left_source),
        }),
        KeyRequest::Put {
            revision: revision(2),
            expected: id,
            contents: CanonicalRecord::from_validated(shape(&different), fp, &mut right_source),
        },
        &mut [],
        &mut || {
            calls += 1;
            if calls == 6 {
                Err(CanonicalError::Cancelled)
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(
        error,
        Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
    ));
    assert_eq!(calls, 6);
    assert!(matches!(
        classify_key(
            rel_key,
            KeyState::Live(CurrentEntity {
                provenance,
                contents: CanonicalRecord::from_validated(
                    shape(&prefix),
                    fp,
                    &mut std::io::empty()
                )
            }),
            KeyRequest::Put {
                revision: revision(2),
                expected: id,
                contents: CanonicalRecord::from_validated(
                    shape(&different),
                    fp,
                    &mut std::io::empty()
                )
            },
            &mut [],
            &mut || Ok(())
        ),
        Err(KeyLifecycleError::RelationshipIdentityChange)
    ));
}

#[test]
fn unkeyed_cypher_entities_keep_identity_and_checked_revision_without_receipts() {
    let id = node((1_u128 << 120) | 1);
    let origin = OperationFields {
        key: None,
        operation: GraphOperation::CypherEdit,
        requested_revision: revision(1),
        expected: ExpectedGraphState::Absent,
        incarnation: id,
        installed_revision: revision(1),
        delete_mode: None,
        original_generation: GraphGeneration::new(2),
    };
    for value in [1, u64::MAX] {
        let provenance = OperationProvenance::from_fields(
            Some(1),
            OperationFields {
                requested_revision: revision(value),
                installed_revision: revision(value),
                ..origin
            },
        )
        .expect("provenance");
        let (bytes, fp) = image(1);
        let mut source = Cursor::new(bytes);
        let result = classify_cypher(
            Some(CurrentEntity {
                provenance,
                contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut source),
            }),
            CypherEdit::Delete(GraphDeleteMode::Detach),
            &mut [],
            &mut || Ok(()),
        );
        if value == u64::MAX {
            assert!(matches!(result, Err(KeyLifecycleError::RevisionOverflow)));
        } else {
            let KeyDecision::Change(change) = result.expect("delete") else {
                panic!("must change")
            };
            let fields = change
                .install(id, GraphGeneration::new(3), &mut || Ok(()))
                .expect("logical finalization")
                .fields();
            assert_eq!(fields.key, None);
            assert_eq!(fields.installed_revision, revision(2));
            assert_eq!(fields.expected, ExpectedGraphState::Entity(id));
            assert_eq!(fields.delete_mode, Some(GraphDeleteMode::Detach));
        }
    }
}

#[test]
fn logical_finalization_checks_cancellation_while_streaming_long_key_provenance() {
    let name = "x".repeat(200_000);
    let key = ApplicationKey::new(EntityKind::Node, "", &name).expect("key");
    let (bytes, fp) = image(1);
    let mut source = Cursor::new(bytes);
    let KeyDecision::Change(change) = classify_key(
        key,
        KeyState::NeverUsed,
        KeyRequest::Create {
            revision: revision(1),
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut source),
        },
        &mut [],
        &mut || Ok(()),
    )
    .expect("new") else {
        panic!("change")
    };
    let mut calls = 0;
    assert!(matches!(
        change.install(node(1), GraphGeneration::new(1), &mut || {
            calls += 1;
            if calls == 4 {
                Err(CanonicalError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
    ));
    assert_eq!(calls, 4);
    assert_eq!(
        change
            .install(node(1), GraphGeneration::new(1), &mut || Ok(()))
            .expect("clean control")
            .fields()
            .key,
        Some(key)
    );
}

#[test]
fn controlled_provenance_matches_existing_admission_and_exact_framing() {
    let key = ApplicationKey::new(EntityKind::Node, "é\0", "chunk").expect("key");
    let fields = OperationFields {
        key: Some(key),
        operation: GraphOperation::StructuredPut,
        requested_revision: revision(3),
        installed_revision: revision(3),
        expected: ExpectedGraphState::Entity(node(9)),
        incarnation: node(9),
        delete_mode: None,
        original_generation: GraphGeneration::new(7),
    };
    for version in [None, Some(0), Some(1), Some(2)] {
        for altered in [
            fields,
            OperationFields {
                incarnation: EntityId::Relationship(RelId::new(9).expect("id")),
                ..fields
            },
            OperationFields {
                expected: ExpectedGraphState::Entity(EntityId::Relationship(
                    RelId::new(9).expect("id"),
                )),
                ..fields
            },
        ] {
            let original = OperationProvenance::from_fields(version, altered);
            let controlled =
                OperationProvenance::from_fields_with_control(version, altered, &mut || Ok(()));
            match (original, controlled) {
                (Ok(original), Ok(controlled)) => {
                    assert_eq!(original, controlled);
                    assert_eq!(original.encoded_len(), controlled.encoded_len());
                    let mut left = Vec::new();
                    let mut right = Vec::new();
                    original
                        .write_to(&mut left, &mut || Ok(()))
                        .expect("original");
                    controlled
                        .write_to(&mut right, &mut || Ok(()))
                        .expect("controlled");
                    assert_eq!(left, right);
                }
                (Err(original), Err(controlled)) => {
                    assert_eq!(original.to_string(), controlled.to_string())
                }
                other => panic!("constructor drift {other:?}"),
            }
        }
    }
    let name = "x".repeat(MAX_GRAPH_INPUT_BYTES);
    let too_large = OperationFields {
        key: Some(ApplicationKey::new(EntityKind::Node, "", &name).expect("raw name bound")),
        ..fields
    };
    let old =
        OperationProvenance::from_fields(Some(1), too_large).expect_err("complete framing cap");
    let new = OperationProvenance::from_fields_with_control(Some(1), too_large, &mut || Ok(()))
        .expect_err("same cap");
    assert_eq!(old.to_string(), new.to_string());
}
