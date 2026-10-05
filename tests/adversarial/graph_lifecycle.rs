//! ZE-75 operation-bound integration; the executors and fault engines are existing seams.
use super::artifacts::{EpisodeAttestation, RunArtifacts, evidence_digest};
use super::campaign::{CampaignKind, FaultPlan, FeatureOperation, PropertyGraphOperation};
use super::coverage::CoverageRegistry;
use super::fault_vfs::FaultSchedule;
use super::profiles::FaultProfile;
use super::program::{Op, Program};
use super::runner::{ComparisonOutcomeCounts, RunOutcome};
use std::collections::BTreeMap;
use std::path::Path;
use zeppelin_embed::graph_commit_recovery_test_support::{
    BatchObservation, Boundary, BoundaryReport, Fixture,
};
use zeppelin_embed::property_graph::EntityId;
use zeppelin_embed_adversarial_oracle::graph_fixture as primitive;
use zeppelin_embed_adversarial_oracle::graph_lifecycle as oracle;
use zeppelin_embed_bench::harness_json::{Value, json};

pub fn input(fixture: &Fixture) -> Vec<primitive::Mutation> {
    use primitive::*;
    [
        (
            Kind::Node,
            "a",
            Image::Node {
                labels: Default::default(),
                properties: BTreeMap::from([(
                    "rank".into(),
                    Property::Scalar(Scalar::I64(fixture.rank)),
                )]),
                text: Some(fixture.text.clone()),
                vector: None,
            },
        ),
        (
            Kind::Node,
            "b",
            Image::Node {
                labels: Default::default(),
                properties: Default::default(),
                text: None,
                vector: Some(fixture.coordinates.map(f32::to_bits).to_vec()),
            },
        ),
        (
            Kind::Relationship,
            "ab",
            Image::Relationship {
                source: 1,
                target: 2,
                relationship_type: "LINKS".into(),
                properties: Default::default(),
            },
        ),
    ]
    .into_iter()
    .map(|(kind, value, image)| Mutation {
        key: Key {
            kind,
            namespace: "ze41".into(),
            value: value.into(),
        },
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(image),
    })
    .collect()
}

