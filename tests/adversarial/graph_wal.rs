//! PG7 exercises private WAL replay, not file publication or store recovery.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::property_graph::storage::artifact::{ArtifactId, BlockKind, PhysicalRef};
use zeppelin_embed::property_graph::wal::*;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed_adversarial_oracle::graph_wal::{self as oracle, State};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.wal.prefix",
    "property-graph.wal.transition",
    "property-graph.wal.corruption.fire",
    "property-graph.wal.corruption.clean",
    "property-graph.wal.missing.fire",
    "property-graph.wal.missing.clean",
    "property-graph.wal.cancel.fire",
    "property-graph.wal.cancel.clean",
    "property-graph.wal.budget.fire",
    "property-graph.wal.budget.clean",
];
fn primitive(v: CommitState<'_>) -> State {
    State {
        store: v.store.get(),
        generation: v.generation.get(),
        sequence: v.sequence,
        high_waters: [
            v.high_waters.node,
            v.high_waters.relationship,
            v.high_waters.symbols[0].into(),
            v.high_waters.symbols[1].into(),
            v.high_waters.symbols[2].into(),
            v.high_waters.symbols[3].into(),
            v.high_waters.creation_serial.into(),
        ],
    }
}
fn base(seed: u64) -> CommitState<'static> {
    let store = StoreInstanceId::new((u128::from(seed) << 64) | 7).unwrap();
    let artifact = ArtifactId::new((u128::from(seed) << 64) | 11).unwrap();
    let reference = RequiredRef {
        object: ArtifactDescriptor {
            store,
            artifact,
            generation: GraphGeneration::new(0),
            serial: 1,
            bytes: 200,
            family: 17,
            version: 1,
            checksum: 9,
        },
        block: PhysicalRef {
            artifact,
            offset: 96,
            length: 64,
            kind: BlockKind::CommitParticipant,
            version: 1,
        },
    };
    CommitState {
        store,
        generation: GraphGeneration::new(0),
        sequence: 0,
        graph: WalGraphRoots::default(),
        catalog: reference,
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
// Synthetic catalog resolver: only the scheduled missing-object failure is
// modeled. Real catalog/root semantics remain separately owned participants.
#[derive(Default)]
struct Resolver {
    missing: bool,
    fires: usize,
}
impl ReplayValidator for Resolver {
    fn required(
        &mut self,
        _: RequiredRef,
        _: RequiredRole,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        if self.missing {
            self.fires += 1;
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
        _: ChangeReader<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Ok(())
    }
}
fn observe(
    bytes: &[u8],
    base: CommitState<'_>,
    resolver: &mut Resolver,
    r: &mut WalResources<'_>,
) -> Result<Vec<State>, WalError> {
    let mut replay = Replay::new(bytes, base, r)?;
    let mut states = Vec::new();
    loop {
        match replay.next_envelope(resolver, r)? {
            ReplayStep::Envelope(v) => states.push(primitive(v.state)),
            ReplayStep::End(_) => return Ok(states),
        }
    }
}
fn clean(bytes: &[u8], base: CommitState<'_>) -> Result<Vec<State>, ()> {
    let mut cancel = || false;
    let mut r = WalResources::new(10_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    observe(bytes, base, &mut Resolver::default(), &mut r).map_err(|_| ())
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::wal_probe", seed);
    let nonce = rng.next_u64();
    let initial = base(nonce);
    let mut previous = initial;
    let mut bytes = vec![0; 8192];
    let mut offset = encode_header(initial.store, 1, &mut bytes).map_err(|e| e.to_string())?;
    let mut commits = Vec::new();
    let mut cancel = || false;
    let mut r = WalResources::new(10_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    for sequence in 1..=3 {
        let target = CommitState {
            generation: GraphGeneration::new(sequence),
            sequence,
            high_waters: HighWaters {
                node: (u128::from(nonce) << 64) + u128::from(sequence),
                ..previous.high_waters
            },
            ..previous
        };
        let envelope = Envelope {
            batch: BatchId::new((u128::from(nonce) << 64) + u128::from(sequence)).unwrap(),
            kind: EnvelopeKind::Mutation,
            changes: &[],
            state: target,
        };
        let n = encode_envelope(previous, envelope, &mut bytes[offset..], &mut r)
            .map_err(|e| e.to_string())?;
        oracle::check_transition(&primitive(previous), &primitive(target), true)?;
        let bad = CommitState {
            high_waters: HighWaters {
                creation_serial: 0,
                ..target.high_waters
            },
            ..target
        };
        oracle::check_transition(
            &primitive(previous),
            &primitive(bad),
            encode_envelope(
                previous,
                Envelope {
                    state: bad,
                    ..envelope
                },
                &mut bytes[offset + n..],
                &mut r,
            )
            .is_ok(),
        )?;
        offset += n;
        commits.push((offset, primitive(target)));
        previous = target;
    }
    bytes.truncate(offset);
    coverage.hit(REQUIRED_COVERAGE[1]);
    let mut prefixes = vec![64, 65, 191, 192, bytes.len() - 1, bytes.len()];
    for (end, _) in &commits {
        prefixes.extend([end - 1, *end]);
    }
    for _ in 0..8 {
        prefixes.push(64 + (rng.next_u64() as usize % (bytes.len() - 63)));
    }
    for end in prefixes {
        oracle::check_prefix(&commits, end, None, &clean(&bytes[..end], initial))?;
    }
    coverage.hit(REQUIRED_COVERAGE[0]);
    let states: Vec<_> = commits.iter().map(|(_, s)| s.clone()).collect();
    for (fault, index) in [
        ("corruption", 2),
        ("missing", 4),
        ("cancel", 6),
        ("budget", 8),
    ] {
        oracle::check_fault(&states, false, 0, &clean(&bytes, initial))?;
        coverage.hit(REQUIRED_COVERAGE[index + 1]);
        let (observed, fires) = match fault {
            "corruption" => {
                let mut damaged = bytes.clone();
                damaged[128] ^= 1;
                let result = clean(&damaged, initial);
                oracle::check_prefix(&commits, damaged.len(), Some(192), &result)?;
                (result, 1)
            }
            "missing" => {
                let mut resolver = Resolver {
                    missing: true,
                    fires: 0,
                };
                let mut cancel = || false;
                let mut r =
                    WalResources::new(10_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
                let result = observe(&bytes, initial, &mut resolver, &mut r).map_err(|_| ());
                (result, resolver.fires)
            }
            "cancel" => {
                let (mut polls, mut fires) = (0, 0);
                let mut cancel = || {
                    polls += 1;
                    if polls == 32 {
                        fires += 1;
                    }
                    polls >= 32
                };
                let mut r =
                    WalResources::new(10_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
                let result = observe(&bytes, initial, &mut Resolver::default(), &mut r);
                if result != Err(WalError::Cancelled) || r.consumed() == 0 {
                    return Err("PG7 cancellation did not reach in-work checkpoint".into());
                }
                (result.map_err(|_| ()), fires)
            }
            _ => {
                let mut cancel = || false;
                let mut r = WalResources::new(200, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
                let result = observe(&bytes, initial, &mut Resolver::default(), &mut r);
                let fires = usize::from(result == Err(WalError::WorkLimit));
                if r.consumed() == 0 {
                    return Err("PG7 work limit did not reach real codec processing".into());
                }
                (result.map_err(|_| ()), fires)
            }
        };
        oracle::check_fault(&states, true, fires, &observed)?;
        coverage.hit(REQUIRED_COVERAGE[index]);
    }
    Ok(())
}
