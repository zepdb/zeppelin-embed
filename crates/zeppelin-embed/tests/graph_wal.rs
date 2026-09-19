#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use zeppelin_embed::property_graph::storage::artifact::{ArtifactId, BlockKind, PhysicalRef};
use zeppelin_embed::property_graph::wal::*;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

fn reference() -> RequiredRef {
    let artifact = ArtifactId::new((1u128 << 96) + 9).unwrap();
    RequiredRef {
        object: ArtifactDescriptor {
            store: StoreInstanceId::new((1u128 << 100) + 3).unwrap(),
            artifact,
            generation: GraphGeneration::new(0),
            serial: 1,
            bytes: 200,
            family: 17,
            version: 1,
            checksum: 7,
        },
        block: PhysicalRef {
            artifact,
            offset: 96,
            length: 64,
            kind: BlockKind::CommitParticipant,
            version: 1,
        },
    }
}
fn base() -> CommitState<'static> {
    CommitState {
        store: reference().object.store,
        generation: GraphGeneration::new(0),
        sequence: 0,
        graph: WalGraphRoots::default(),
        catalog: reference(),
        vector: None,
        text: None,
        reclaim: None,
        high_waters: HighWaters {
            creation_serial: 1,
            ..HighWaters::default()
        },
        prepared_inventories: ReferenceList::Values(&[]),
    }
}
#[test]
fn complete_graph_envelope_encodes_full_width_identity_and_state() {
    let base = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        ..base
    };
    let envelope = Envelope {
        batch: BatchId::new((1u128 << 101) + 5).unwrap(),
        kind: EnvelopeKind::Maintenance,
        changes: &[],
        state: target,
    };
    let mut bytes = [0; 4096];
    let mut cancel = || false;
    let mut resources = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let header = encode_header(base.store, 1, &mut bytes).expect("required graph WAL header");
    assert_eq!(header, 64);
    let length = encode_envelope(base, envelope, &mut bytes[header..], &mut resources)
        .expect("complete graph envelope");
    assert!(length > 128);
    assert_eq!(&bytes[32..48], &base.store.get().to_le_bytes());
    assert_eq!(&bytes[88..104], &envelope.batch.get().to_le_bytes());
}

struct FixtureValidator {
    calls: usize,
    reject: bool,
}
impl ReplayValidator for FixtureValidator {
    fn required(
        &mut self,
        _: RequiredRef,
        _: RequiredRole,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.calls += 1;
        if self.reject {
            Err(WalError::MissingArtifact)
        } else {
            Ok(())
        }
    }
    fn mutation(&mut self, _: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
        Err(WalError::Participant)
    }
    fn inventory(&mut self, _: InventoryChange, _: &mut WalResources<'_>) -> Result<(), WalError> {
        Err(WalError::Participant)
    }
    fn reclaim_intent(
        &mut self,
        _: ReclaimIntent<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Err(WalError::Participant)
    }
    fn reclaim_complete(
        &mut self,
        _: ReclaimComplete<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Err(WalError::Participant)
    }
    fn state(
        &mut self,
        _: CommitState<'_>,
        _: CommitState<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Ok(())
    }
}
fn empty_log() -> Vec<u8> {
    let base = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        ..base
    };
    let mut bytes = vec![0; 4096];
    let mut cancelled = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancelled).unwrap();
    encode_header(base.store, 1, &mut bytes).unwrap();
    let n = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new((1u128 << 101) + 5).unwrap(),
            kind: EnvelopeKind::Maintenance,
            changes: &[],
            state: target,
        },
        &mut bytes[64..],
        &mut r,
    )
    .unwrap();
    bytes.truncate(64 + n);
    bytes
}
#[test]
fn every_prefix_exposes_only_complete_envelopes_and_requires_validation() {
    let bytes = empty_log();
    for end in 64..=bytes.len() {
        let mut cancelled = || false;
        let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancelled).unwrap();
        let mut replay = Replay::new(&bytes[..end], base(), &mut r).unwrap();
        let mut validator = FixtureValidator {
            calls: 0,
            reject: false,
        };
        match replay
            .next_envelope(&mut validator, &mut r)
            .expect("complete or proved prefix")
        {
            ReplayStep::End(tail) => {
                assert!(end < bytes.len());
                assert_eq!(tail.complete_bytes, 64);
                assert_eq!(tail.incomplete_tail, end > 64);
                assert_eq!(validator.calls, 0);
            }
            ReplayStep::Envelope(batch) => {
                assert_eq!(end, bytes.len());
                assert_eq!(batch.batch.get(), (1u128 << 101) + 5);
                assert_eq!(batch.state.generation.get(), 1);
                assert_eq!(validator.calls, 1);
            }
        }
    }
    let mut cancelled = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancelled).unwrap();
    let mut replay = Replay::new(&bytes, base(), &mut r).unwrap();
    let mut validator = FixtureValidator {
        calls: 0,
        reject: true,
    };
    assert_eq!(
        replay.next_envelope(&mut validator, &mut r).unwrap_err(),
        WalError::MissingArtifact
    );
    assert_eq!(
        replay.next_envelope(&mut validator, &mut r).unwrap_err(),
        WalError::Failed
    );
}