pub fn snapshot(actual: Option<&BatchObservation>) -> Result<primitive::Snapshot, String> {
    use primitive::*;
    let Some(a) = actual else {
        return Ok(Snapshot::default());
    };
    let rank = a
        .rank_property
        .get(1..)
        .and_then(|b| b.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or("complete-prefix: invalid rank bytes")?;
    let key = |kind, index: usize| -> Result<Option<Key>, String> {
        Ok(Some(Key {
            kind,
            namespace: String::from_utf8(a.namespaces[index].clone()).map_err(|e| e.to_string())?,
            value: String::from_utf8(a.keys[index].clone()).map_err(|e| e.to_string())?,
        }))
    };
    Ok(Snapshot {
        nodes: vec![
            Node {
                id: a.relationship.source.get(),
                key: key(Kind::Node, 0)?,
                revision: a.first_revision,
                generation: a.original_generations[0],
                labels: Default::default(),
                properties: BTreeMap::from([("rank".into(), Property::Scalar(Scalar::I64(rank)))]),
                text: Some(String::from_utf8(a.text.clone()).map_err(|e| e.to_string())?),
                vector: None,
            },
            Node {
                id: a.relationship.target.get(),
                key: key(Kind::Node, 1)?,
                revision: a.second_revision,
                generation: a.original_generations[1],
                labels: Default::default(),
                properties: Default::default(),
                text: None,
                vector: Some(a.vector_bits.clone()),
            },
        ],
        relationships: vec![Relationship {
            id: a.relationship.rel.get(),
            key: key(Kind::Relationship, 2)?,
            revision: a.relationship_revision,
            generation: a.original_generations[2],
            source: a.relationship.source.get(),
            target: a.relationship.target.get(),
            relationship_type: String::from_utf8(a.relationship_type_name.clone())
                .map_err(|e| e.to_string())?,
            properties: Default::default(),
        }],
    })
}

pub fn identities(report: &BoundaryReport) -> Vec<oracle::Identity> {
    let mut result: Vec<_> = report
        .retry
        .receipts()
        .iter()
        .zip(["a", "b", "ab"])
        .map(|(r, key)| oracle::Identity {
            relationship: matches!(r.entity, EntityId::Relationship(_)),
            key: key.into(),
            id: match r.entity {
                EntityId::Node(id) => id.get(),
                EntityId::Relationship(id) => id.get(),
            },
            revision: r.revision.get(),
            generation: r.generation.get(),
            replayed: r.replayed,
        })
        .collect();
    result.push(oracle::Identity {
        relationship: false,
        key: "fresh".into(),
        id: report.fresh_identity.0,
        revision: report.fresh_identity.1,
        generation: report.fresh_identity.2,
        replayed: report.fresh_identity.3,
    });
    result
}
pub fn outcome(report: &BoundaryReport) -> oracle::Outcome {
    oracle::Outcome {
        error: report.error_kind.clone(),
        nothing_committed: report.nothing_committed,
        stopped_error: report.stopped_error.clone(),
    }
}
fn committed(boundary: Boundary) -> bool {
    matches!(
        boundary,
        Boundary::WalSync
            | Boundary::Publication
            | Boundary::CheckpointReplace
            | Boundary::CheckpointSync
    )
}
pub fn compare(
    fixture: &Fixture,
    boundary: Boundary,
    report: &BoundaryReport,
    fault: bool,
) -> Result<Vec<Value>, String> {
    let present = !fault || committed(boundary);
    let actual_snapshot = snapshot(report.observation.as_ref())?;
    oracle::compare_complete_prefix(&[input(fixture)], &[usize::from(present)], &actual_snapshot)?;
    let mut model = primitive::Graph::default();
    if present {
        model
            .apply(&input(fixture))
            .map_err(|e| format!("fixture {e:?}"))?;
    }
    let expected_snapshot = model.snapshot();
    if let Some(a) = &report.observation {
        super::graph_recovery::compare_batch(fixture, a)?;
    }
    let retry = model
        .apply(&input(fixture))
        .map_err(|e| format!("retry {e:?}"))?;
    let mut expected_ids: Vec<_> = retry
        .receipts
        .iter()
        .zip(["a", "b", "ab"])
        .enumerate()
        .map(|(i, (r, key))| oracle::Identity {
            relationship: i == 2,
            key: key.into(),
            id: r.id,
            revision: r.revision,
            generation: r.generation,
            replayed: r.replayed,
        })
        .collect();
    let fresh = primitive::Mutation {
        key: primitive::Key {
            kind: primitive::Kind::Node,
            namespace: "ze41".into(),
            value: "fresh".into(),
        },
        operation: primitive::Operation::Create,
        revision: 1,
        expected: primitive::Expectation::Absent,
        detach: false,
        image: Some(primitive::Image::Node {
            labels: Default::default(),
            properties: Default::default(),
            text: None,
            vector: None,
        }),
    };
    let allocation = model
        .apply(&[fresh])
        .map_err(|e| format!("fresh fixture {e:?}"))?;
    let receipt = allocation
        .receipts
        .first()
        .ok_or("fresh fixture receipt missing")?;
    expected_ids.push(oracle::Identity {
        relationship: false,
        key: "fresh".into(),
        id: receipt.id,
        revision: receipt.revision,
        generation: receipt.generation,
        replayed: receipt.replayed,
    });
    oracle::compare_identity_history(&expected_ids, &identities(report))?;
    let private = matches!(
        boundary,
        Boundary::ArtifactCreate
            | Boundary::ArtifactWrite
            | Boundary::ArtifactSync
            | Boundary::DirectorySync
    );
    let checkpoint = matches!(
        boundary,
        Boundary::CheckpointReplace | Boundary::CheckpointSync
    );
    let expected = oracle::Outcome {
        error: fault.then(|| {
            if private || checkpoint {
                "Storage"
            } else {
                "WriteIndeterminate"
            }
            .into()
        }),
        nothing_committed: fault && (private || checkpoint),
        stopped_error: (fault && !private && !checkpoint).then(|| "Unavailable".into()),
    };
    oracle::compare_outcome(&expected, &outcome(report))?;
    let protected = report
        .protected_before
        .iter()
        .filter(|(name, _)| name.ends_with(".zgraph"))
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    oracle::compare_protected_artifacts(&protected, &report.protected_after)?;
    if fault
        && boundary == Boundary::ArtifactCreate
        && !report.durable_image.values().any(|b| b == b"foreign-owner")
    {
        return Err("protected-artifacts: foreign collision bytes lost".into());
    }
    Ok(vec![
        json!({"checker_id": oracle::COMPLETE_PREFIX, "invariant": "I16", "expected": format!("{expected_snapshot:?}"), "observed": format!("{actual_snapshot:?}")}),
        json!({"checker_id": oracle::IDENTITY_HISTORY, "invariant": "I17", "expected": format!("{expected_ids:?}"), "observed": format!("{:?}", identities(report))}),
        json!({"checker_id": oracle::OUTCOME, "invariant": "I18", "expected": format!("{expected:?}"), "observed": format!("{:?}", outcome(report))}),
        json!({"checker_id": oracle::PROTECTED_ARTIFACTS, "invariant": "I19", "expected": protected, "observed": report.protected_after}),
    ])
}
fn boundary(operation: PropertyGraphOperation) -> Option<Boundary> {
    use PropertyGraphOperation as O;
    Some(match operation {
        O::ArtifactCreate => Boundary::ArtifactCreate,
        O::ArtifactWrite => Boundary::ArtifactWrite,
        O::ArtifactSync => Boundary::ArtifactSync,
        O::DirectorySync => Boundary::DirectorySync,
        O::WalAppend => Boundary::WalAppend,
        O::WalPartialAppend => Boundary::WalPartialAppend,
        O::WalSync => Boundary::WalSync,
        O::Publication => Boundary::Publication,
        O::CheckpointReplace => Boundary::CheckpointReplace,
        O::CheckpointSync => Boundary::CheckpointSync,
        _ => return None,
    })
}
fn report_json(report: &BoundaryReport) -> Value {
    json!({"site": report.key, "fires": report.fires, "controls": report.clean_controls,
        "raw_error": report.raw_error, "fresh_identity": report.fresh_identity,
        "outcome": {"error": report.error_kind, "nothing_committed": report.nothing_committed, "stopped_error": report.stopped_error},
        "batch": format!("{:?}", report.observation), "retry": format!("{:?}", identities(report)),
        "reservations": [report.reservation_before, report.reservation_after, report.remaining_ownership]})
}
fn images(operation: &str, leg: &str, report: &BoundaryReport) -> Value {
    json!({"operation": operation, "leg": leg, "files": report.durable_image.iter().map(|(name, bytes)|
        json!({"name": name, "bytes": bytes, "digest": evidence_digest(&[bytes])})).collect::<Vec<_>>()})
}
fn lines(records: &[Value]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for record in records {
        bytes
            .extend(zeppelin_embed_bench::harness_json::to_vec(record).map_err(|e| e.to_string())?);
        bytes.push(b'\n');
    }
    Ok(bytes)
}

pub fn run(
    seed: u64,
    profile: FaultProfile,
    overridden: bool,
    root: &Path,
) -> Result<RunOutcome, String> {
    zeppelin_embed::graph_commit_recovery_test_support::with_qualification_nonces(seed, || {
        run_inner(seed, profile, overridden, root)
    })
}
fn run_inner(
    seed: u64,
    profile: FaultProfile,
    overridden: bool,
    root: &Path,
) -> Result<RunOutcome, String> {
    if profile != FaultProfile::None {
        return Err(
            "property-graph uses its explicit operation-bound schedule; select --profile none"
                .into(),
        );
    }
    let _guard = super::feature_process_lock()
        .lock()
        .map_err(|e| e.to_string())?;
    let campaign = CampaignKind::PropertyGraph;
    let program = Program::generate_for(campaign, seed);
    let artifacts = RunArtifacts::create_for(root, campaign, seed, profile)?;
    let program_bytes = artifacts.write_program(&program)?;
    let mut plan =
        FaultPlan::for_program(campaign, seed, profile, &program, FaultSchedule::default());
    let (fixture, _) = super::graph_recovery::schedule_for(seed);
    let fixture_json = json!({"version": "graph-fixture-v1", "generator": "ze75-v1", "seed": seed,
        "store": fixture.store.to_string(), "rank": fixture.rank, "text": fixture.text,
        "vector_bits": fixture.coordinates.map(f32::to_bits), "history": format!("{:?}", input(&fixture)),
        "schedule": plan.feature.iter().map(|e| json!({"operation": e.op_index, "site": e.fault.key()})).collect::<Vec<_>>()});
    let fixture_bytes = lines(&[fixture_json])?;
    let input_digest = evidence_digest(&[&fixture_bytes]);
    let mut coverage = CoverageRegistry::default();
    let mut observations = Vec::new();
    let mut durable = Vec::new();
    let mut controls = Vec::new();
    let mut receipts = Vec::new();
    let mut checks = Vec::new();
    let mut counts = BTreeMap::<String, u64>::new();
    for (index, op) in program.ops.iter().enumerate() {
        let Op::Feature(FeatureOperation::PropertyGraph(operation)) = op else {
            return Err("graph program contains unrelated operation".into());
        };
        let event = plan
            .feature
            .iter_mut()
            .find(|e| e.op_index == index)
            .ok_or("missing operation fault selection")?;
        let mut operation_checks = Vec::new();
        let observed = if let Some(b) = boundary(*operation) {
            let (fault, clean) =
                super::graph_recovery::run_boundary_pair(&fixture, b, &mut coverage)?;
            for (leg, report, faulted) in [("fault", &fault, true), ("clean", &clean, false)] {
                for mut check in compare(&fixture, b, report, faulted)? {
                    check["leg"] = json!(leg);
                    operation_checks.push(check);
                }
            }
            durable.push(images(operation.key(), "fault", &fault));
            durable.push(images(operation.key(), "clean", &clean));
            json!({"fault": report_json(&fault), "clean": report_json(&clean)})
        } else {
            match operation {
                PropertyGraphOperation::Storage => {
                    let mut storage = super::graph_storage_faults::observe(seed, &mut coverage)?;
                    let adjacency = super::graph_adjacency_store::probe(seed, &mut coverage)?;
                    let growth = super::graph_adjacency::probe(seed, &mut coverage)?;
                    if adjacency.fault_fires == 0
                        || adjacency.clean_controls == 0
                        || growth.fault_fires == 0
                        || growth.clean_controls == 0
                    {
                        return Err("adjacency adaptation omitted measured controls".into());
                    }
                    storage["private_participant_adaptations"] = json!({"scope": "private finalized native adjacency producer, not public store recovery",
                    "history": adjacency.histories, "observations": format!("{:?}", adjacency.observations),
                    "adjacency": format!("{adjacency:?}"), "growth": format!("{growth:?}")});
                    storage
                }
                PropertyGraphOperation::Search => json!(
                    super::graph_search_qualification::lifecycle_observations(seed, &mut coverage)?
                ),
                PropertyGraphOperation::Reclaim => {
                    let mut report = super::graph_reclaim::observe(seed, &mut coverage)?;
                    let race = super::graph_reclaim::race_observation(seed, &mut coverage)?;
                    report["reader_race"] = race;
                    let reclaim = zeppelin_embed::graph_commit_recovery_test_support::run_ze75_reclaim_evidence(seed);
                    check_reclaim(&reclaim)?;
                    report["checkpoint_retry_reclaim"] = json!(reclaim.iter().map(|r| json!({
                    "cell": r.cell, "fires": r.fires, "controls": r.controls,
                    "provenance_phases": r.provenance_phases.iter().map(|p| p.iter().map(|(k,(v,c))| json!({"key": k, "provenance_bytes": v, "canonical_bytes": c})).collect::<Vec<_>>()).collect::<Vec<_>>(),
                    "inventory_pending": r.inventory_pending, "inventory_replayed": r.inventory_replayed,
                    "pending_proof": r.pending_proof, "replayed_proof": r.replayed_proof,
                    "completion": r.completion, "retry_receipts": r.retry_receipts, "error": r.error,
                })).collect::<Vec<_>>());
                    for r in &reclaim {
                        durable.push(json!({"operation": "reclaim", "cell": r.cell,
                        "input_files": r.input_image.iter().map(|(n,b)| json!({"name": n, "bytes": b, "digest": evidence_digest(&[b])})).collect::<Vec<_>>(),
                        "files": r.durable_image.iter().map(|(n,b)| json!({"name": n, "bytes": b, "digest": evidence_digest(&[b])})).collect::<Vec<_>>() }));
                    }
                    report
                }
                PropertyGraphOperation::Loss => {
                    #[cfg(unix)]
                    {
                        {
                            let mut losses = super::graph_recovery::lifecycle_loss_observations(
                                seed,
                                &mut coverage,
                            )?;
                            losses.push(fault_vfs_observation(&fixture)?);
                            json!(losses)
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        return Err("SIGKILL graph qualification requires Unix".into());
                    }
                }
                _ => return Err("unbound graph operation".into()),
            }
        };
        if boundary(*operation).is_none() {
            let (id, checker, expected) = match operation {
                PropertyGraphOperation::Storage => (
                    18,
                    "native-storage",
                    "ZE47/172 seeded storage refusals; OUT/IN, retained root and independent keyed model; every declared site has a measured control",
                ),
                PropertyGraphOperation::Search => (
                    49,
                    "search-materialization",
                    "ZE65 primitive corpus queries and acknowledged membership history; admitted-generation rows and complete results; no owner leak",
                ),
                PropertyGraphOperation::Reclaim => (
                    19,
                    "checkpoint-retry-reclaim",
                    "ZE46/98 input protected-reference union; unchanged complete provenance and candidate inventory/proof after checkpoint and replay; exact original retry receipts",
                ),
                PropertyGraphOperation::Loss => (
                    16,
                    "graph-loss-modes",
                    "SIGKILL retains cached complete batch; directory power-cut admits only full-synced batch; corrupt/missing committed media refuse without mutation; FaultVfs barrier versus full bytes",
                ),
                _ => return Err("unbound composite checker".into()),
            };
            operation_checks.push(json!({"invariant": format!("I{id}"), "checker_id": checker, "expected": expected, "observed": observed}));
        }
        // Counts come from executor receipts; each selected operation must have real measured pairs.
        let (fires, clean_count) = measured_counts(&observed)?;
        if fires == 0 || clean_count == 0 {
            return Err(format!("{} omitted measured fire/control", operation.key()));
        }
        event.fired = true;
        event.fire_count = 1;
        coverage.hit(event.fault.coverage_key());
        coverage.hit(format!("campaign.op.property-graph.{}", operation.key()));
        let row = json!({"version": 1, "op": index, "operation": operation.key(), "input_digest": input_digest,
            "fires": fires, "controls": clean_count, "observation": observed});
        observations.push(row);
        controls.push(json!({"op": index, "operation": operation.key(), "same_seed_control_passed": true,
            "input_digest": input_digest, "clean_input_digest": input_digest, "controls": clean_count}));
        receipts.push(json!({"op": index, "operation": operation.key(), "fault": event.fault.key(),
            "fire_count": 1, "site_fires": fires, "site_controls": clean_count, "integrated": true}));
        for mut check in operation_checks {
            let invariant = check["invariant"]
                .as_str()
                .ok_or("missing comparator invariant")?
                .to_owned();
            *counts.entry(invariant.clone()).or_default() += 1;
            coverage.hit(format!("invariant.{invariant}.checked"));
            check["operation"] = json!(operation.key());
            check["input_digest"] = json!(input_digest);
            check["passed"] = json!(true);
            check["first_difference"] = Value::Null;
            check["op"] = json!(index);
            checks.push(check);
        }
    }
    let oracle_bytes = lines(&checks)?;
    let controls_bytes =
        artifacts.write_controls(&controls.iter().map(Value::to_string).collect::<Vec<_>>())?;
    let receipts_bytes =
        artifacts.write_receipts(&receipts.iter().map(Value::to_string).collect::<Vec<_>>())?;
    let mutations_bytes = artifacts.write_mutations(&[])?;
    std::fs::write(artifacts.directory().join("oracle.jsonl"), &oracle_bytes)
        .map_err(|e| e.to_string())?;
    let faults_bytes = artifacts.write_fault_plan(&plan.schedule.events, &plan.feature)?;
    let violations_bytes = artifacts.write_violations_for(campaign, &[])?;
    let coverage_bytes = artifacts.write_coverage(&coverage)?;
    let mut family = BTreeMap::<String, Vec<u8>>::new();
    for (name, bytes) in [
        ("graph-fixture.json", fixture_bytes),
        ("graph-observations.jsonl", lines(&observations)?),
        ("graph-durable-images.jsonl", lines(&durable)?),
    ] {
        family.insert(
            name.into(),
            artifacts.write_family_artifact(campaign, name, &bytes)?,
        );
    }
    let comparison_outcome_counts = counts
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                ComparisonOutcomeCounts {
                    equal: *value,
                    refused: 0,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let operation_evidence = [&program_bytes[..], &controls_bytes[..]]
        .into_iter()
        .chain(family.values().map(Vec::as_slice))
        .collect::<Vec<_>>();
    let fault_evidence = [&faults_bytes[..], &receipts_bytes[..], &mutations_bytes[..]]
        .into_iter()
        .chain(family.values().map(Vec::as_slice))
        .collect::<Vec<_>>();
    let mut evidence_digests = BTreeMap::from([
        (
            "operation_evidence".into(),
            evidence_digest(&operation_evidence),
        ),
        ("checker_evidence".into(), evidence_digest(&[&oracle_bytes])),
        ("fault_evidence".into(), evidence_digest(&fault_evidence)),
    ]);
    for (name, bytes) in family
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .chain([
            ("faults.jsonl", faults_bytes.as_slice()),
            ("program.jsonl", program_bytes.as_slice()),
            ("controls.jsonl", controls_bytes.as_slice()),
            ("receipts.jsonl", receipts_bytes.as_slice()),
            ("oracle.jsonl", oracle_bytes.as_slice()),
        ])
    {
        evidence_digests.insert(name.to_owned(), evidence_digest(&[bytes]));
    }
    let attestation = EpisodeAttestation {
        comparison_counts: counts.clone(),
        comparison_outcome_counts: comparison_outcome_counts.clone(),
        same_seed_clean_controls: controls.len() as u64,
        integrated_feature_fault_receipts: receipts.len() as u64,
        expected_feature_fault_receipts: plan.feature.len() as u64,
        evidence_digests,
        family_oracle_attestation: None,
    };
    let reproduction = format!(
        "scripts/adversarial.sh episode --campaign property-graph --seed {seed} --profile none"
    );
    artifacts.write_reproduction(&reproduction)?;
    let episode_bytes = artifacts.write_episode_metadata(
        campaign,
        seed,
        profile,
        overridden,
        &reproduction,
        Some(&attestation),
    )?;
    Ok(RunOutcome {
        campaign,
        seed,
        profile,
        profile_overridden: overridden,
        operations: program.ops.len(),
        faults_fired: plan.feature.len(),
        scheduled_faults_fired: 0,
        feature_faults_scheduled: plan.feature.len(),
        feature_faults_fired: plan.feature.len(),
        missing_feature_faults: plan.missing_feature_faults(),
        graph_searches: 0,
        filtered_searches: 0,
        filtered_graph_searches: 0,
        predicate_searches: 0,
        hybrid_searches: 0,
        text_documents_ingested: 0,
        store_lexical_searches: 0,
        store_hybrid_searches: 0,
        phrase_searches: 0,
        prefix_searches: 0,
        fuzzy_searches: 0,
        phonetic_encodes: 0,
        snippets_built: 0,
        hybrid_sealed_vector_documents: 0,
        hybrid_lexical_documents: 0,
        epoch_preparations: 0,
        epoch_alias_switches: 0,
        epoch_rollbacks: 0,
        epoch_drops: 0,
        rejected_dropped_epoch_rollbacks: 0,
        coverage,
        violations: vec![],
        program_bytes,
        faults_bytes,
        violations_bytes,
        coverage_bytes,
        oracle_bytes,
        controls_bytes,
        receipts_bytes,
        mutations_bytes,
        episode_bytes,
        family_artifact_bytes: family,
        comparison_pass_counts: counts.clone(),
        comparison_counts: counts,
        comparison_outcome_counts,
        same_seed_clean_controls: controls.len() as u64,
        integrated_feature_fault_receipts: receipts.len() as u64,
        expected_feature_fault_receipts: plan.feature.len() as u64,
    })
}

fn measured_counts(value: &Value) -> Result<(u64, u64), String> {
    if let Some(rows) = value.as_array() {
        return rows.iter().try_fold((0, 0), |(f, c), row| {
            measured_counts(row).map(|(a, b)| (f + a, c + b))
        });
    }
    if let Some(rows) = value.get("receipts") {
        return measured_counts(rows);
    }
    if let (Some(f), Some(c)) = (value["fires"].as_u64(), value["controls"].as_u64()) {
        return Ok((f, c));
    }
    if let (Some(fault), Some(clean)) = (value.get("fault"), value.get("clean")) {
        let (f, _) = measured_counts(fault)?;
        let (_, c) = measured_counts(clean)?;
        return Ok((f, c));
    }
    Err("executor omitted measured receipt".into())
}

pub fn validate_retained(directory: &Path, attestation: &Value) -> Result<(), String> {
    for name in [
        "graph-fixture.json",
        "graph-observations.jsonl",
        "graph-durable-images.jsonl",
        "faults.jsonl",
        "program.jsonl",
        "controls.jsonl",
        "receipts.jsonl",
        "oracle.jsonl",
    ] {
        let bytes = std::fs::read(directory.join(name)).map_err(|e| format!("{name}: {e}"))?;
        if attestation["evidence_digests"][name].as_str()
            != Some(evidence_digest(&[&bytes]).as_str())
        {
            return Err(format!("{name}: retained evidence digest mismatch"));
        }
    }
    Ok(())
}
/// The memory VFS cannot map a native graph. Qualify its byte/barrier model using
/// the real executor's WAL bytes, separately from directory loss and SIGKILL.
fn fault_vfs_observation(fixture: &Fixture) -> Result<Value, String> {
    use zeppelin_embed::vfs::{SyncKind, Vfs};
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = zeppelin_embed::graph_commit_recovery_test_support::ProbeStore::create(
        &root.path().join("native"),
        fixture,
        std::sync::Arc::new(zeppelin_embed::vfs::StdVfs),
    );
    store.apply(fixture).map_err(|e| e.to_string())?;
    super::graph_recovery::compare_batch(fixture, &store.observe().ok_or("missing WAL control")?)?;
    if store.release() != 0 {
        return Err("FaultVfs control retained mappings".into());
    }
    let entry = std::fs::read_dir(root.path().join("native"))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|e| e.file_name().to_string_lossy().starts_with("graph-wal-"))
        .ok_or("native WAL missing")?;
    let bytes = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
    let vfs = zeppelin_embed::vfs::fault::FaultVfs::new();
    let path = Path::new("graph-wal-control.ze");
    let mut file = vfs.open_append(path).map_err(|e| e.to_string())?;
    file.append(&bytes).map_err(|e| e.to_string())?;
    file.sync(SyncKind::Barrier).map_err(|e| e.to_string())?;
    let visible = vfs
        .application_crash()
        .map_err(|e| e.to_string())?
        .read(path)
        .map_err(|e| e.to_string())?;
    let absent = vfs
        .power_cut()
        .map_err(|e| e.to_string())?
        .read(path)
        .err()
        .ok_or("barrier incorrectly persisted WAL")?;
    if visible != bytes || absent.kind() != std::io::ErrorKind::NotFound {
        return Err("FaultVfs barrier model differs".into());
    }
    file.sync(SyncKind::Full).map_err(|e| e.to_string())?;
    let media = vfs
        .power_cut()
        .map_err(|e| e.to_string())?
        .read(path)
        .map_err(|e| e.to_string())?;
    if media != bytes {
        return Err("FaultVfs full-sync lost bytes".into());
    }
    Ok(
        json!({"model": "FaultVfs-visible-media-barrier-full-sync", "scope": "real native WAL bytes; no file-backed mappings or directory model",
        "fires": 1, "controls": 1, "barrier_power_error": format!("{:?}", absent.kind()),
        "visible_bytes": visible, "media_bytes": media, "digest": evidence_digest(&[&bytes])}),
    )
}

pub fn check_reclaim(
    reports: &[zeppelin_embed::graph_commit_recovery_test_support::ReclaimEvidence],
) -> Result<(), String> {
    let clean = reports
        .iter()
        .find(|r| r.cell == "Control")
        .ok_or("missing reclaim control")?;
    if reports.len() != 7 || clean.controls != 1 || clean.fires != 0 {
        return Err("invalid reclaim control receipts".into());
    }
    for r in reports {
        if r.pending_proof != r.replayed_proof || r.inventory_pending != r.inventory_replayed {
            return Err(format!(
                "protected-artifacts: {} checkpoint changed pending proof/inventory",
                r.cell
            ));
        }
        if r.provenance_phases.len() != 4
            || r.provenance_phases
                .iter()
                .any(|p| Some(p) != r.provenance_phases.first())
        {
            return Err(format!(
                "identity-history: {} provenance fields changed",
                r.cell
            ));
        }
        if r.input_image != clean.input_image {
            return Err(format!("{} reclaim control input bytes differ", r.cell));
        }
        if r.fires != u64::from(r.cell != "Control") || r.controls != u64::from(r.cell == "Control")
        {
            return Err(format!("{} reclaim boundary did not fire", r.cell));
        }
        if !r.retry_receipts.contains("replayed: true") {
            return Err("identity-history: reclaim retry was not replay".into());
        }
    }
    Ok(())
}
