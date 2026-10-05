//! Directed ZE-40 recovery observations with an independent graph oracle.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{
    Operation, RelationshipRow, compare_read_view_expansion,
};

fn compare_state(
    expected: (u64, u64, u128, u128),
    actual: (u64, u64, u128, u128),
) -> Result<(), &'static str> {
    if expected == actual {
        Ok(())
    } else {
        Err("reopened recovery state differs from input history")
    }
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = zeppelin_embed::graph_recovery_test_support::run_actual_probe(seed);
    let expected = [
        Operation::CreateNode { id: 1 },
        Operation::CreateNode { id: 2 },
        Operation::CreateRelationship {
            rel: 1,
            source: 1,
            target: 2,
            relationship_type: 1,
        },
    ];
    let actual = [RelationshipRow {
        rel: report.state.relationship.rel,
        source: report.state.relationship.source,
        target: report.state.relationship.target,
        relationship_type: report.state.relationship.relationship_type,
    }];
    // The expected history installs the edge in generation 1; the seed is
    // only a probe input, not an oracle generation.
    compare_read_view_expansion(1, &expected, 1, &actual)
        .map_err(|difference| format!("recovery adjacency mismatch: {difference:?}"))?;
    if compare_read_view_expansion(1, &expected, 1, &[]).is_ok() {
        return Err("recovery comparator accepted a missing committed edge".into());
    }
    let state = (
        report.state.generation,
        report.state.sequence,
        report.state.first_node,
        report.state.second_node,
    );
    compare_state((1, 1, 1, 2), state).map_err(str::to_owned)?;
    if compare_state((2, 1, 1, 2), state).is_ok() {
        return Err("recovery comparator accepted an incorrect generation".into());
    }

    let mut seen = BTreeSet::new();
    for receipt in report.receipts {
        if !receipt.key.starts_with("property-graph.recovery.")
            || !seen.insert(receipt.key)
            || receipt.clean_controls == 0
        {
            return Err(format!("invalid recovery receipt {}", receipt.key));
        }
        if [
            "property-graph.recovery.lost-ack",
            "property-graph.recovery.torn-tail",
            "property-graph.recovery.serial-orphan",
            "property-graph.recovery.checkpoint",
            "property-graph.recovery.read-only",
        ]
        .contains(&receipt.key)
            && receipt.fires == 0
        {
            return Err(format!("unfired recovery boundary {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    if seen.len() != 9 {
        return Err("missing recovery boundary receipts".into());
    }
    probe_commit_boundaries(seed, coverage)?;
    #[cfg(unix)]
    probe_loss_modes(seed, coverage)?;
    Ok(())
}

use rand::{Rng, seq::SliceRandom};
use zeppelin_embed::graph_commit_recovery_test_support::{
    BatchObservation, Boundary, Fixture, ProbeStore,
};
use zeppelin_embed::property_graph::{EntityId, GraphGeneration, GraphWriteOutcome};

pub const COMMIT_KEYS: &[&str] = &[
    "property-graph.recovery.commit.artifact-create",
    "property-graph.recovery.commit.artifact-write",
    "property-graph.recovery.commit.artifact-sync",
    "property-graph.recovery.commit.directory-sync",
    "property-graph.recovery.commit.wal-append",
    "property-graph.recovery.commit.wal-partial-append",
    "property-graph.recovery.commit.wal-sync",
    "property-graph.recovery.commit.publication",
    "property-graph.recovery.commit.checkpoint-replace",
    "property-graph.recovery.commit.checkpoint-sync",
    "property-graph.recovery.commit.reclaim-unlink",
    "property-graph.recovery.commit.reclaim-sync",
    "property-graph.recovery.commit.reclaim-completion",
    "property-graph.recovery.commit.comparator",
    "property-graph.recovery.commit.resources",
];

pub fn schedule_for(seed: u64) -> (Fixture, Vec<Boundary>) {
    let mut rng = super::test_support::seeded_rng("property_graph::commit_recovery", seed);
    let fixture = Fixture {
        store: rng.random::<u128>() | 1,
        rank: rng.random_range(-1000..1000),
        text: format!("seeded graph document {}", rng.random::<u32>()),
        coordinates: [rng.random_range(0.25..2.0), rng.random_range(-2.0..-0.25)],
    };
    let mut boundaries = vec![
        Boundary::ArtifactCreate,
        Boundary::ArtifactWrite,
        Boundary::ArtifactSync,
        Boundary::DirectorySync,
        Boundary::WalAppend,
        Boundary::WalPartialAppend,
        Boundary::WalSync,
        Boundary::Publication,
        Boundary::CheckpointReplace,
        Boundary::CheckpointSync,
    ];
    boundaries.shuffle(&mut rng);
    (fixture, boundaries)
}

fn oracle_row(
    row: &zeppelin_embed::property_graph::storage::adjacency::RelationshipRow,
) -> RelationshipRow {
    RelationshipRow {
        rel: row.rel.get(),
        source: row.source.get(),
        target: row.target.get(),
        relationship_type: row.relationship_type.get(),
    }
}

/// One comparator for real and deliberately mutated complete observations.
/// Expected graph, logical contents and membership come only from fixture input.
pub fn compare_batch(fixture: &Fixture, actual: &BatchObservation) -> Result<(), String> {
    use zeppelin_embed_adversarial_oracle::graph_contents::{Contents, Observation, Value, check};
    if actual.store.get() != fixture.store
        || actual.generation.get() != 1
        || actual.sequence != 1
        || actual.first_revision != 1
        || actual.second_revision != 1
    {
        return Err("identity/revision/generation/sequence differs from complete batch".into());
    }
    if actual.node_count != 2
        || actual.canonical_properties != [1, 0, 0]
        || actual.canonical_modalities != [[true, false], [false, true], [false, false]]
        || actual.relationship_type_name != b"LINKS"
        || actual.label_counts != [0, 0]
        || actual.relationship_revision != 1
        || actual.original_generations != [1, 1, 1]
        || actual.keys != [b"a".to_vec(), b"b".to_vec(), b"ab".to_vec()]
        || actual.namespaces != [b"ze41".to_vec(), b"ze41".to_vec(), b"ze41".to_vec()]
    {
        return Err("canonical shape/provenance differs from fixture".into());
    }
    let operations = [
        Operation::CreateNode { id: 1 },
        Operation::CreateNode { id: 2 },
        Operation::CreateRelationship {
            rel: 1,
            source: 1,
            target: 2,
            relationship_type: 1,
        },
    ];
    compare_read_view_expansion(1, &operations, 1, &[oracle_row(&actual.relationship)])
        .map_err(|e| format!("canonical relationship: {e:?}"))?;
    for (name, rows) in [("OUT", &actual.outgoing), ("IN", &actual.incoming)] {
        let rows: Vec<_> = rows.iter().map(oracle_row).collect();
        compare_read_view_expansion(1, &operations, 1, &rows)
            .map_err(|e| format!("{name}: {e:?}"))?;
    }
    if actual.rank_property.len() != 9 || actual.rank_property.first() != Some(&3) {
        return Err("missing or malformed canonical rank".into());
    }
    let rank = i64::from_le_bytes(
        actual.rank_property[1..]
            .try_into()
            .map_err(|_| "rank extent")?,
    );
    let text = std::str::from_utf8(&actual.text).map_err(|_| "text encoding")?;
    let expected = Contents {
        relationship: None,
        labels: vec![],
        properties: vec![("rank", Value::Integer(fixture.rank))],
        text: Some(&fixture.text),
        embedding: Some((
            "ze41-document",
            fixture.coordinates.map(f32::to_bits).to_vec(),
        )),
    };
    let observed = Contents {
        relationship: None,
        labels: vec![],
        properties: vec![("rank", Value::Integer(rank))],
        text: Some(text),
        embedding: Some(("ze41-document", actual.vector_bits.clone())),
    };
    check(
        &expected,
        &observed,
        Observation {
            left_accepted: true,
            right_accepted: true,
            exact_equal: Some(true),
        },
    )?;
    if actual.sparse_vector_bits != fixture.coordinates.map(f32::to_bits)
        || actual.text_count != 1
        || actual.vector_count != 1
        || actual.text_membership != [true, false]
        || actual.vector_membership != [false, true]
    {
        return Err("complete batch search membership/payload differs".into());
    }
    Ok(())
}

fn compare_retry(
    outcome: GraphWriteOutcome,
    admitted_generation: u64,
    receipts: &[zeppelin_embed::property_graph::staging::ItemReceipt],
    present: bool,
) -> Result<(), String> {
    use zeppelin_embed_adversarial_oracle::graph_key_lifecycle::{
        Action, Observation, Record, predict,
    };
    if receipts.len() != 3 {
        return Err("incomplete retry receipts".into());
    }
    let expected_outcome = if present {
        GraphWriteOutcome::Replayed
    } else {
        GraphWriteOutcome::Committed {
            generation: GraphGeneration::new(1),
        }
    };
    if outcome != expected_outcome || admitted_generation != u64::from(present) {
        return Err("retry disposition/admitted generation differs".into());
    }
    for (index, receipt) in receipts.iter().enumerate() {
        let id = [1, 2, 1][index];
        if !matches!(
            (index, receipt.entity),
            (0 | 1, EntityId::Node(_)) | (2, EntityId::Relationship(_))
        ) {
            return Err("retry identity domain differs".into());
        }
        let actual_id = match receipt.entity {
            EntityId::Node(node) => node.get(),
            EntityId::Relationship(rel) => rel.get(),
        };
        let action = Action::Create {
            revision: 1,
            bits: index as u64,
        };
        let initial = match predict(None, action, id, 1) {
            Observation::Changed(record) => record,
            _ => return Err("invalid fixture history".into()),
        };
        let expected = predict(present.then_some(initial), action, id, 1);
        let observed_record = Record {
            id: actual_id,
            revision: receipt.revision.get(),
            generation: receipt.generation.get(),
            ..initial
        };
        let observed = if receipt.replayed {
            Observation::Replay(observed_record)
        } else {
            Observation::Changed(observed_record)
        };
        if expected != observed {
            return Err(format!("key/revision fence retry mismatch {index}"));
        }
    }
    Ok(())
}

pub fn comparator_mutations(fixture: &Fixture, good: &BatchObservation) -> Result<usize, String> {
    compare_batch(fixture, good)?;
    let mut mutations = Vec::new();
    macro_rules! mutate {
        ($field:ident, $value:expr) => {{
            let mut value = good.clone();
            value.$field = $value;
            mutations.push(value);
        }};
    }
    mutate!(
        store,
        zeppelin_embed::property_graph::StoreInstanceId::new(fixture.store ^ 2).unwrap()
    );
    mutate!(generation, GraphGeneration::new(2));
    mutate!(sequence, 2);
    mutate!(node_count, 3);
    mutate!(canonical_properties, [2, 0, 0]);
    mutate!(canonical_modalities, [[false, false]; 3]);
    mutate!(relationship_type_name, b"OTHER".to_vec());
    mutate!(label_counts, [1, 0]);
    mutate!(keys, Default::default());
    mutate!(namespaces, Default::default());
    mutate!(original_generations, [2, 1, 1]);
    mutate!(relationship_revision, 2);
    mutate!(first_revision, 2);
    mutate!(second_revision, 2);
    mutate!(rank_property, vec![3; 9]);
    mutate!(text, b"different text".to_vec());
    mutate!(vector_bits, vec![0; 2]);
    mutate!(sparse_vector_bits, vec![0; 2]);
    mutate!(text_count, 0);
    mutate!(vector_count, 0);
    mutate!(text_membership, [false, false]);
    mutate!(vector_membership, [false, false]);
    mutate!(outgoing, vec![]);
    mutate!(incoming, vec![]);
    for column in 0..4 {
        let mut value = good.clone();
        match column {
            0 => value.relationship.rel = zeppelin_embed::property_graph::RelId::new(2).unwrap(),
            1 => {
                value.relationship.source = zeppelin_embed::property_graph::NodeId::new(2).unwrap()
            }
            2 => {
                value.relationship.target = zeppelin_embed::property_graph::NodeId::new(1).unwrap()
            }
            _ => {
                value.relationship.relationship_type =
                    zeppelin_embed::property_graph::catalog::RelTypeId::new(2).unwrap()
            }
        }
        mutations.push(value);
    }
    for (index, mutation) in mutations.iter().enumerate() {
        if compare_batch(fixture, mutation).is_ok() {
            return Err(format!(
                "complete batch comparator accepted mutation {index}"
            ));
        }
    }
    Ok(mutations.len())
}

pub fn run_boundary_pair(
    fixture: &Fixture,
    boundary: Boundary,
    coverage: &mut CoverageRegistry,
) -> Result<
    (
        zeppelin_embed::graph_commit_recovery_test_support::BoundaryReport,
        zeppelin_embed::graph_commit_recovery_test_support::BoundaryReport,
    ),
    String,
> {
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let actual = zeppelin_embed::graph_commit_recovery_test_support::run_boundary(
        &root.path().join("fault"),
        fixture,
        boundary,
        true,
    );
    let clean = zeppelin_embed::graph_commit_recovery_test_support::run_boundary(
        &root.path().join("control"),
        fixture,
        boundary,
        false,
    );
    if actual.key != boundary.key()
        || actual.fires != 1
        || actual.clean_controls != 0
        || clean.fires != 0
        || clean.clean_controls != 1
    {
        return Err("invalid measured fire/control receipt".into());
    }
    if actual.protected_before != clean.protected_before {
        return Err("same-seed commit control input bytes differ".into());
    }
    let good = clean
        .observation
        .as_ref()
        .ok_or("clean control omitted batch")?;
    compare_batch(fixture, good)?;
    compare_retry(
        clean.retry.outcome(),
        clean.retry.admitted_generation().get(),
        clean.retry.receipts(),
        true,
    )?;
    retry_comparator_mutations(&clean.retry, true)?;
    let present = matches!(
        boundary,
        Boundary::WalSync
            | Boundary::Publication
            | Boundary::CheckpointReplace
            | Boundary::CheckpointSync
    );
    if actual.observation.is_some() != present {
        return Err(format!(
            "{boundary:?}: recovery crossed complete batch cutoff"
        ));
    }
    if let Some(observed) = &actual.observation {
        compare_batch(fixture, observed)?;
    }
    compare_retry(
        actual.retry.outcome(),
        actual.retry.admitted_generation().get(),
        actual.retry.receipts(),
        present,
    )?;
    retry_comparator_mutations(&actual.retry, present)?;
    // A faulted operation has not published a new memory owner. Its old
    // live-view baseline must remain; teardown must release every owner.
    let live_baseline = actual.reservation_before;
    if actual.reservation_after != live_baseline
        || actual.remaining_ownership != 0
        || clean.remaining_ownership != 0
    {
        return Err(format!(
            "{boundary:?}: reservation leak before={} after={} baseline={live_baseline} teardown={}/{}",
            actual.reservation_before,
            actual.reservation_after,
            actual.remaining_ownership,
            clean.remaining_ownership
        ));
    }
    comparator_mutations(fixture, good)?;
    run_after_boundary_pair(fixture, boundary, clean.post_sync_ordinal)?;
    coverage.hit(boundary.key());
    Ok((actual, clean))
}

pub fn probe_commit_boundaries(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let (fixture, boundaries) = schedule_for(seed);
    for boundary in boundaries {
        run_boundary_pair(&fixture, boundary, coverage)?;
    }
    for receipt in zeppelin_embed::graph_commit_recovery_test_support::run_reclaim_boundaries() {
        if receipt.fires != 1 || receipt.clean_controls != 1 {
            return Err("unmeasured reclaim boundary".into());
        }
        coverage.hit(receipt.key);
    }
    coverage.hit("property-graph.recovery.commit.comparator");
    coverage.hit("property-graph.recovery.commit.resources");
    Ok(())
}

pub const LOSS_KEYS: &[&str] = &[
    "property-graph.recovery.commit.process-kill",
    "property-graph.recovery.commit.durable-process-kill",
    "property-graph.recovery.commit.power-cut",
    "property-graph.recovery.commit.durable-power-cut",
    "property-graph.recovery.commit.corruption",
    "property-graph.recovery.commit.missing-object",
    "property-graph.recovery.commit.published-process-kill",
];

#[cfg(unix)]
pub fn process_child() {
    if let Ok(value) = std::env::var("ZE75_NONCE_SEED") {
        let seed = value.parse().unwrap();
        zeppelin_embed::graph_commit_recovery_test_support::with_qualification_nonces(
            seed,
            process_child_inner,
        );
    } else {
        process_child_inner();
    }
}
fn process_child_inner() {
    use super::fault_vfs::{FaultSchedule, ProcessCrashVfs, ScheduledVfs};
    use super::program::CrashBoundary;
    use std::sync::Arc;
    let path = std::path::PathBuf::from(std::env::var("ZE41_CHILD_PATH").unwrap());
    let seed = std::env::var("ZE41_CHILD_SEED").unwrap().parse().unwrap();
    let mode = std::env::var("ZE41_CHILD_SYNC").unwrap();
    let after_sync = mode == "1";
    let (fixture, _) = schedule_for(seed);
    let vfs = ProcessCrashVfs::new(
        ScheduledVfs::new(zeppelin_embed::vfs::StdVfs, FaultSchedule::default()),
        CrashBoundary::MidWalGroup,
    );
    let vfs = Arc::new(if after_sync {
        vfs.after_native_wal_sync()
    } else {
        vfs
    });
    let store = ProbeStore::create(&path, &fixture, vfs.clone());
    if mode != "2" {
        vfs.arm();
    }
    let result = store
        .apply(&fixture)
        .expect("child must reach kill boundary");
    if mode == "2" {
        assert_eq!(
            result.outcome(),
            GraphWriteOutcome::Committed {
                generation: GraphGeneration::new(1)
            }
        );
        let wal = std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("graph-wal-")
            })
            .unwrap();
        super::fault_vfs::kill_native_graph_process(&wal, "published-generation-1");
    }
    panic!("native process kill did not fire");
}