#[test]
fn mutation_preserves_complete_provenance_membership_and_atomic_framing() {
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphOperation, GraphRevision,
        NodeId, OperationFields,
    };
    let base = base();
    let id = EntityId::Node(NodeId::new((1u128 << 80) + 19).unwrap());
    let fields = OperationFields {
        operation: GraphOperation::StructuredCreate,
        key: Some(ApplicationKey::new(EntityKind::Node, "n\0é", "key").unwrap()),
        requested_revision: GraphRevision::new(31).unwrap(),
        installed_revision: GraphRevision::new(31).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: id,
        delete_mode: None,
        original_generation: GraphGeneration::new(1),
    };
    let canonical = RequiredRef {
        block: PhysicalRef {
            kind: BlockKind::CanonicalImage,
            ..reference().block
        },
        ..reference()
    };
    let changes = [Change::Mutation(Mutation {
        provenance_version: 1,
        provenance: fields,
        live: true,
        canonical: Some(canonical),
        membership: Membership {
            text_after: true,
            vector_after: true,
            ..Membership::default()
        },
    })];
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        high_waters: HighWaters {
            node: (1u128 << 80) + 19,
            ..base.high_waters
        },
        ..base
    };
    let mut bytes = vec![0; 4096];
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    encode_header(base.store, 1, &mut bytes).unwrap();
    let n = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new(1).unwrap(),
            kind: EnvelopeKind::Mutation,
            changes: &changes,
            state: target,
        },
        &mut bytes[64..],
        &mut r,
    )
    .expect("normalized mutation framing");
    bytes.truncate(n + 64);
    struct Observe<'a> {
        expected: OperationFields<'a>,
        seen: usize,
    }
    impl ReplayValidator for Observe<'_> {
        fn required(
            &mut self,
            _: RequiredRef,
            _: RequiredRole,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Ok(())
        }
        fn mutation(&mut self, v: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
            assert_eq!(v.provenance, self.expected);
            assert!(v.membership.text_after && v.membership.vector_after);
            assert!(v.live);
            self.seen += 1;
            Ok(())
        }
        fn inventory(
            &mut self,
            _: InventoryChange,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn reclaim_intent(
            &mut self,
            _: ReclaimIntent<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn reclaim_complete(
            &mut self,
            _: ReclaimComplete<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn state(
            &mut self,
            _: CommitState<'_>,
            _: CommitState<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Ok(())
        }
    }
    let mut replay = Replay::new(&bytes, base, &mut r).unwrap();
    let mut observe = Observe {
        expected: fields,
        seen: 0,
    };
    let ReplayStep::Envelope(batch) = replay.next_envelope(&mut observe, &mut r).unwrap() else {
        panic!("whole batch")
    };
    assert_eq!(batch.state.high_waters.node, (1u128 << 80) + 19);
    assert_eq!(observe.seen, 1);
    let mut changes = batch.changes();
    let Some(Change::Mutation(decoded)) = changes
        .next_change(&mut r)
        .expect("borrowed complete mutation")
    else {
        panic!("mutation")
    };
    assert_eq!(decoded.provenance, fields);
    assert!(changes.next_change(&mut r).unwrap().is_none());
}

#[test]
fn maintenance_keeps_required_proofs_separate_from_missing_deletion_targets() {
    let base = base();
    let id = BatchId::new((1u128 << 89) + 3).unwrap();
    let candidate = ArtifactDescriptor {
        artifact: ArtifactId::new((1u128 << 90) + 8).unwrap(),
        ..reference().object
    };
    let candidates = [candidate];
    let changes = [
        Change::Inventory(InventoryChange {
            object: candidate,
            state: InventoryState::ReclaimPending(id),
        }),
        Change::ReclaimIntent(ReclaimIntent {
            id,
            capture_generation: base.generation,
            capture_sequence: base.sequence,
            serial_fence: 1,
            protected_roots: reference(),
            protected_digest: 19,
            completed_mark: reference(),
            mark_digest: 23,
            candidates: DescriptorList::Values(&candidates),
        }),
    ];
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        reclaim: Some(reference()),
        ..base
    };
    let mut bytes = vec![0; 4096];
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    encode_header(base.store, 1, &mut bytes).unwrap();
    let n = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new(5).unwrap(),
            kind: EnvelopeKind::Maintenance,
            changes: &changes,
            state: target,
        },
        &mut bytes[64..],
        &mut r,
    )
    .expect("explicit durable maintenance");
    bytes.truncate(64 + n);
    struct Reclaim {
        candidate: ArtifactDescriptor,
        calls: Vec<RequiredRole>,
        intent: usize,
        reject_mark: bool,
    }
    impl ReplayValidator for Reclaim {
        fn required(
            &mut self,
            v: RequiredRef,
            role: RequiredRole,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            assert_ne!(
                v.object.artifact, self.candidate.artifact,
                "deletion targets may be absent after interrupted unlink"
            );
            self.calls.push(role);
            if self.reject_mark && role == RequiredRole::Participant(ParticipantRole::CompletedMark)
            {
                Err(WalError::MissingArtifact)
            } else {
                Ok(())
            }
        }
        fn mutation(&mut self, _: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn inventory(
            &mut self,
            v: InventoryChange,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            assert_eq!(v.object, self.candidate);
            Ok(())
        }
        fn reclaim_intent(
            &mut self,
            v: ReclaimIntent<'_>,
            r: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            assert_eq!(v.candidates.get(0, r)?, self.candidate);
            assert_eq!(v.protected_digest, 19);
            assert_eq!(v.mark_digest, 23);
            self.intent += 1;
            Ok(())
        }
        fn reclaim_complete(
            &mut self,
            _: ReclaimComplete<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn state(
            &mut self,
            _: CommitState<'_>,
            _: CommitState<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Ok(())
        }
    }
    for reject_mark in [false, true] {
        let mut replay = Replay::new(&bytes, base, &mut r).unwrap();
        let mut validator = Reclaim {
            candidate,
            calls: vec![],
            intent: 0,
            reject_mark,
        };
        let result = replay.next_envelope(&mut validator, &mut r);
        if reject_mark {
            assert_eq!(result.unwrap_err(), WalError::MissingArtifact);
            assert_eq!(validator.intent, 0);
        } else {
            assert!(matches!(result, Ok(ReplayStep::Envelope(_))));
            assert_eq!(validator.intent, 1);
            assert!(
                validator
                    .calls
                    .contains(&RequiredRole::Participant(ParticipantRole::ProtectedRoots))
            );
        }
    }
}

fn mutation_log() -> Vec<u8> {
    use zeppelin_embed::property_graph::*;
    let base = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        high_waters: HighWaters {
            node: 19,
            ..base.high_waters
        },
        ..base
    };
    let p = OperationFields {
        operation: GraphOperation::StructuredCreate,
        key: Some(ApplicationKey::new(EntityKind::Node, "ns", "key").unwrap()),
        requested_revision: GraphRevision::new(1).unwrap(),
        installed_revision: GraphRevision::new(1).unwrap(),
        expected: ExpectedGraphState::Absent,
        incarnation: EntityId::Node(NodeId::new(19).unwrap()),
        delete_mode: None,
        original_generation: target.generation,
    };
    let canonical = RequiredRef {
        block: PhysicalRef {
            kind: BlockKind::CanonicalImage,
            ..reference().block
        },
        ..reference()
    };
    let changes = [Change::Mutation(Mutation {
        provenance_version: 1,
        provenance: p,
        live: true,
        canonical: Some(canonical),
        membership: Membership::default(),
    })];
    let mut bytes = vec![0; 4096];
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    encode_header(base.store, 1, &mut bytes).unwrap();
    let n = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new(9).unwrap(),
            kind: EnvelopeKind::Mutation,
            changes: &changes,
            state: target,
        },
        &mut bytes[64..],
        &mut r,
    )
    .unwrap();
    bytes.truncate(64 + n);
    bytes
}
fn frames(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut at = 64;
    let mut result = Vec::new();
    while at < bytes.len() {
        let n = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize + 72;
        result.push((at, n));
        at += n;
    }
    result
}
fn repair_record(bytes: &mut [u8], at: usize, n: usize) {
    let h = xxhash_rust::xxh3::xxh3_64(&bytes[at..at + 56]);
    bytes[at + 56..at + 64].copy_from_slice(&h.to_le_bytes());
    let h = xxhash_rust::xxh3::xxh3_64(&bytes[at..at + n - 8]);
    bytes[at + n - 8..at + n].copy_from_slice(&h.to_le_bytes());
}
#[test]
fn complete_malformed_change_is_never_hidden_by_incomplete_commit() {
    let mut bytes = mutation_log();
    let rows = frames(&bytes);
    let (at, n) = rows[1];
    bytes[at + 64 + 20..at + 64 + 22].copy_from_slice(&2u16.to_le_bytes()); // Unsupported ZGOP version.
    repair_record(&mut bytes, at, n);
    bytes.truncate(rows[2].0);
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let mut replay = Replay::new(&bytes, base(), &mut r).unwrap();
    let mut validator = FixtureValidator {
        calls: 0,
        reject: false,
    };
    assert_eq!(
        replay.next_envelope(&mut validator, &mut r).unwrap_err(),
        WalError::Unsupported
    );
    assert_eq!(validator.calls, 0);
}

