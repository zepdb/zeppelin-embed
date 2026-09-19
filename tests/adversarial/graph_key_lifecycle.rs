//! PG5 runs pure logical lifecycle observations in each seeded fault episode.
//! Publication, allocator persistence, adjacency liveness and WAL are later owners.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::property_graph::*;
use zeppelin_embed_adversarial_oracle::graph_key_lifecycle::{
    Action, Expected, Observation, Origin, Record, Rejection, check,
};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.lifecycle.create",
    "property-graph.lifecycle.replay",
    "property-graph.lifecycle.conflict",
    "property-graph.lifecycle.stale",
    "property-graph.lifecycle.incarnation",
    "property-graph.lifecycle.delete",
    "property-graph.lifecycle.recreate",
    "property-graph.lifecycle.cypher",
    "property-graph.lifecycle.noop",
    "property-graph.lifecycle.overflow",
    "property-graph.lifecycle.duplicate",
    "property-graph.lifecycle.cancel",
];
fn key() -> ApplicationKey<'static> {
    ApplicationKey::new(EntityKind::Node, "source", "chunk").expect("valid key")
}
fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).expect("positive trace revision")
}
fn id(value: u128) -> EntityId {
    EntityId::Node(NodeId::new(value).expect("nonzero trace identity"))
}
fn mode(detach: bool) -> GraphDeleteMode {
    if detach {
        GraphDeleteMode::Detach
    } else {
        GraphDeleteMode::Restrict
    }
}
fn origin(value: Origin) -> GraphOperation {
    match value {
        Origin::Create => GraphOperation::StructuredCreate,
        Origin::Put => GraphOperation::StructuredPut,
        Origin::Delete => GraphOperation::StructuredDelete,
        Origin::Recreate => GraphOperation::StructuredRecreate,
        Origin::Cypher => GraphOperation::CypherEdit,
    }
}
fn provenance(record: Record) -> OperationProvenance<'static> {
    OperationProvenance::from_fields(
        Some(1),
        OperationFields {
            operation: origin(record.origin),
            key: Some(key()),
            requested_revision: revision(record.revision),
            installed_revision: revision(record.revision),
            expected: match record.expected {
                Expected::Absent => ExpectedGraphState::Absent,
                Expected::Entity(value) => ExpectedGraphState::Entity(id(value)),
                Expected::Deletion(value) => ExpectedGraphState::Deletion(revision(value)),
            },
            incarnation: id(record.id),
            delete_mode: record.detach.map(mode),
            original_generation: GraphGeneration::new(record.generation),
        },
    )
    .expect("valid oracle state")
}
fn image(bits: u64) -> Vec<u8> {
    let mut properties = [GraphProperty::new(
        GraphName::new("value").expect("name"),
        PropertyValue::new(PropertyData::F64(f64::from_bits(bits))).expect("all original bits"),
    )];
    let mut labels = [];
    let image =
        CanonicalContents::node(&mut labels, &mut properties, None, None).expect("canonical input");
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).expect("bytes");
    bytes
}
fn record(provenance: OperationProvenance<'_>, bits: Option<u64>) -> Result<Record, String> {
    let f = provenance.fields();
    if f.key != Some(key())
        || f.requested_revision != f.installed_revision
        || provenance.version() != 1
    {
        return Err("PG5 incomplete installing provenance".into());
    }
    Ok(Record {
        id: match f.incarnation {
            EntityId::Node(id) => id.get(),
            _ => return Err("PG5 changed identity kind".into()),
        },
        revision: f.installed_revision.get(),
        origin: match f.operation {
            GraphOperation::StructuredCreate => Origin::Create,
            GraphOperation::StructuredPut => Origin::Put,
            GraphOperation::StructuredDelete => Origin::Delete,
            GraphOperation::StructuredRecreate => Origin::Recreate,
            GraphOperation::CypherEdit => Origin::Cypher,
        },
        expected: match f.expected {
            ExpectedGraphState::Absent => Expected::Absent,
            ExpectedGraphState::Entity(EntityId::Node(id)) => Expected::Entity(id.get()),
            ExpectedGraphState::Deletion(value) => Expected::Deletion(value.get()),
            _ => return Err("PG5 changed expected identity kind".into()),
        },
        detach: f.delete_mode.map(|mode| mode == GraphDeleteMode::Detach),
        generation: f.original_generation.get(),
        bits,
    })
}
fn observe(
    state: Option<Record>,
    action: Action,
    fresh: u128,
    generation: u64,
) -> Result<Observation, String> {
    let bits = match action {
        Action::Create { bits, .. }
        | Action::Put { bits, .. }
        | Action::Recreate { bits, .. }
        | Action::CypherPut { bits } => Some(bits),
        _ => None,
    };
    let left = image(state.and_then(|old| old.bits).unwrap_or(0));
    let right = image(bits.unwrap_or(0));
    let fp = CanonicalFingerprint::new(left.len() as u64, 0).expect("forced equal hash");
    let mut left_source = left.as_slice();
    let mut right_source = right.as_slice();
    let current = state
        .filter(|old| old.bits.is_some())
        .map(|old| CurrentEntity {
            provenance: provenance(old),
            contents: CanonicalRecord::from_validated(EntityShape::Node, fp, &mut left_source),
        });
    let attempted = CanonicalRecord::from_validated(EntityShape::Node, fp, &mut right_source);
    let mut scratch = [0; 17];
    let decision = match action {
        Action::CypherPut { .. } => classify_cypher(
            current,
            CypherEdit::Put(attempted),
            &mut scratch,
            &mut || Ok(()),
        ),
        Action::CypherDelete { detach } => classify_cypher(
            current,
            CypherEdit::Delete(mode(detach)),
            &mut scratch,
            &mut || Ok(()),
        ),
        _ => {
            let state = if let Some(current) = current {
                KeyState::Live(current)
            } else if let Some(old) = state {
                KeyState::Deleted(provenance(old))
            } else {
                KeyState::NeverUsed
            };
            let request = match action {
                Action::Create { revision: rev, .. } => KeyRequest::Create {
                    revision: revision(rev),
                    contents: attempted,
                },
                Action::Put {
                    revision: rev,
                    expected,
                    ..
                } => KeyRequest::Put {
                    revision: revision(rev),
                    expected: id(expected),
                    contents: attempted,
                },
                Action::Delete {
                    revision: rev,
                    expected,
                    detach,
                } => KeyRequest::Delete {
                    revision: revision(rev),
                    expected: id(expected),
                    mode: mode(detach),
                },
                Action::Recreate {
                    revision: rev,
                    deleted,
                    ..
                } => KeyRequest::Recreate {
                    revision: revision(rev),
                    deleted_revision: revision(deleted),
                    contents: attempted,
                },
                _ => unreachable!(),
            };
            classify_key(key(), state, request, &mut scratch, &mut || Ok(()))
        }
    };
    Ok(match decision {
        Ok(KeyDecision::NoOp) => Observation::NoOp,
        Ok(KeyDecision::Replay(value)) => {
            Observation::Replay(record(value, state.and_then(|old| old.bits))?)
        }
        Ok(KeyDecision::Change(change)) => {
            let installed = change
                .install(
                    change.existing_incarnation().unwrap_or_else(|| id(fresh)),
                    GraphGeneration::new(generation),
                    &mut || Ok(()),
                )
                .map_err(|e| e.to_string())?;
            Observation::Changed(record(installed, bits)?)
        }
        Err(error) => Observation::Rejected(match error {
            KeyLifecycleError::MissingKey => Rejection::Missing,
            KeyLifecycleError::Stale { .. } => Rejection::Stale,
            KeyLifecycleError::RevisionConflict => Rejection::Conflict,
            KeyLifecycleError::AlreadyExists => Rejection::Exists,
            KeyLifecycleError::DeletedKey => Rejection::Deleted,
            KeyLifecycleError::NotDeleted => Rejection::NotDeleted,
            KeyLifecycleError::IncarnationConflict => Rejection::Incarnation,
            KeyLifecycleError::DeletionRevisionConflict => Rejection::DeletionRevision,
            KeyLifecycleError::RevisionOverflow => Rejection::Overflow,
            other => return Err(format!("PG5 unexpected engine error {other}")),
        }),
    })
}
fn step(
    state: &mut Option<Record>,
    action: Action,
    serial: &mut u128,
    generation: &mut u64,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    let fresh = (1_u128 << 100) | *serial;
    let observed = observe(*state, action, fresh, *generation)?;
    check(*state, action, fresh, *generation, observed)?;
    let category = match observed {
        Observation::Changed(record) => {
            *state = Some(record);
            *serial += 1;
            *generation += 1;
            match record.origin {
                Origin::Create => 0,
                Origin::Delete => 5,
                Origin::Recreate => 6,
                Origin::Cypher => 7,
                Origin::Put => 7,
            }
        }
        Observation::Replay(_) => 1,
        Observation::NoOp => 8,
        Observation::Rejected(Rejection::Stale) => 3,
        Observation::Rejected(Rejection::Incarnation) => 4,
        Observation::Rejected(Rejection::Overflow) => 9,
        Observation::Rejected(_) => 2,
    };
    coverage.hit(REQUIRED_COVERAGE[category]);
    Ok(())
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::key_lifecycle", seed);
    let bits = rng.next_u64();
    let first = (1_u128 << 100) | 1;
    let mut state = None;
    let mut serial = 1;
    let mut generation = 1;
    let fixed = [
        Action::Put {
            revision: 1,
            expected: first,
            bits,
        },
        Action::Create { revision: 7, bits },
        Action::Create { revision: 7, bits },
        Action::Create {
            revision: 7,
            bits: bits ^ 1,
        },
        Action::Create { revision: 6, bits },
        Action::Create { revision: 8, bits },
        Action::Recreate {
            revision: 8,
            deleted: 7,
            bits,
        },
        Action::Put {
            revision: u64::MAX,
            expected: 1,
            bits,
        },
        Action::Put {
            revision: 8,
            expected: first,
            bits: bits ^ 1,
        },
        Action::Put {
            revision: 8,
            expected: first,
            bits: bits ^ 1,
        },
        Action::Delete {
            revision: 9,
            expected: first,
            detach: true,
        },
        Action::Delete {
            revision: 9,
            expected: first,
            detach: true,
        },
        Action::Delete {
            revision: 9,
            expected: first,
            detach: false,
        },
        Action::Create { revision: 10, bits },
        Action::Put {
            revision: 10,
            expected: first,
            bits,
        },
        Action::Delete {
            revision: 10,
            expected: first,
            detach: true,
        },
        Action::Recreate {
            revision: 10,
            deleted: 8,
            bits,
        },
        Action::Recreate {
            revision: 10,
            deleted: 9,
            bits,
        },
        Action::Recreate {
            revision: 10,
            deleted: 9,
            bits,
        },
        Action::Delete {
            revision: u64::MAX,
            expected: first,
            detach: true,
        },
        Action::CypherPut { bits },
        Action::CypherPut { bits: bits ^ 1 },
        Action::Put {
            revision: 11,
            expected: (1_u128 << 100) | 4,
            bits: bits ^ 1,
        },
        Action::CypherDelete { detach: false },
        Action::CypherDelete { detach: true },
    ];
    for action in fixed {
        step(&mut state, action, &mut serial, &mut generation, coverage)?;
    }
    for _ in 0..96 {
        let old = state.expect("trace has created a key");
        let revision = match rng.next_u32() % 3 {
            0 => old.revision.saturating_sub(1).max(1),
            1 => old.revision,
            _ => old.revision + 1,
        };
        let expected = if rng.next_u32().is_multiple_of(4) {
            first
        } else {
            old.id
        };
        let bits = if rng.next_u32().is_multiple_of(2) {
            old.bits.unwrap_or(0)
        } else {
            rng.next_u64()
        };
        let action = match rng.next_u32() % 6 {
            0 => Action::Create { revision, bits },
            1 => Action::Put {
                revision,
                expected,
                bits,
            },
            2 => Action::Delete {
                revision,
                expected,
                detach: true,
            },
            3 => Action::Recreate {
                revision,
                deleted: old.revision,
                bits,
            },
            4 => Action::CypherPut { bits },
            _ => Action::CypherDelete { detach: false },
        };
        step(&mut state, action, &mut serial, &mut generation, coverage)?;
    }
    let max = Record {
        id: first,
        revision: u64::MAX,
        origin: Origin::Put,
        expected: Expected::Entity(first),
        detach: None,
        generation: 1,
        bits: Some(bits),
    };
    state = Some(max);
    for action in [
        Action::CypherPut { bits },
        Action::CypherPut { bits: bits ^ 1 },
        Action::CypherDelete { detach: true },
    ] {
        step(&mut state, action, &mut serial, &mut generation, coverage)?;
    }
    let target = BatchTarget::new(Some(key()), Some(id(first))).expect("target");
    if !matches!(
        validate_distinct_targets(&mut [target, target], &mut || Ok(())),
        Err(KeyLifecycleError::DuplicateTarget)
    ) {
        return Err("PG5 identical duplicate escaped".into());
    }
    coverage.hit(REQUIRED_COVERAGE[10]);
    if !matches!(
        validate_distinct_targets(&mut [target], &mut || Err(CanonicalError::Cancelled)),
        Err(KeyLifecycleError::Canonical(CanonicalError::Cancelled))
    ) {
        return Err("PG5 cancellation escaped".into());
    }
    coverage.hit(REQUIRED_COVERAGE[11]);
    Ok(())
}

#[test]
fn lifecycle_oracle_can_fire_on_result_and_retained_field_corruption() {
    let action = Action::Create {
        revision: 7,
        bits: 0x8000_0000_0000_0000,
    };
    let first = (1_u128 << 100) | 9;
    let observation = observe(None, action, first, 13).expect("control");
    check(None, action, first, 13, observation).expect("clean model");
    let Observation::Changed(original) = observation else {
        panic!("must create")
    };
    let mut corruptions = vec![
        Observation::NoOp,
        Observation::Replay(original),
        Observation::Rejected(Rejection::Conflict),
    ];
    for field in 0..7 {
        let mut corrupt = original;
        match field {
            0 => corrupt.id = 9,
            1 => corrupt.revision += 1,
            2 => corrupt.origin = Origin::Put,
            3 => corrupt.expected = Expected::Entity(first),
            4 => corrupt.detach = Some(true),
            5 => corrupt.generation += 1,
            _ => corrupt.bits = Some(0),
        }
        corruptions.push(Observation::Changed(corrupt));
    }
    for corrupted in corruptions {
        assert!(
            check(None, action, first, 13, corrupted)
                .expect_err("CAN FIRE")
                .contains("PG5 key lifecycle")
        );
    }
}
