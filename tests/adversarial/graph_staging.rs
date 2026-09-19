//! PG10 public staging seam, primitive history oracle and paired private faults.
//! The retained base is a fixture adapter: this is not publication/WAL evidence.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::{
    fts::tokenizer::{TokenizerConfig, TokenizerEpoch},
    lifecycle::{OpenOptions, Store},
    property_graph::{catalog::*, resources::GraphResources, staging::*, *},
};
use zeppelin_embed_adversarial_oracle::{
    graph_key_lifecycle::{Action, Expected, Origin, Record, Rejection},
    graph_staging::{self as model, Accepted, Disposition, Outcome, Refusal, Request},
};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.staging.changed",
    "property-graph.staging.replay",
    "property-graph.staging.mixed",
    "property-graph.staging.noop",
    "property-graph.staging.duplicate",
    "property-graph.staging.rejected",
    "property-graph.staging.cancel.fire",
    "property-graph.staging.cancel.clean",
    "property-graph.staging.budget.fire",
    "property-graph.staging.budget.clean",
    "property-graph.staging.oracle.can-fire",
];
fn key(name: &str) -> ApplicationKey<'_> {
    ApplicationKey::new(EntityKind::Node, "pg10", name).unwrap()
}
fn id(value: u128) -> EntityId {
    EntityId::Node(NodeId::new(value).unwrap())
}
fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).unwrap()
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
fn property(bits: u64) -> [GraphProperty<'static>; 1] {
    [GraphProperty::new(
        GraphName::new("v").unwrap(),
        PropertyValue::new(PropertyData::F64(f64::from_bits(bits))).unwrap(),
    )]
}
fn bytes(bits: u64) -> Vec<u8> {
    let mut p = property(bits);
    let image = CanonicalContents::node(&mut [], &mut p, None, None).unwrap();
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    bytes
}
struct Row {
    name: String,
    record: Record,
    bytes: Vec<u8>,
}
impl CanonicalSource for Row {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> std::io::Result<usize> {
        CanonicalSlice(&self.bytes).read_at(offset, out)
    }
}
impl Row {
    fn provenance(&self) -> OperationProvenance<'_> {
        let r = self.record;
        OperationProvenance::from_fields(
            Some(1),
            OperationFields {
                operation: origin(r.origin),
                key: Some(key(&self.name)),
                requested_revision: revision(r.revision),
                installed_revision: revision(r.revision),
                expected: match r.expected {
                    Expected::Absent => ExpectedGraphState::Absent,
                    Expected::Entity(value) => ExpectedGraphState::Entity(id(value)),
                    Expected::Deletion(value) => ExpectedGraphState::Deletion(revision(value)),
                },
                incarnation: id(r.id),
                delete_mode: r.detach.map(mode),
                original_generation: GraphGeneration::new(r.generation),
            },
        )
        .unwrap()
    }
    fn entity(&self, view: BaseIdentity) -> BaseEntity<'_> {
        BaseEntity {
            view,
            provenance: self.provenance(),
            shape: EntityShape::Node,
            fingerprint: CanonicalFingerprint::new(
                self.bytes.len() as u64,
                fingerprint_hash(&self.bytes),
            )
            .unwrap(),
            source: self,
            membership: Membership::default(),
        }
    }
}
// The real public canonical fingerprint keeps the base independent of private encoders.
fn fingerprint_hash(bytes: &[u8]) -> u64 {
    let bits = u64::from_le_bytes(bytes[33..41].try_into().unwrap());
    let mut p = property(bits);
    CanonicalContents::node(&mut [], &mut p, None, None)
        .unwrap()
        .fingerprint(&mut || Ok(()))
        .unwrap()
        .hash()
}
struct Base {
    rows: Vec<Row>,
    generation: u64,
    high: u128,
    interpretation: GraphInterpretation<'static>,
}
impl Base {
    fn new(rows: &[(u16, Record)], high: u128, generation: u64) -> Self {
        Self {
            rows: rows
                .iter()
                .map(|(k, r)| Row {
                    name: k.to_string(),
                    record: *r,
                    bytes: bytes(r.bits.unwrap_or(0)),
                })
                .collect(),
            high,
            generation,
            interpretation: GraphInterpretation::new(
                TokenizerEpoch::of(&TokenizerConfig::text_default()),
                None,
            )
            .unwrap(),
        }
    }
}
impl AdmittedBase for Base {
    fn identity(&self) -> BaseIdentity {
        BaseIdentity {
            store: StoreInstanceId::new(1).unwrap(),
            generation: GraphGeneration::new(self.generation),
            roots: (self.generation != 0).then(|| storage::artifact::ArtifactId::new(1).unwrap()),
        }
    }
    fn high_waters(&self) -> HighWaters {
        HighWaters {
            node: self.high,
            symbols: SymbolHighWaters {
                namespace: 1,
                property: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        self.interpretation
    }
    fn key(
        &self,
        k: ApplicationKey<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        Ok(match self.rows.iter().find(|r| key(&r.name) == k) {
            None => BaseKeyState::NeverUsed,
            Some(r) if r.record.bits.is_none() => {
                BaseKeyState::Deleted(self.identity(), r.provenance())
            }
            Some(r) => BaseKeyState::Live(r.entity(self.identity())),
        })
    }
    fn entity(
        &self,
        target: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        Ok(self
            .rows
            .iter()
            .find(|r| id(r.record.id) == target && r.record.bits.is_some())
            .map(|r| r.entity(self.identity())))
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
        kind: SymbolKind,
        name: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        Ok(match (kind, name.as_str()) {
            (SymbolKind::Namespace, "pg10") | (SymbolKind::Property, "v") => {
                Some(Symbol::new(kind, 1).unwrap())
            }
            _ => None,
        })
    }
}
fn bits_from_image(bytes: Option<&[u8]>) -> Result<Option<u64>, StageError> {
    let Some(b) = bytes else { return Ok(None) };
    // Independent explicit ZGCI-v1 one-F64-property fixture layout, not an engine decoder.
    if b.len() != 43
        || &b[..7] != b"ZGCI\x01\0\x01"
        || b[7..15] != [0; 8]
        || b[15..23] != 1u64.to_le_bytes()
        || b[23..31] != 1u64.to_le_bytes()
        || b[31] != b'v'
        || b[32] != 4
        || b[41..43] != [0, 0]
    {
        return Err(StageError::InvalidInput);
    }
    Ok(Some(u64::from_le_bytes(b[33..41].try_into().unwrap())))
}
fn record(p: OperationProvenance<'_>, bits: Option<u64>) -> Record {
    let p = p.fields();
    Record {
        id: match p.incarnation {
            EntityId::Node(id) => id.get(),
            _ => panic!("node fixture"),
        },
        revision: p.installed_revision.get(),
        origin: match p.operation {
            GraphOperation::StructuredCreate => Origin::Create,
            GraphOperation::StructuredPut => Origin::Put,
            GraphOperation::StructuredDelete => Origin::Delete,
            GraphOperation::StructuredRecreate => Origin::Recreate,
            GraphOperation::CypherEdit => Origin::Cypher,
        },
        expected: match p.expected {
            ExpectedGraphState::Absent => Expected::Absent,
            ExpectedGraphState::Entity(EntityId::Node(id)) => Expected::Entity(id.get()),
            ExpectedGraphState::Deletion(r) => Expected::Deletion(r.get()),
            _ => panic!("node fixture"),
        },
        detach: p.delete_mode.map(|m| m == GraphDeleteMode::Detach),
        generation: p.original_generation.get(),
        bits,
    }
}
fn execute(
    base: &Base,
    requests: &[Request],
    memory: &WriteMemory<'_>,
    control: &mut WriteControl<'_>,
) -> Result<Outcome, StageError> {
    let names: Vec<_> = requests.iter().map(|r| r.key.to_string()).collect();
    let mut props: Vec<_> = requests
        .iter()
        .map(|r| {
            property(match r.action {
                Action::Create { bits, .. }
                | Action::Put { bits, .. }
                | Action::Recreate { bits, .. } => bits,
                _ => 0,
            })
        })
        .collect();
    let images: Vec<_> = props
        .iter_mut()
        .map(|p| CanonicalContents::node(&mut [], p, None, None).unwrap())
        .collect();
    let native: Vec<_> = requests
        .iter()
        .zip(&names)
        .zip(&images)
        .map(|((request, name), image)| {
            let (rev, operation, live) = match request.action {
                Action::Create { revision, .. } => (revision, StructuredOperation::Create, true),
                Action::Put {
                    revision, expected, ..
                } => (revision, StructuredOperation::Put(id(expected)), true),
                Action::Delete {
                    revision,
                    expected,
                    detach,
                } => (
                    revision,
                    StructuredOperation::Delete(id(expected), mode(detach)),
                    false,
                ),
                Action::Recreate {
                    revision, deleted, ..
                } => (
                    revision,
                    StructuredOperation::Recreate(self::revision(deleted)),
                    true,
                ),
                _ => panic!("structured trace"),
            };
            StructuredWrite {
                key: key(name),
                revision: revision(rev),
                operation,
                image: live.then_some(WriteImage::Node(image)),
            }
        })
        .collect();
    match stage_structured(base, &native, memory, control) {
        Err(StageError::Lifecycle(e)) => Ok(Outcome::Rejected(match e {
            KeyLifecycleError::DuplicateTarget => Refusal::Duplicate,
            KeyLifecycleError::GenerationOverflow => Refusal::Generation,
            other => Refusal::Lifecycle(match other {
                KeyLifecycleError::MissingKey => Rejection::Missing,
                KeyLifecycleError::Stale { .. } => Rejection::Stale,
                KeyLifecycleError::RevisionConflict => Rejection::Conflict,
                KeyLifecycleError::AlreadyExists => Rejection::Exists,
                KeyLifecycleError::DeletedKey => Rejection::Deleted,
                KeyLifecycleError::NotDeleted => Rejection::NotDeleted,
                KeyLifecycleError::IncarnationConflict => Rejection::Incarnation,
                KeyLifecycleError::DeletionRevisionConflict => Rejection::DeletionRevision,
                KeyLifecycleError::RevisionOverflow => Rejection::Overflow,
                other => return Err(StageError::Lifecycle(other)),
            }),
        })),
        Err(StageError::IdentityOverflow) => Ok(Outcome::Rejected(Refusal::Allocator)),
        Err(e) => Err(e),
        Ok(batch) => {
            if batch.receipts().len() != requests.len() {
                return Err(StageError::InvalidInput);
            }
            let mut changes = Vec::new();
            for delta in batch.deltas() {
                let p = delta.provenance();
                if p.version() != 1
                    || p.fields().requested_revision != p.fields().installed_revision
                    || delta.membership() != (Membership::default(), Membership::default())
                {
                    return Err(StageError::InvalidInput);
                }
                let k = p.fields().key.unwrap().key().as_str().parse().unwrap();
                changes.push((k, record(p, bits_from_image(delta.canonical())?)));
            }
            let mut receipts = Vec::new();
            for (request, receipt) in requests.iter().zip(batch.receipts()) {
                let r = if receipt.replayed {
                    base.rows
                        .iter()
                        .find(|r| r.name == request.key.to_string())
                        .unwrap()
                        .record
                } else {
                    changes.iter().find(|(k, _)| *k == request.key).unwrap().1
                };
                let r = Record {
                    id: match receipt.entity {
                        EntityId::Node(id) => id.get(),
                        _ => return Err(StageError::InvalidInput),
                    },
                    revision: receipt.revision.get(),
                    generation: receipt.generation.get(),
                    ..r
                };
                receipts.push((request.key, r, receipt.replayed));
            }
            Ok(Outcome::Accepted(Accepted {
                disposition: match batch.disposition() {
                    BatchDisposition::NoOp => Disposition::NoOp,
                    BatchDisposition::Replayed => Disposition::Replay,
                    BatchDisposition::Changed => Disposition::Changed,
                },
                high_water: batch.high_waters().node,
                receipts,
                changes,
            }))
        }
    }
}
fn checked(
    base: &Base,
    primitive: &[(u16, Record)],
    requests: &[Request],
    memory: &WriteMemory<'_>,
    coverage: &mut CoverageRegistry,
) -> Result<Outcome, String> {
    let observed = execute(base, requests, memory, &mut |_| Ok(())).map_err(|e| e.to_string())?;
    model::check(primitive, requests, base.high, base.generation, &observed)?;
    if memory.reserved_bytes() != 0 {
        return Err("PG10 retained writer capacity after outcome extraction".into());
    }
    coverage.hit(match &observed {
        Outcome::Rejected(Refusal::Duplicate) => REQUIRED_COVERAGE[4],
        Outcome::Rejected(_) => REQUIRED_COVERAGE[5],
        Outcome::Accepted(a) => match a.disposition {
            Disposition::NoOp => REQUIRED_COVERAGE[3],
            Disposition::Replay => REQUIRED_COVERAGE[1],
            Disposition::Changed => {
                if a.receipts.iter().any(|r| r.2) {
                    REQUIRED_COVERAGE[2]
                } else {
                    REQUIRED_COVERAGE[0]
                }
            }
        },
    });
    Ok(observed)
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::staging", seed);
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let resources = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let baseline = resources.reserved_bytes().map_err(|e| e.to_string())?;
    let memory = WriteMemory::new(&resources, WriteLimits::default()).map_err(|e| e.to_string())?;
    let old = Record {
        id: (1u128 << 111) + 9,
        revision: 4,
        origin: Origin::Create,
        expected: Expected::Absent,
        detach: None,
        generation: 5,
        bits: Some(rng.next_u64()),
    };
    let primitive = [(1, old)];
    let base = Base::new(&primitive, old.id, 7);
    let replay = Request {
        key: 1,
        action: Action::Create {
            revision: 4,
            bits: old.bits.unwrap(),
        },
    };
    let new = Request {
        key: 2,
        action: Action::Create {
            revision: 1,
            bits: rng.next_u64(),
        },
    };
    for requests in [
        vec![],
        vec![replay],
        vec![new],
        vec![replay, new],
        vec![replay, replay],
        vec![
            new,
            Request {
                key: 1,
                action: Action::Put {
                    revision: 3,
                    expected: old.id,
                    bits: 0,
                },
            },
        ],
    ] {
        checked(&base, &primitive, &requests, &memory, coverage)?;
    }
    for _ in 0..48 {
        let rev = 3 + u64::from(rng.next_u32() % 3);
        let bits = if rng.next_u32().is_multiple_of(2) {
            old.bits.unwrap()
        } else {
            rng.next_u64()
        };
        let action = match rng.next_u32() % 4 {
            0 => Action::Create {
                revision: rev,
                bits,
            },
            1 => Action::Put {
                revision: rev,
                expected: old.id,
                bits,
            },
            2 => Action::Delete {
                revision: rev,
                expected: old.id,
                detach: true,
            },
            _ => Action::Recreate {
                revision: rev,
                deleted: 4,
                bits,
            },
        };
        checked(
            &base,
            &primitive,
            &[new, Request { key: 1, action }],
            &memory,
            coverage,
        )?;
    }
    let requests = [replay, new];
    let clean = checked(&base, &primitive, &requests, &memory, coverage)?;
    let mut calls = 0;
    execute(&base, &requests, &memory, &mut |_| {
        calls += 1;
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    for cut in [1, calls / 2, calls] {
        let mut seen = 0;
        let result = execute(&base, &requests, &memory, &mut |_| {
            seen += 1;
            if seen == cut {
                Err(StageError::Cancelled)
            } else {
                Ok(())
            }
        });
        if result.is_ok()
            || seen != cut
            || memory.reserved_bytes() != 0
            || resources.reserved_bytes().map_err(|e| e.to_string())? != baseline
        {
            return Err(format!(
                "PG10 cancellation did not fire/release cut={cut} seen={seen}"
            ));
        }
        coverage.hit(REQUIRED_COVERAGE[6]);
        if checked(&base, &primitive, &requests, &memory, coverage)? != clean {
            return Err("PG10 clean fault pair differs".into());
        }
        coverage.hit(REQUIRED_COVERAGE[7]);
    }
    let limited = WriteMemory::new(
        &resources,
        WriteLimits {
            writer_bytes: 1,
            ..WriteLimits::default()
        },
    )
    .map_err(|e| e.to_string())?;
    if !matches!(
        execute(&base, &requests, &limited, &mut |_| Ok(())),
        Err(StageError::Limit)
    ) || limited.reserved_bytes() != 0
    {
        return Err("PG10 writer budget did not fire/release".into());
    }
    coverage.hit(REQUIRED_COVERAGE[8]);
    if checked(&base, &primitive, &requests, &memory, coverage)? != clean {
        return Err("PG10 budget clean pair differs".into());
    }
    coverage.hit(REQUIRED_COVERAGE[9]);
    let Outcome::Accepted(mut corrupt) = clean else {
        return Err("PG10 clean mixed fixture rejected".into());
    };
    corrupt.receipts[0].1.generation += 1;
    if model::check(
        &primitive,
        &requests,
        old.id,
        7,
        &Outcome::Accepted(corrupt),
    )
    .is_ok()
    {
        return Err("PG10 comparator failed to fire".into());
    }
    coverage.hit(REQUIRED_COVERAGE[10]);
    if resources.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("PG10 shared capacity leaked".into());
    }
    Ok(())
}