#[test]
fn required_object_validation_binds_full_descriptor_role_and_checksum() {
    use zeppelin_embed::property_graph::storage::artifact::{
        self, ArtifactIdentity, Block, ContainerKind,
    };
    let payload = b"ZGCP\x01\x00\x01\x00catalog-fixture";
    let identity = ArtifactIdentity {
        store: base().store,
        artifact: reference().object.artifact,
        generation: GraphGeneration::new(0),
        creation_serial: 1,
    };
    let mut bytes = vec![0; 4096];
    let n = artifact::encode_into(
        ContainerKind::Object,
        identity,
        &[Block {
            kind: BlockKind::CommitParticipant,
            payload,
        }],
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    let frame = artifact::decode(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        &bytes,
    )
    .unwrap();
    let required = RequiredRef {
        object: ArtifactDescriptor {
            bytes: n as u32,
            checksum: u64::from_le_bytes(bytes[n - 8..].try_into().unwrap()),
            ..reference().object
        },
        block: frame.reference(0).unwrap(),
    };
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let role = RequiredRole::Participant(ParticipantRole::Catalog);
    assert_eq!(
        validate_required_block(required, role, &frame, &mut r).expect("actual complete object"),
        &payload[8..]
    );
    let wrong = RequiredRef {
        object: ArtifactDescriptor {
            checksum: required.object.checksum ^ 1,
            ..required.object
        },
        ..required
    };
    assert_eq!(
        validate_required_block(wrong, role, &frame, &mut r).unwrap_err(),
        WalError::Checksum
    );
    assert_eq!(
        validate_required_block(
            required,
            RequiredRole::Participant(ParticipantRole::CompletedMark),
            &frame,
            &mut r
        )
        .unwrap_err(),
        WalError::Participant
    );
    assert!(
        artifact::decode(
            ContainerKind::Object,
            Some((identity.store, identity.artifact)),
            &bytes[..n - 1]
        )
        .is_err()
    );
}

#[test]
fn partial_headers_reject_impossible_kind_length_and_batch_prefixes() {
    let original = mutation_log();
    let rows = frames(&original);
    let at = rows[1].0;
    for (offset, value) in [(4, 255u8), (11, 127u8)] {
        let mut bytes = original.clone();
        bytes[at + offset] = value;
        bytes.truncate(at + offset + 1);
        let mut cancel = || false;
        let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
        let mut replay = Replay::new(&bytes, base(), &mut r).unwrap();
        assert!(
            replay
                .next_envelope(
                    &mut FixtureValidator {
                        calls: 0,
                        reject: false
                    },
                    &mut r
                )
                .is_err(),
            "invalid available header byte {offset}"
        );
    }
}

#[test]
fn checked_checkpoint_watermark_skips_only_complete_retired_history() {
    let mut bytes = empty_log();
    let watermark = bytes.len();
    let checkpoint = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        ..base()
    };
    let fresh = RequiredRef {
        object: ArtifactDescriptor {
            artifact: ArtifactId::new(99).unwrap(),
            ..reference().object
        },
        block: PhysicalRef {
            artifact: ArtifactId::new(99).unwrap(),
            ..reference().block
        },
    };
    let target = CommitState {
        generation: GraphGeneration::new(2),
        sequence: 2,
        catalog: fresh,
        ..checkpoint
    };
    bytes.resize(watermark + 4096, 0);
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let n = encode_envelope(
        checkpoint,
        Envelope {
            batch: BatchId::new(2).unwrap(),
            kind: EnvelopeKind::Maintenance,
            changes: &[],
            state: target,
        },
        &mut bytes[watermark..],
        &mut r,
    )
    .unwrap();
    bytes.truncate(watermark + n);
    let mut replay = Replay::at_watermark(&bytes, checkpoint, watermark, &mut r)
        .expect("durable checkpoint with old unretired WAL");
    struct LiveOnly {
        live: ArtifactId,
        calls: usize,
    }
    impl ReplayValidator for LiveOnly {
        fn required(
            &mut self,
            v: RequiredRef,
            _: RequiredRole,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            if v.object.artifact != self.live {
                return Err(WalError::MissingArtifact);
            }
            self.calls += 1;
            Ok(())
        }
        fn mutation(&mut self, _: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn inventory(
            &mut self,
            _: InventoryChange,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn reclaim_intent(
            &mut self,
            _: ReclaimIntent<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn reclaim_complete(
            &mut self,
            _: ReclaimComplete<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn state(
            &mut self,
            _: CommitState<'_>,
            _: CommitState<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Ok(())
        }
    }
    let mut validator = LiveOnly {
        live: fresh.object.artifact,
        calls: 0,
    };
    assert!(matches!(
        replay.next_envelope(&mut validator, &mut r),
        Ok(ReplayStep::Envelope(_))
    ));
    assert_eq!(validator.calls, 1);
    for offset in [0, 64, watermark - 1, watermark + 1, bytes.len() + 1] {
        assert!(
            Replay::at_watermark(&bytes, checkpoint, offset, &mut r).is_err(),
            "wrong watermark {offset}"
        );
    }
    let wrong = CommitState {
        high_waters: HighWaters {
            node: 1,
            ..checkpoint.high_waters
        },
        ..checkpoint
    };
    assert!(Replay::at_watermark(&bytes, wrong, watermark, &mut r).is_err());
    assert!(Replay::at_watermark(&bytes[..watermark - 1], checkpoint, watermark, &mut r).is_err());
    let mut cut = vec![0; 64 + n];
    encode_header(checkpoint.store, 2, &mut cut).unwrap();
    cut[64..].copy_from_slice(&bytes[watermark..]);
    assert!(Replay::at_watermark(&cut, checkpoint, 64, &mut r).is_ok());
}

#[test]
fn retired_history_still_checks_mutation_ids_against_committed_high_waters() {
    let mut bytes = mutation_log();
    let rows = frames(&bytes);
    let (at, n) = rows[2];
    bytes[at + 112..at + 128].fill(0); // Commit state's NodeId high-water, with a live node19.
    repair_record(&mut bytes, at, n);
    let checkpoint = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        ..base()
    };
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    assert!(matches!(
        Replay::at_watermark(&bytes, checkpoint, bytes.len(), &mut r),
        Err(WalError::HighWater)
    ));
}

#[test]
fn cypher_deleted_rows_require_a_preexisting_entity_expectation() {
    use zeppelin_embed::property_graph::{
        EntityId, ExpectedGraphState, GraphDeleteMode, GraphOperation, GraphRevision, NodeId,
        OperationFields,
    };
    let initial = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        high_waters: HighWaters {
            node: 19,
            ..initial.high_waters
        },
        ..initial
    };
    let deletion = Mutation {
        provenance_version: 1,
        provenance: OperationFields {
            operation: GraphOperation::CypherEdit,
            key: None,
            requested_revision: GraphRevision::new(1).unwrap(),
            installed_revision: GraphRevision::new(1).unwrap(),
            expected: ExpectedGraphState::Absent,
            incarnation: EntityId::Node(NodeId::new(19).unwrap()),
            delete_mode: Some(GraphDeleteMode::Detach),
            original_generation: GraphGeneration::new(1),
        },
        live: false,
        canonical: None,
        membership: Membership::default(),
    };
    let changes = [Change::Mutation(deletion)];
    let mut bytes = [0; 4096];
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    assert_eq!(
        encode_envelope(
            initial,
            Envelope {
                batch: BatchId::new(5).unwrap(),
                kind: EnvelopeKind::Mutation,
                changes: &changes,
                state: target
            },
            &mut bytes,
            &mut r
        ),
        Err(WalError::Participant)
    );
    // A normalized net-empty statement still consumes and fences its IDs.
    assert!(
        encode_envelope(
            initial,
            Envelope {
                batch: BatchId::new(5).unwrap(),
                kind: EnvelopeKind::Mutation,
                changes: &[],
                state: target
            },
            &mut bytes,
            &mut r
        )
        .is_ok()
    );
}

// This fixture validates carriage/call ordering only. It does not prove real
// catalog, reachability, inventory, or unlink semantics owned by other tickets.
#[derive(Default)]
struct CarriageValidator<'a> {
    visits: [usize; 6],
    tree_roles: Vec<zeppelin_embed::property_graph::storage::tree::TreeKind>,
    cancel_on_state: Option<&'a std::cell::Cell<bool>>,
}
impl ReplayValidator for CarriageValidator<'_> {
    fn required(
        &mut self,
        _: RequiredRef,
        role: RequiredRole,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.visits[0] += 1;
        if let RequiredRole::Tree(tree) = role {
            self.tree_roles.push(tree);
        }
        Ok(())
    }
    fn mutation(&mut self, _: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
        self.visits[1] += 1;
        Ok(())
    }
    fn inventory(&mut self, _: InventoryChange, _: &mut WalResources<'_>) -> Result<(), WalError> {
        self.visits[2] += 1;
        Ok(())
    }
    fn reclaim_intent(
        &mut self,
        _: ReclaimIntent<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.visits[3] += 1;
        Ok(())
    }
    fn reclaim_complete(
        &mut self,
        _: ReclaimComplete<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.visits[4] += 1;
        Ok(())
    }
    fn state(
        &mut self,
        _: CommitState<'_>,
        _: CommitState<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.visits[5] += 1;
        if let Some(flag) = self.cancel_on_state {
            flag.set(true);
        }
        Ok(())
    }
}