fn image(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>, String> {
    std::fs::read_dir(path)
        .map_err(|e| e.to_string())?
        .map(|entry| {
            let entry = entry.map_err(|e| e.to_string())?;
            Ok((
                entry.file_name(),
                std::fs::read(entry.path()).map_err(|e| e.to_string())?,
            ))
        })
        .collect()
}

/// Process loss retains cached complete envelopes. Modeled power loss retains
/// only synced bytes/dirents. Committed media damage refuses all admission.
#[cfg(unix)]
pub fn probe_loss_modes(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    lifecycle_loss_observations(seed, coverage).map(|_| ())
}
#[cfg(unix)]
pub fn lifecycle_loss_observations(
    seed: u64,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<zeppelin_embed_bench::harness_json::Value>, String> {
    use zeppelin_embed_bench::harness_json::json;
    let mut observations = Vec::new();
    use super::fault_vfs::{
        FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledVfs, SimulatedCrashVfs,
    };
    use std::{os::unix::process::ExitStatusExt, sync::Arc};
    use zeppelin_embed::vfs::StdVfs;
    let (fixture, _) = schedule_for(seed);
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    for mode in [0, 1, 2] {
        let name = ["kill", "durable-kill", "published-kill"][mode];
        let path = root.path().join(name);
        let child_log_path = root.path().join(format!("{name}-child.log"));
        let child_log = std::fs::File::create(&child_log_path).map_err(|e| e.to_string())?;
        let mut child =
            std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
                .args([
                    "--exact",
                    "ze41_process_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("ZE41_CHILD_PATH", &path)
                .env("ZE41_CHILD_SEED", seed.to_string())
                .env("ZE41_CHILD_SYNC", mode.to_string())
                .env(
                    "ZE75_NONCE_SEED",
                    seed.wrapping_add(mode as u64).to_string(),
                )
                .stdout(std::process::Stdio::null())
                .stderr(child_log)
                .spawn()
                .map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().map_err(|e| e.to_string())?;
                child.wait().map_err(|e| e.to_string())?;
                return Err("native process child exceeded 30 seconds".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        if status.signal() != Some(9) {
            return Err(format!(
                "native boundary did not SIGKILL: {status}: {}",
                std::fs::read_to_string(&child_log_path).map_err(|e| e.to_string())?
            ));
        }
        let receipt = std::fs::read_to_string(path.with_extension("kill-receipt"))
            .map_err(|e| e.to_string())?;
        let operation = ["append", "full-sync", "published-generation-1"][mode];
        if !receipt.starts_with(operation) || !receipt.contains("graph-wal-") {
            return Err("native kill has no matching path receipt".into());
        }
        let reopened = ProbeStore::open(&path).map_err(|e| format!("process reopen: {e:?}"))?;
        let actual = reopened
            .observe()
            .ok_or("process loss discarded a complete cached commit")?;
        compare_batch(&fixture, &actual)?;
        if reopened.close() != 0 {
            return Err("process recovered owner leaked".into());
        }
        let clean = ProbeStore::create(
            &root.path().join(format!("{name}-control")),
            &fixture,
            Arc::new(StdVfs),
        );
        clean
            .apply(&fixture)
            .map_err(|e| format!("process clean control: {e:?}"))?;
        compare_batch(&fixture, &clean.observe().ok_or("missing process control")?)?;
        if clean.close() != 0 {
            return Err("process clean owner leaked".into());
        }
        observations.push(json!({"model": "real-SIGKILL", "site": operation,
            "fires": 1, "controls": 1, "signal": status.signal(), "batch": format!("{actual:?}"),
            "files": image(&path)?.iter().map(|(n,b)| json!({"name": n.to_string_lossy(), "bytes": b, "digest": super::artifacts::evidence_digest(&[b])})).collect::<Vec<_>>() }));
        coverage.hit(LOSS_KEYS[[0, 1, 6][mode]]);
    }
    for durable in [false, true] {
        let path = root
            .path()
            .join(if durable { "durable-power" } else { "power" });
        let simulated = SimulatedCrashVfs::new(StdVfs);
        let event = FaultEvent {
            id: "ze41-wal-sync".into(),
            op_index: 1,
            layer: Layer::Io,
            site: FaultSite::Sync,
            mode: FaultMode::Eio,
            nth_match: 1,
            expected_matches: None,
            deadline_budget_seconds: None,
            path_contains: Some("graph-wal-".into()),
            fired: false,
            fire_count: 0,
            path: None,
        };
        let vfs = Arc::new(ScheduledVfs::new(
            simulated.clone(),
            if durable {
                FaultSchedule::default()
            } else {
                FaultSchedule::single(event)
            },
        ));
        let store = ProbeStore::create(&path, &fixture, vfs.clone());
        vfs.set_operation(1);
        let result = store.apply(&fixture);
        if durable {
            result.map_err(|e| format!("durable power commit: {e:?}"))?;
        } else {
            if result.expect_err("WAL sync must fail").nothing_committed() {
                return Err("attempted WAL sync claimed rollback".into());
            }
            let events = vfs.events();
            if events.len() != 1
                || events[0].fire_count != 1
                || !events[0]
                    .path
                    .as_ref()
                    .is_some_and(|p| p.to_string_lossy().contains("graph-wal-"))
            {
                return Err("power sync fault did not fire at native WAL".into());
            }
        }
        if store.release() != 0 {
            return Err("mapped power-image owner remains".into());
        }
        simulated.crash().map_err(|e| e.to_string())?;
        let reopened = ProbeStore::open(&path).map_err(|e| format!("power reopen: {e:?}"))?;
        let actual = reopened.observe();
        if actual.is_some() != durable {
            return Err("modeled power cut violated synced complete cutoff".into());
        }
        if let Some(actual) = &actual {
            compare_batch(&fixture, actual)?;
        }
        if reopened.close() != 0 {
            return Err("power recovered owner leaked".into());
        }
        let clean = ProbeStore::create(
            &root.path().join(if durable {
                "durable-power-control"
            } else {
                "power-control"
            }),
            &fixture,
            Arc::new(StdVfs),
        );
        clean.apply(&fixture).unwrap();
        compare_batch(&fixture, &clean.observe().ok_or("missing power control")?)?;
        if clean.close() != 0 {
            return Err("power clean owner leaked".into());
        }
        observations.push(json!({"model": "SimulatedCrashVfs-directory", "site": if durable { "full-sync" } else { "sync-error" },
            "fires": 1, "controls": 1, "batch": format!("{actual:?}"),
            "files": image(&path)?.iter().map(|(n,b)| json!({"name": n.to_string_lossy(), "bytes": b, "digest": super::artifacts::evidence_digest(&[b])})).collect::<Vec<_>>() }));
        coverage.hit(if durable { LOSS_KEYS[3] } else { LOSS_KEYS[2] });
    }
    // Each damaged image starts from a separately executed, verified clean
    // commit. No older-root fallback or mutation is allowed on refusal.
    for missing in [false, true] {
        let path = root
            .path()
            .join(if missing { "missing" } else { "corrupt" });
        let store = ProbeStore::create(&path, &fixture, Arc::new(StdVfs));
        let initial = image(&path)?;
        store.apply(&fixture).unwrap();
        compare_batch(
            &fixture,
            &store.observe().ok_or("damage control missing commit")?,
        )?;
        if store.release() != 0 {
            return Err("damage image still mapped".into());
        }
        let files = image(&path)?;
        if missing {
            let name = files
                .keys()
                .find(|name| {
                    name.to_string_lossy().ends_with(".zgraph") && !initial.contains_key(*name)
                })
                .ok_or("no newly referenced object")?;
            std::fs::remove_file(path.join(name)).map_err(|e| e.to_string())?;
        } else {
            let (name, data) = files
                .iter()
                .find(|(name, _)| name.to_string_lossy().starts_with("graph-wal-"))
                .ok_or("no native WAL")?;
            let mut bytes = data.clone();
            *bytes.last_mut().ok_or("empty WAL")? ^= 0x80;
            std::fs::write(path.join(name), bytes).map_err(|e| e.to_string())?;
        }
        let damaged = image(&path)?;
        let error = match ProbeStore::open(&path) {
            Ok(_) => return Err("committed corruption admitted an older plausible batch".into()),
            Err(error) => error,
        };
        use zeppelin_embed::property_graph::GraphStoreErrorKind;
        if if missing {
            !matches!(
                error.kind(),
                GraphStoreErrorKind::Corruption | GraphStoreErrorKind::Storage
            )
        } else {
            error.kind() != GraphStoreErrorKind::Corruption
        } {
            return Err(format!("damage refused for an unrelated cause: {error:?}"));
        }
        if image(&path)? != damaged {
            return Err("refused corruption mutated store files".into());
        }
        observations.push(json!({"model": "corrupt-media-refusal", "site": if missing { "missing-object" } else { "framed-corruption" },
            "fires": 1, "controls": 1, "error": format!("{:?}", error.kind()), "bytes_unchanged": true,
            "files": damaged.iter().map(|(n,b)| json!({"name": n.to_string_lossy(), "bytes": b, "digest": super::artifacts::evidence_digest(&[b])})).collect::<Vec<_>>() }));
        coverage.hit(if missing { LOSS_KEYS[5] } else { LOSS_KEYS[4] });
    }
    Ok(observations)
}

// writes.md requires failures on both sides of observable I/O boundaries.
// Reuse ScheduledVfs's post-operation errors rather than a second fault engine.
fn run_after_boundary_pair(
    fixture: &Fixture,
    boundary: Boundary,
    sync_ordinal: usize,
) -> Result<(), String> {
    use super::fault_vfs::{FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledVfs};
    use std::sync::Arc;
    use zeppelin_embed::vfs::StdVfs;
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let checkpoint = matches!(
        boundary,
        Boundary::CheckpointReplace | Boundary::CheckpointSync
    );
    let (site, needle) = match boundary {
        Boundary::ArtifactCreate | Boundary::ArtifactWrite => (FaultSite::Write, ".zgraph"),
        Boundary::ArtifactSync => (FaultSite::Sync, ".zgraph"),
        Boundary::DirectorySync => (FaultSite::Sync, "fault"),
        Boundary::WalAppend | Boundary::WalPartialAppend => (FaultSite::Append, "graph-wal-"),
        Boundary::WalSync => (FaultSite::Sync, "graph-wal-"),
        Boundary::CheckpointReplace => (FaultSite::Rename, "graph-root.ze"),
        Boundary::CheckpointSync => (FaultSite::Sync, "fault"),
        // The post-publication SIGKILL cell separately qualifies lost delivery.
        Boundary::Publication => return Ok(()),
    };
    let path = root.path().join("fault");
    let event = FaultEvent {
        id: format!("ze41-after-{boundary:?}"),
        op_index: 1,
        layer: Layer::Io,
        site,
        mode: FaultMode::PostCommitError,
        // Select the declared directory using its measured ordinal in the
        // independently executed same-seed control, then verify the path.
        nth_match: if matches!(boundary, Boundary::DirectorySync | Boundary::CheckpointSync) {
            sync_ordinal
        } else {
            1
        },
        expected_matches: None,
        deadline_budget_seconds: None,
        path_contains: Some(needle.into()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let vfs = Arc::new(ScheduledVfs::new(StdVfs, FaultSchedule::single(event)));
    let store = ProbeStore::create(&path, fixture, vfs.clone());
    if checkpoint {
        store.apply(fixture).unwrap();
    }
    vfs.set_operation(1);
    let result = if checkpoint {
        store.checkpoint()
    } else {
        store.apply(fixture).map(|_| ())
    };
    let error = result.expect_err("post-operation boundary must refuse");
    if !checkpoint
        && error.nothing_committed()
            != matches!(
                boundary,
                Boundary::ArtifactCreate
                    | Boundary::ArtifactWrite
                    | Boundary::ArtifactSync
                    | Boundary::DirectorySync
            )
    {
        return Err("post-operation error misclassified commit attempt".into());
    }
    let events = vfs.events();
    if events.len() != 1 || events[0].fire_count != 1 {
        return Err("post-operation fault did not fire once".into());
    }
    let fired_path = events[0]
        .path
        .as_ref()
        .ok_or("post-operation fault has no path")?;
    if matches!(boundary, Boundary::DirectorySync | Boundary::CheckpointSync)
        && !fired_path.ends_with("fault")
    {
        return Err(format!(
            "{boundary:?}: fault hit a file instead of directory: {fired_path:?}"
        ));
    }
    if store.release() != 0 {
        return Err("post-operation owner leaked".into());
    }
    let reopened =
        ProbeStore::open(&path).map_err(|e| format!("post-operation recovery: {e:?}"))?;
    let observation = reopened.observe();
    let present = matches!(
        boundary,
        Boundary::WalAppend
            | Boundary::WalPartialAppend
            | Boundary::WalSync
            | Boundary::CheckpointReplace
            | Boundary::CheckpointSync
    );
    if observation.is_some() != present {
        return Err(format!("{boundary:?}: post-operation recovery cutoff"));
    }
    if let Some(observation) = observation {
        compare_batch(fixture, &observation)?;
    }
    let retry = reopened.apply(fixture).expect("post-operation exact retry");
    compare_retry(
        retry.outcome(),
        retry.admitted_generation().get(),
        retry.receipts(),
        present,
    )?;
    if reopened.close() != 0 {
        return Err("post-operation recovered owner leaked".into());
    }
    let clean = ProbeStore::create(&root.path().join("control"), fixture, Arc::new(StdVfs));
    clean.apply(fixture).unwrap();
    if checkpoint {
        clean.checkpoint().unwrap();
    }
    compare_batch(
        fixture,
        &clean
            .observe()
            .ok_or("post-operation control missing batch")?,
    )?;
    if clean.close() != 0 {
        return Err("post-operation control owner leaked".into());
    }
    Ok(())
}

pub fn retry_comparator_mutations(
    retry: &zeppelin_embed::property_graph::GraphWriteResult,
    present: bool,
) -> Result<usize, String> {
    use zeppelin_embed::property_graph::{GraphRevision, NodeId, RelId};
    let compare =
        |outcome, generation, receipts: &[_]| compare_retry(outcome, generation, receipts, present);
    compare(
        retry.outcome(),
        retry.admitted_generation().get(),
        retry.receipts(),
    )?;
    if compare(
        GraphWriteOutcome::NoOp,
        retry.admitted_generation().get(),
        retry.receipts(),
    )
    .is_ok()
        || compare(retry.outcome(), u64::from(!present), retry.receipts()).is_ok()
    {
        return Err("retry comparator accepted altered disposition or generation".into());
    }
    let mut count = 2;
    for index in 0..retry.receipts().len() {
        for field in 0..5 {
            let mut receipts = retry.receipts().to_vec();
            let receipt = &mut receipts[index];
            match field {
                0 => {
                    receipt.entity = match receipt.entity {
                        EntityId::Node(_) => EntityId::Node(NodeId::new(3).unwrap()),
                        EntityId::Relationship(_) => EntityId::Relationship(RelId::new(3).unwrap()),
                    }
                }
                1 => {
                    receipt.entity = match receipt.entity {
                        EntityId::Node(_) => EntityId::Relationship(RelId::new(1).unwrap()),
                        EntityId::Relationship(_) => EntityId::Node(NodeId::new(1).unwrap()),
                    }
                }
                2 => receipt.revision = GraphRevision::new(2).unwrap(),
                3 => receipt.generation = GraphGeneration::new(2),
                _ => receipt.replayed = !receipt.replayed,
            }
            if compare(
                retry.outcome(),
                retry.admitted_generation().get(),
                &receipts,
            )
            .is_ok()
            {
                return Err(format!(
                    "retry comparator accepted item {index} mutation {field}"
                ));
            }
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    #[test]
    fn recovery_probe_seed_zero_validates_committed_adjacency() {
        super::probe(0, &mut super::CoverageRegistry::default()).unwrap();
    }
}
