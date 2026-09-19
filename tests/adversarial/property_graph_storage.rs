//! Seeded physical artifact allocation and checked reopen, before publication.
use super::{
    coverage::CoverageRegistry,
    fault_vfs::{FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledVfs},
};
use rand::RngCore;
use std::{collections::BTreeMap, path::Path};
use zeppelin_embed::property_graph::storage::{
    allocation::{AllocationError, ArtifactAllocator, EntropyProvider, artifact_path},
    artifact::{self, ArtifactId, Block, BlockKind, ContainerKind},
};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed::vfs::{CountingVfs, StdVfs};
use zeppelin_embed_adversarial_oracle::property_graph_storage::{
    Case, Observation, Outcome, check,
};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.artifact.clean",
    "property-graph.artifact.collision",
    "property-graph.artifact.entropy",
    "property-graph.artifact.before-create",
    "property-graph.artifact.after-create",
    "property-graph.artifact.torn",
    "property-graph.artifact.bit-flip",
];
struct Nonce {
    value: u128,
    fail: bool,
    calls: usize,
}
impl EntropyProvider for Nonce {
    fn fill_nonce(&mut self, output: &mut [u8; 16]) -> std::io::Result<()> {
        self.calls += 1;
        if self.fail {
            return Err(std::io::Error::other("injected graph entropy failure"));
        }
        *output = self.value.to_le_bytes();
        Ok(())
    }
}
fn inventory(path: &Path) -> std::io::Result<BTreeMap<String, Vec<u8>>> {
    std::fs::read_dir(path)?
        .map(|entry| {
            let entry = entry?;
            Ok((
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path())?,
            ))
        })
        .collect()
}
fn observe(case: Case, store: u128, artifact: u128, payload: &[u8]) -> Result<Observation, String> {
    let run = || -> Result<Observation, Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("published"), b"published exact bytes")?;
        std::fs::write(directory.path().join("orphan"), b"orphan exact bytes")?;
        let candidate = artifact_path(directory.path(), ArtifactId::new(artifact)?);
        if case == Case::Collision {
            std::fs::write(&candidate, b"collision belongs to another attempt")?;
        }
        let before = inventory(directory.path())?;
        let mode = match case {
            Case::BeforeCreate => Some(FaultMode::Eio),
            Case::AfterCreate => Some(FaultMode::PostCommitError),
            Case::Torn => Some(FaultMode::TornWrite),
            Case::BitFlip => Some(FaultMode::BitFlip),
            _ => None,
        };
        let schedule = mode.map_or_else(FaultSchedule::default, |mode| {
            FaultSchedule::single(FaultEvent {
                id: format!("graph-artifact-{case:?}"),
                op_index: 0,
                layer: Layer::Io,
                site: FaultSite::Write,
                mode,
                nth_match: 1,
                expected_matches: None,
                deadline_budget_seconds: None,
                path_contains: Some("graph-".into()),
                fired: false,
                fire_count: 0,
                path: None,
            })
        });
        let fs = CountingVfs::new(ScheduledVfs::new(StdVfs, schedule));
        fs.inner().set_operation(0);
        let mut nonce = Nonce {
            value: artifact,
            fail: case == Case::Entropy,
            calls: 0,
        };
        let blocks = [Block {
            kind: BlockKind::CanonicalImage,
            payload,
        }];
        let mut buffer = vec![0; 152 + payload.len()];
        let result = ArtifactAllocator {
            filesystem: &fs,
            directory: directory.path(),
            store: StoreInstanceId::new(store)?,
            entropy: &mut nonce,
        }
        .create(GraphGeneration::new(7), 19, &blocks, &mut buffer);
        let (outcome, attempted) = match result {
            Ok(identity) => (Outcome::Created, Some(identity.artifact.get())),
            Err(error) => {
                let attempted = error.attempted_artifact().map(ArtifactId::get);
                let outcome = match error {
                    AllocationError::Collision { .. } => Outcome::Collision,
                    AllocationError::Entropy(_) => Outcome::Entropy,
                    AllocationError::CreateFailed { .. } => Outcome::CreateFailed,
                    AllocationError::Format(error) => return Err(Box::new(error)),
                };
                (outcome, attempted)
            }
        };
        let after = inventory(directory.path())?;
        let candidate_bytes = match std::fs::read(&candidate) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Box::new(e)),
        };
        let decoded = candidate_bytes.as_ref().and_then(|bytes| {
            let frame = artifact::decode(ContainerKind::Object, None, bytes).ok()?;
            let payload = frame.resolve_framed_block(frame.reference(0).ok()?).ok()?;
            Some((
                frame.identity().store.get(),
                frame.identity().artifact.get(),
                payload.to_vec(),
            ))
        });
        Ok(Observation {
            outcome,
            attempted,
            decoded,
            candidate_present: candidate_bytes.is_some(),
            protected_unchanged: before
                .iter()
                .all(|(name, bytes)| after.get(name) == Some(bytes)),
            inventory_count: after.len(),
            entropy_calls: nonce.calls,
            fault_fires: fs
                .inner()
                .events()
                .iter()
                .map(|event| event.fire_count)
                .sum(),
            write_calls: fs.write_calls(),
            bytes_written: fs.bytes_written(),
        })
    };
    run().map_err(|error| error.to_string())
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::artifact_probe", seed);
    let store = (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64()) | (1 << 100);
    let artifact = (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64()) | (1 << 96);
    let mut payload = vec![0; 128];
    rng.fill_bytes(&mut payload);
    for (case, key) in [
        Case::Clean,
        Case::Collision,
        Case::Entropy,
        Case::BeforeCreate,
        Case::AfterCreate,
        Case::Torn,
        Case::BitFlip,
    ]
    .into_iter()
    .zip(REQUIRED_COVERAGE)
    {
        // Every injected episode has an actual same-seed, same-input clean control.
        let clean = observe(Case::Clean, store, artifact, &payload)?;
        check(Case::Clean, store, artifact, &payload, &clean)?;
        let observed = observe(case, store, artifact, &payload)?;
        check(case, store, artifact, &payload, &observed)
            .map_err(|e| format!("{e}; seed={seed}"))?;
        coverage.hit(*key);
    }
    Ok(())
}

#[test]
fn artifact_oracle_rejects_each_changed_observable() {
    let store = 1 << 100;
    let artifact = 1 << 96;
    let payload = b"oracle control";
    for case in [
        Case::Clean,
        Case::Collision,
        Case::Entropy,
        Case::BeforeCreate,
        Case::AfterCreate,
        Case::Torn,
        Case::BitFlip,
    ] {
        let clean = observe(case, store, artifact, payload).unwrap();
        check(case, store, artifact, payload, &clean).unwrap();
        for field in 0..10 {
            let mut changed = clean.clone();
            match field {
                0 => {
                    changed.outcome = if changed.outcome == Outcome::Created {
                        Outcome::Entropy
                    } else {
                        Outcome::Created
                    }
                }
                1 => changed.attempted = Some(1),
                2 => changed.decoded = Some((1, 2, vec![3])),
                3 => changed.candidate_present = !changed.candidate_present,
                4 => changed.protected_unchanged = false,
                5 => changed.inventory_count += 1,
                6 => changed.entropy_calls += 1,
                7 => changed.fault_fires += 1,
                8 => changed.write_calls += 1,
                _ => changed.bytes_written += 1,
            }
            assert!(
                check(case, store, artifact, payload, &changed).is_err(),
                "accepted altered field {field} in {case:?}"
            );
        }
    }
}