#[test]
fn cancellation_triggered_by_final_validation_cannot_publish_a_batch() {
    let bytes = empty_log();
    let flag = std::cell::Cell::new(false);
    let mut cancel = || flag.get();
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let mut replay = Replay::new(&bytes, base(), &mut r).unwrap();
    let mut validator = CarriageValidator {
        cancel_on_state: Some(&flag),
        ..CarriageValidator::default()
    };
    assert_eq!(
        replay.next_envelope(&mut validator, &mut r).unwrap_err(),
        WalError::Cancelled
    );
    assert_eq!(validator.visits[5], 1);
    assert_eq!(
        replay.next_envelope(&mut validator, &mut r).unwrap_err(),
        WalError::Failed
    );
}

const GOLDEN: &[u8] = include_bytes!("fixtures/graph-wal/complete-v1.bin");

fn replay_count(bytes: &[u8]) -> Result<usize, WalError> {
    let mut cancel = || false;
    let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel)?;
    let mut replay = Replay::new(bytes, base(), &mut r)?;
    let mut fixture = CarriageValidator::default();
    let mut count = 0;
    loop {
        match replay.next_envelope(&mut fixture, &mut r)? {
            ReplayStep::Envelope(_) => count += 1,
            ReplayStep::End(_) => return Ok(count),
        }
    }
}

#[test]
fn independently_minted_golden_preserves_every_frame_and_state_position() {
    let hex: String = GOLDEN.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex,
        include_str!("fixtures/graph-wal/complete-v1.hex").trim()
    );
    let mut cancel = || false;
    let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let mut replay = Replay::new(GOLDEN, base(), &mut r).unwrap();
    let mut fixture = CarriageValidator::default();
    let mut previous = base();
    let mut rebuilt = vec![0; GOLDEN.len()];
    let mut offset = encode_header(previous.store, 1, &mut rebuilt).unwrap();
    for sequence in 1..=3 {
        let ReplayStep::Envelope(envelope) = replay.next_envelope(&mut fixture, &mut r).unwrap()
        else {
            panic!("golden envelope missing")
        };
        assert_eq!(envelope.state.sequence, sequence);
        assert_eq!(envelope.state.high_waters.node, (1u128 << 80) + 19);
        assert_eq!(envelope.state.high_waters.relationship, (1u128 << 79) + 7);
        assert_eq!(envelope.state.high_waters.symbols, [3, 5, 7, 11]);
        for (slot, root) in envelope.state.graph.slots.iter().enumerate() {
            assert_eq!(
                root.unwrap().object.artifact.get(),
                reference().object.artifact.get() + slot as u128 + 1
            );
        }
        assert!(
            envelope.state.vector.is_some()
                && envelope.state.text.is_some()
                && envelope.state.reclaim.is_some()
        );
        assert_eq!(envelope.state.prepared_inventories.len().unwrap(), 2);
        let mut reader = envelope.changes();
        let mut changes = Vec::new();
        while let Some(change) = reader.next_change(&mut r).unwrap() {
            changes.push(change);
        }
        assert_eq!(changes.len(), if sequence == 1 { 1 } else { 2 });
        if let Change::Mutation(m) = changes[0] {
            assert_eq!(m.provenance.key.unwrap().namespace().as_str(), "n\0é");
            assert_eq!(m.provenance.key.unwrap().key().as_str(), "recreated");
            assert_eq!(m.provenance.installed_revision.get(), 31);
            assert_eq!(
                m.provenance.expected,
                zeppelin_embed::property_graph::ExpectedGraphState::Deletion(
                    zeppelin_embed::property_graph::GraphRevision::new(29).unwrap()
                )
            );
        }
        offset += encode_envelope(
            previous,
            Envelope {
                batch: envelope.batch,
                kind: envelope.kind,
                changes: &changes,
                state: envelope.state,
            },
            &mut rebuilt[offset..],
            &mut r,
        )
        .unwrap();
        previous = envelope.state;
    }
    assert!(matches!(
        replay.next_envelope(&mut fixture, &mut r),
        Ok(ReplayStep::End(ReplayEnd {
            incomplete_tail: false,
            ..
        }))
    ));
    assert_eq!(offset, GOLDEN.len());
    assert_eq!(rebuilt, GOLDEN);
    assert_eq!(fixture.visits, [46, 1, 2, 1, 1, 3]);
    for (i, role) in fixture.tree_roles.iter().enumerate() {
        assert_eq!(*role as usize, i % 8 + 1);
    }
    assert_eq!(fixture.tree_roles.len(), 24);
}

#[test]
fn every_mutation_and_maintenance_prefix_obeys_complete_commit_boundaries() {
    let rows = frames(GOLDEN);
    let ends: Vec<_> = rows
        .iter()
        .filter(|(at, _)| GOLDEN[*at + 4] == 6)
        .map(|(at, n)| at + n)
        .collect();
    for end in 64..=GOLDEN.len() {
        let expected = ends.iter().filter(|boundary| **boundary <= end).count();
        assert_eq!(replay_count(&GOLDEN[..end]), Ok(expected), "prefix {end}");
    }
}

fn repair_envelope(bytes: &mut [u8]) {
    let rows = frames(bytes);
    let (commit, n) = *rows.last().unwrap();
    let digest = xxhash_rust::xxh3::xxh3_64(&bytes[64..commit]);
    bytes[commit + 72..commit + 80].copy_from_slice(&digest.to_le_bytes());
    repair_record(bytes, commit, n);
}

#[test]
fn repaired_checksums_do_not_hide_frame_splicing_counts_or_state_regression() {
    let original = mutation_log();
    let rows = frames(&original);
    for (label, row, within, value) in [
        ("index", 1, 12, 9),
        ("sequence", 1, 16, 9),
        ("batch", 1, 24, 1),
        ("provenance version", 1, 84, 2),
        ("commit count", 2, 64, 2),
        ("node high-water", 2, 112, 19),
    ] {
        let mut bytes = original.clone();
        let (at, n) = rows[row];
        bytes[at + within] ^= value;
        repair_record(&mut bytes, at, n);
        repair_envelope(&mut bytes);
        assert!(replay_count(&bytes).is_err(), "{label}");
    }
    let mut bytes = original.clone();
    let (commit, n) = rows[2];
    bytes[commit + 72] ^= 1;
    repair_record(&mut bytes, commit, n);
    assert_eq!(replay_count(&bytes), Err(WalError::Checksum));
    let mut bytes = original.clone();
    bytes.extend_from_slice(&original[64..]);
    assert!(replay_count(&bytes).is_err(), "repeated sequence");
    assert_eq!(replay_count(&original), Ok(1));
    let (change, length) = rows[1];
    let mut missing = original.clone();
    missing.drain(change..change + length);
    assert!(replay_count(&missing).is_err(), "missing complete change");
    let rows = frames(GOLDEN);
    let first = rows
        .iter()
        .position(|(at, _)| GOLDEN[*at + 4] == 3)
        .unwrap();
    let (a, an) = rows[first];
    let (b, bn) = rows[first + 1];
    let mut reordered = GOLDEN[..a].to_vec();
    reordered.extend_from_slice(&GOLDEN[b..b + bn]);
    reordered.extend_from_slice(&GOLDEN[a..a + an]);
    reordered.extend_from_slice(&GOLDEN[b + bn..]);
    assert!(
        replay_count(&reordered).is_err(),
        "reordered complete changes"
    );

    let mut bytes = original.clone();
    bytes.push(0);
    assert!(
        replay_count(&bytes).is_err(),
        "invalid terminal header prefix"
    );
}

#[test]
fn capacities_high_waters_and_sequence_never_wrap_or_partially_write() {
    let initial = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        ..initial
    };
    let envelope = Envelope {
        batch: BatchId::new(1).unwrap(),
        kind: EnvelopeKind::Mutation,
        changes: &[],
        state: target,
    };
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let mut tiny = [0xa5; 100];
    assert_eq!(
        encode_envelope(initial, envelope, &mut tiny, &mut r),
        Err(WalError::Capacity)
    );
    assert_eq!(tiny, [0xa5; 100]);
    for field in 0..7 {
        let mut high = initial.high_waters;
        match field {
            0 => high.node = 1,
            1 => high.relationship = 1,
            2..=5 => high.symbols[field - 2] = 1,
            _ => high.creation_serial = 2,
        }
        assert_eq!(
            encode_envelope(
                CommitState {
                    high_waters: high,
                    ..initial
                },
                envelope,
                &mut tiny,
                &mut r
            ),
            Err(WalError::HighWater)
        );
    }
    assert_eq!(
        encode_envelope(
            CommitState {
                sequence: u64::MAX,
                ..initial
            },
            envelope,
            &mut tiny,
            &mut r
        ),
        Err(WalError::Sequence)
    );
    assert_eq!(
        encode_envelope(
            CommitState {
                generation: GraphGeneration::new(u64::MAX),
                ..initial
            },
            envelope,
            &mut tiny,
            &mut r
        ),
        Err(WalError::Sequence)
    );
    assert!(matches!(
        WalResources::new(1, STACK_RESERVATION_BYTES - 1, &mut cancel),
        Err(WalError::Capacity)
    ));
    let mut r = WalResources::new(1, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    assert_eq!(
        encode_envelope(initial, envelope, &mut tiny, &mut r),
        Err(WalError::WorkLimit)
    );
    assert_eq!(tiny, [0xa5; 100]);
}

#[test]
fn long_split_utf8_provenance_is_cancellable_during_real_codec_work() {
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphOperation, GraphRevision,
        NodeId, OperationFields,
    };
    for split in 1..=3 {
        let name = format!("{}🧭{}", "x".repeat(65_536 - split), "z".repeat(131_072));
        let initial = base();
        let target = CommitState {
            generation: GraphGeneration::new(1),
            sequence: 1,
            high_waters: HighWaters {
                node: 19,
                ..initial.high_waters
            },
            ..initial
        };
        let changes = [Change::Mutation(Mutation {
            provenance_version: 1,
            provenance: OperationFields {
                operation: GraphOperation::StructuredCreate,
                key: Some(ApplicationKey::new(EntityKind::Node, &name, "k").unwrap()),
                requested_revision: GraphRevision::new(1).unwrap(),
                installed_revision: GraphRevision::new(1).unwrap(),
                expected: ExpectedGraphState::Absent,
                incarnation: EntityId::Node(NodeId::new(19).unwrap()),
                delete_mode: None,
                original_generation: target.generation,
            },
            live: true,
            canonical: Some(RequiredRef {
                block: PhysicalRef {
                    kind: BlockKind::CanonicalImage,
                    ..reference().block
                },
                ..reference()
            }),
            membership: Membership::default(),
        })];
        let envelope = Envelope {
            batch: BatchId::new(99).unwrap(),
            kind: EnvelopeKind::Mutation,
            changes: &changes,
            state: target,
        };
        let mut bytes = vec![0; name.len() + 4096];
        let mut polls = 0;
        let mut cancel = || {
            polls += 1;
            false
        };
        let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
        encode_header(initial.store, 1, &mut bytes).unwrap();
        let n = encode_envelope(initial, envelope, &mut bytes[64..], &mut r).unwrap();
        bytes.truncate(64 + n);
        let encoding_polls = polls;
        assert!(encoding_polls > 100);
        assert_eq!(replay_count(&bytes), Ok(1));
        let mut calls = 0;
        let mut cancel = || {
            calls += 1;
            calls >= encoding_polls / 2
        };
        let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
        let mut output = vec![0; n];
        assert_eq!(
            encode_envelope(initial, envelope, &mut output, &mut r),
            Err(WalError::Cancelled)
        );
        assert!(r.consumed() > 65_536);
        let mut polls = 0;
        let mut cancel = || {
            polls += 1;
            false
        };
        let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
        let mut replay = Replay::new(&bytes, initial, &mut r).unwrap();
        replay
            .next_envelope(&mut CarriageValidator::default(), &mut r)
            .unwrap();
        let decoding_polls = polls;
        let mut calls = 0;
        let mut cancel = || {
            calls += 1;
            calls >= decoding_polls / 2
        };
        let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
        let mut replay = Replay::new(&bytes, initial, &mut r).unwrap();
        assert_eq!(
            replay
                .next_envelope(&mut CarriageValidator::default(), &mut r)
                .unwrap_err(),
            WalError::Cancelled
        );
        assert!(r.consumed() > 65_536);
    }
}

fn framed_fixture(kind: BlockKind, payload: &[u8]) -> (Vec<u8>, RequiredRef) {
    use zeppelin_embed::property_graph::storage::artifact::{
        self, ArtifactIdentity, Block, ContainerKind,
    };
    let identity = ArtifactIdentity {
        store: base().store,
        artifact: reference().object.artifact,
        generation: GraphGeneration::new(0),
        creation_serial: 1,
    };
    let mut bytes = vec![0; payload.len() + 256];
    let n = artifact::encode_into(
        ContainerKind::Object,
        identity,
        &[Block { kind, payload }],
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    let frame = artifact::decode(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        &bytes,
    )
    .unwrap();
    let required = RequiredRef {
        object: ArtifactDescriptor {
            bytes: n as u32,
            checksum: u64::from_le_bytes(bytes[n - 8..].try_into().unwrap()),
            ..reference().object
        },
        block: frame.reference(0).unwrap(),
    };
    (bytes, required)
}

#[test]
fn framed_objects_reject_swapped_tree_roles_and_unknown_participant_tags() {
    use zeppelin_embed::property_graph::storage::{
        artifact::{self, ContainerKind},
        tree::{self, PageHeader, TreeKind},
    };
    let mut page = vec![0; tree::PAGE_BYTES];
    tree::encode_page(
        PageHeader {
            kind: TreeKind::Nodes,
            level: 0,
            generation: GraphGeneration::new(0),
        },
        &[],
        &mut page,
    )
    .unwrap();
    let (bytes, required) = framed_fixture(BlockKind::TreePage, &page);
    let frame = artifact::decode(ContainerKind::Object, None, &bytes).unwrap();
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    assert!(
        validate_required_block(
            required,
            RequiredRole::Tree(TreeKind::Nodes),
            &frame,
            &mut r
        )
        .is_ok()
    );
    assert_eq!(
        validate_required_block(
            required,
            RequiredRole::Tree(TreeKind::Relationships),
            &frame,
            &mut r
        ),
        Err(WalError::Participant)
    );
    for (role, version) in [(0u16, 1u16), (7, 1), (1, 2)] {
        let mut payload = b"ZGCP".to_vec();
        payload.extend(role.to_le_bytes());
        payload.extend(version.to_le_bytes());
        let (bytes, required) = framed_fixture(BlockKind::CommitParticipant, &payload);
        let frame = artifact::decode(ContainerKind::Object, None, &bytes).unwrap();
        assert!(
            validate_required_block(
                required,
                RequiredRole::Participant(ParticipantRole::Catalog),
                &frame,
                &mut r
            )
            .is_err()
        );
    }
}

#[test]
fn reclaim_completion_requires_disjoint_exact_candidate_partitions() {
    let initial = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        ..initial
    };
    let candidates = [reference().object];
    let completion = ReclaimComplete {
        id: BatchId::new(1).unwrap(),
        intent: reference(),
        completed: DescriptorList::Values(&candidates),
        remaining: DescriptorList::Values(&candidates),
    };
    let mut bytes = vec![0; 4096];
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let changes = [Change::ReclaimComplete(completion)];
    assert_eq!(
        encode_envelope(
            initial,
            Envelope {
                batch: BatchId::new(2).unwrap(),
                kind: EnvelopeKind::Maintenance,
                changes: &changes,
                state: target
            },
            &mut bytes,
            &mut r
        ),
        Err(WalError::Malformed)
    );
    let changes = [Change::ReclaimComplete(ReclaimComplete {
        remaining: DescriptorList::Values(&[]),
        ..completion
    })];
    encode_header(initial.store, 1, &mut bytes).unwrap();
    let n = encode_envelope(
        initial,
        Envelope {
            batch: BatchId::new(2).unwrap(),
            kind: EnvelopeKind::Maintenance,
            changes: &changes,
            state: target,
        },
        &mut bytes[64..],
        &mut r,
    )
    .unwrap();
    bytes.truncate(64 + n);
    assert_eq!(replay_count(&bytes), Ok(1));
    let (at, n) = frames(&bytes)[1];
    bytes[at + 64 + 16 + 96] = 1;
    repair_record(&mut bytes, at, n);
    repair_envelope(&mut bytes);
    assert_eq!(replay_count(&bytes), Err(WalError::Participant));
}

#[test]
fn missing_middle_extent_is_observed_before_any_batch_escapes() {
    use zeppelin_embed::property_graph::CanonicalContents;
    use zeppelin_embed::property_graph::storage::artifact::{self, ArtifactFrame, ContainerKind};
    // A fixture-only resolver models retained chunk availability. Actual chunk
    // artifact admission and ZGEX semantics are ZE43's separate responsibility.
    struct Extents<'a> {
        frame: &'a ArtifactFrame<'a>,
        chunks: [Option<&'a [u8]>; 3],
        expected: &'a [u8],
        fires: usize,
        state_calls: usize,
    }
    impl ReplayValidator for Extents<'_> {
        fn required(
            &mut self,
            reference: RequiredRef,
            role: RequiredRole,
            r: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            if role != RequiredRole::Canonical {
                return Ok(());
            }
            let descriptor = validate_required_block(reference, role, self.frame, r)?;
            if descriptor.get(..4) != Some(b"ZGEX".as_slice()) {
                return Err(WalError::Participant);
            }
            let mut consumed = 0;
            for (slot, expected) in self.expected.chunks(65_536).enumerate() {
                r.charge(1)?;
                let Some(actual) = self.chunks[slot] else {
                    self.fires += 1;
                    return Err(WalError::MissingArtifact);
                };
                r.charge(actual.len() as u64)?;
                if actual != expected {
                    return Err(WalError::Participant);
                }
                consumed += actual.len();
            }
            if consumed != self.expected.len() {
                return Err(WalError::Participant);
            }
            Ok(())
        }
        fn mutation(&mut self, _: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
            Ok(())
        }
        fn inventory(
            &mut self,
            _: InventoryChange,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn reclaim_intent(
            &mut self,
            _: ReclaimIntent<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn reclaim_complete(
            &mut self,
            _: ReclaimComplete<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            Err(WalError::Participant)
        }
        fn state(
            &mut self,
            _: CommitState<'_>,
            _: CommitState<'_>,
            _: &mut WalResources<'_>,
        ) -> Result<(), WalError> {
            self.state_calls += 1;
            Ok(())
        }
    }
    let text = "x".repeat(131_072);
    let mut labels = [];
    let mut properties = [];
    let image = CanonicalContents::node(&mut labels, &mut properties, Some(&text), None).unwrap();
    let mut canonical = Vec::new();
    image.write_to(&mut canonical, &mut || Ok(())).unwrap();
    let mut extent = b"ZGEX\x01\x00\x04\x00".to_vec();
    extent.extend((canonical.len() as u64).to_le_bytes());
    extent.extend(3u32.to_le_bytes());
    extent.extend(65_536u32.to_le_bytes());
    extent.extend([0; 8]);
    for id in 1u128..=3 {
        extent.extend(id.to_le_bytes());
        extent.extend(96u64.to_le_bytes());
        extent.extend(
            ((canonical.len() - (id as usize - 1) * 65_536).min(65_536) as u32 + 24).to_le_bytes(),
        );
        extent.extend(11u16.to_le_bytes());
        extent.extend(1u16.to_le_bytes());
    }
    let (artifact_bytes, root) = framed_fixture(BlockKind::ExtentList, &extent);
    let frame = artifact::decode(ContainerKind::Object, None, &artifact_bytes).unwrap();
    let initial_log = mutation_log();
    let mut cancel = || false;
    let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    let mut reader = Replay::new(&initial_log, base(), &mut r).unwrap();
    let ReplayStep::Envelope(envelope) = reader
        .next_envelope(&mut CarriageValidator::default(), &mut r)
        .unwrap()
    else {
        panic!("fixture")
    };
    let Change::Mutation(mutation) = envelope.changes().next_change(&mut r).unwrap().unwrap()
    else {
        panic!("fixture mutation")
    };
    let changes = [Change::Mutation(Mutation {
        canonical: Some(root),
        ..mutation
    })];
    let mut bytes = vec![0; 4096];
    encode_header(base().store, 1, &mut bytes).unwrap();
    let n = encode_envelope(
        base(),
        Envelope {
            batch: envelope.batch,
            kind: envelope.kind,
            changes: &changes,
            state: envelope.state,
        },
        &mut bytes[64..],
        &mut r,
    )
    .unwrap();
    bytes.truncate(64 + n);
    for missing in [false, true] {
        let mut chunks = [
            Some(&canonical[..65_536]),
            Some(&canonical[65_536..131_072]),
            Some(&canonical[131_072..]),
        ];
        if missing {
            chunks[1] = None;
        }
        let mut fixture = Extents {
            frame: &frame,
            chunks,
            expected: &canonical,
            fires: 0,
            state_calls: 0,
        };
        let mut replay = Replay::new(&bytes, base(), &mut r).unwrap();
        let result = replay.next_envelope(&mut fixture, &mut r);
        if missing {
            assert_eq!(result.unwrap_err(), WalError::MissingArtifact);
            assert_eq!((fixture.fires, fixture.state_calls), (1, 0));
        } else {
            assert!(matches!(result, Ok(ReplayStep::Envelope(_))));
            assert_eq!((fixture.fires, fixture.state_calls), (0, 1));
        }
    }
}

#[test]
fn complete_envelope_cap_includes_all_headers_and_change_count() {
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphOperation, GraphRevision,
        NodeId, OperationFields,
    };
    let namespace = "n".repeat(4 * 1024 * 1024);
    let initial = base();
    let target = CommitState {
        generation: GraphGeneration::new(1),
        sequence: 1,
        high_waters: HighWaters {
            node: 1,
            ..initial.high_waters
        },
        ..initial
    };
    let mutation = Mutation {
        provenance_version: 1,
        provenance: OperationFields {
            operation: GraphOperation::StructuredCreate,
            key: Some(ApplicationKey::new(EntityKind::Node, &namespace, "").unwrap()),
            requested_revision: GraphRevision::new(1).unwrap(),
            installed_revision: GraphRevision::new(1).unwrap(),
            expected: ExpectedGraphState::Absent,
            incarnation: EntityId::Node(NodeId::new(1).unwrap()),
            delete_mode: None,
            original_generation: target.generation,
        },
        live: true,
        canonical: Some(RequiredRef {
            block: PhysicalRef {
                kind: BlockKind::CanonicalImage,
                ..reference().block
            },
            ..reference()
        }),
        membership: Membership::default(),
    };
    let changes = [Change::Mutation(mutation); 4];
    let mut output = vec![0xa5; MAX_ENVELOPE_BYTES];
    let mut cancel = || false;
    let mut r = WalResources::new(1_000_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    assert_eq!(
        encode_envelope(
            initial,
            Envelope {
                batch: BatchId::new(1).unwrap(),
                kind: EnvelopeKind::Mutation,
                changes: &changes,
                state: target
            },
            &mut output,
            &mut r
        ),
        Err(WalError::Capacity)
    );
    assert!(output.iter().all(|v| *v == 0xa5));
    let changes = vec![Change::Mutation(mutation); 16_385];
    assert_eq!(
        encode_envelope(
            initial,
            Envelope {
                batch: BatchId::new(1).unwrap(),
                kind: EnvelopeKind::Mutation,
                changes: &changes,
                state: target
            },
            &mut output,
            &mut r
        ),
        Err(WalError::Capacity)
    );
    assert!(output.iter().all(|v| *v == 0xa5));
}
