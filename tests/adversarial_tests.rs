#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
#![recursion_limit = "256"]

mod adversarial;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs::File, io::Write as _};

use adversarial::campaign::{
    CampaignKind, CampaignSpec, FaultPlan, InvariantId, Qualification, RunConfig,
};
use adversarial::coverage::{CoverageRegistry, REQUIRED_SMOKE_COVERAGE};
use adversarial::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, LAST_MATCH, Layer, ScheduledVfs, plan_schedule,
};
use adversarial::profiles::{Environment, FaultProfile, environment_for_profile, profile_for_seed};
use adversarial::program::{CrashBoundary, Op, PredicateKind, Program};
use adversarial::runner::{Invariant, SelfTestBug};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::build::GraphBuildError;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{
    ManualMonotonicClock, OpenOptions, Store, StoreError, StoreTestDependencies,
};
use zeppelin_embed::meta::Schema;
use zeppelin_embed::segment::SegmentError;
use zeppelin_embed::tier::{
    MaintenanceBudget, MaintenanceError, MaintenanceStatus, TierThresholds,
};
use zeppelin_embed::vfs::{StdVfs, Vfs};

#[test]
fn no_family_sets_clean_control_passed_as_a_literal() {
    let source_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("adversarial");
    let helper_only_families = [
        "tiering_maintenance.rs",
        "fts.rs",
        "vamana_graph.rs",
        "lifecycle_accounting.rs",
        "diagnostics_health.rs",
        "ffi_bindings.rs",
    ];
    let mut violations = Vec::new();
    let mut paths = std::fs::read_dir(&source_directory)
        .expect("read adversarial sources")
        .map(|entry| entry.expect("read adversarial source entry").path())
        .collect::<Vec<_>>();
    paths.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("adversarial_tests.rs"));
    let forbidden = [
        ["clean_control_passed", ":", "true"].concat(),
        ["clean_control_passed", "=", "true"].concat(),
        ["clean_control_passed", ":", "!false"].concat(),
        ["clean_control_passed", "=", "!false"].concat(),
    ];
    for path in paths {
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read adversarial source");
        let compact = source.split_whitespace().collect::<String>();
        let is_helper_only_family = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| helper_only_families.contains(&name));
        if forbidden.iter().any(|literal| compact.contains(literal))
            || (is_helper_only_family && compact.contains("clean_control_passed"))
        {
            violations.push(path.display().to_string());
        }
    }
    assert!(
        violations.is_empty(),
        "family clean controls must be measured, not literals: {violations:?}"
    );
}

#[test]
fn clean_control_helper_runs_the_clean_leg_without_any_scheduled_event() {
    let source = tempfile::tempdir().expect("clean-control source fixture");
    let store = Store::open(source.path(), OpenOptions::default()).expect("open source fixture");
    store.close().expect("close source fixture");
    let event = FaultEvent {
        id: "clean-control-must-not-see-schedule".to_owned(),
        op_index: 17,
        site: FaultSite::Append,
        layer: Layer::Io,
        mode: FaultMode::Eio,
        nth_match: 1,
        expected_matches: None,
        path_contains: Some("wal.ze".to_owned()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let fixture = adversarial::runner::FrozenStoreFixture::capture(source.path())
        .expect("capture source fixture")
        .with_fault(event, 17);
    let outcome = adversarial::runner::run_with_clean_control(
        &fixture,
        |store| {
            let document = zeppelin_embed::ingest::IngestDocument::new(
                zeppelin_embed::ingest::DocumentVersion::new(
                    zeppelin_embed::ingest::DocId::new(1),
                    zeppelin_embed::ingest::Revision::new(1),
                ),
                vec![1.0],
            );
            store
                .store()?
                .ingest(zeppelin_embed::ingest::IngestBatch::new(vec![document]))
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        |store| {
            let document = zeppelin_embed::ingest::IngestDocument::new(
                zeppelin_embed::ingest::DocumentVersion::new(
                    zeppelin_embed::ingest::DocId::new(1),
                    zeppelin_embed::ingest::Revision::new(1),
                ),
                vec![1.0],
            );
            store
                .store()?
                .ingest(zeppelin_embed::ingest::IngestBatch::new(vec![document]))
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        |_| Ok(()),
    )
    .expect("materialize clean-control pair");

    assert!(outcome.clean.is_ok(), "plain clean VFS saw the fault");
    assert!(
        format!("{:?}", outcome.faulted).contains("Refused"),
        "faulted VFS did not refuse: {:?}",
        outcome.faulted
    );
    assert!(
        outcome.fault_event.is_some_and(|event| event.fired),
        "faulted schedule did not fire"
    );
}

#[test]
fn clean_control_helper_reports_false_when_the_clean_leg_disagrees_with_the_oracle() {
    let source = tempfile::tempdir().expect("clean-control source fixture");
    let store = Store::open(source.path(), OpenOptions::default()).expect("open source fixture");
    store.close().expect("close source fixture");
    let fixture = adversarial::runner::FrozenStoreFixture::capture(source.path())
        .expect("capture source fixture");
    let outcome = adversarial::runner::run_with_clean_control(
        &fixture,
        |_store| Ok(()),
        |_store| Ok(()),
        |_| Err("family oracle disagreed".to_owned()),
    )
    .expect("materialize clean-control pair");

    assert_eq!(outcome.clean, Ok(()));
    assert!(
        !outcome.same_seed_control_passed,
        "oracle disagreement was counted as a passing clean control"
    );
}

#[test]
fn clean_control_helper_records_faulted_open_as_a_typed_refusal() {
    let source = tempfile::tempdir().expect("clean-control source fixture");
    let store = Store::open(source.path(), OpenOptions::default()).expect("open source fixture");
    store.close().expect("close source fixture");
    let event = FaultEvent {
        id: "faulted-open-refusal".to_owned(),
        op_index: 23,
        site: FaultSite::Open,
        layer: Layer::Io,
        mode: FaultMode::Eio,
        nth_match: 1,
        expected_matches: None,
        path_contains: None,
        fired: false,
        fire_count: 0,
        path: None,
    };
    let fixture = adversarial::runner::FrozenStoreFixture::capture(source.path())
        .expect("capture source fixture")
        .with_fault(event, 23);
    let faulted_operation_ran = Arc::new(AtomicBool::new(false));
    let faulted_marker = Arc::clone(&faulted_operation_ran);
    let outcome = adversarial::runner::run_with_clean_control(
        &fixture,
        |_store| Ok(()),
        move |_store| {
            faulted_marker.store(true, Ordering::Relaxed);
            Ok(())
        },
        |_| Ok(()),
    )
    .expect("materialize clean-control pair");

    assert!(outcome.clean.is_ok(), "clean leg did not run");
    assert!(
        format!("{:?}", outcome.faulted).contains("Refused"),
        "scheduled Store::open fault was not recorded as a typed refusal: {:?}",
        outcome.faulted
    );
    assert!(
        !faulted_operation_ran.load(Ordering::Relaxed),
        "faulted operation ran after Store::open refusal"
    );
}

#[test]
fn tiering_control_goes_false_under_a_planted_product_mutation() {
    let passed = adversarial::runner::tier_clean_control_for_test(
        adversarial::tiering_maintenance::TierOperationKind::Policy,
        2,
    )
    .expect("tiering clean control");
    assert!(
        passed,
        "tiering clean control disagreed with its independent oracle"
    );
}

#[test]
fn fts_control_goes_false_under_a_planted_product_mutation() {
    let passed = adversarial::runner::fts_clean_control_for_test(
        adversarial::fts::FtsOperationKind::Bm25,
        1,
    )
    .expect("FTS clean control");
    assert!(
        passed,
        "FTS clean control disagreed with its independent oracle"
    );
}

#[test]
fn vamana_control_goes_false_under_a_planted_product_mutation() {
    let passed = adversarial::runner::graph_clean_control_for_test(
        adversarial::vamana_graph::GraphOperationKind::FilteredSearch,
        7,
    )
    .expect("Vamana clean control");
    assert!(
        passed,
        "Vamana clean control disagreed with its independent oracle"
    );
}

#[test]
fn runner_counts_qualifying_controls_from_the_helper_for_every_family() {
    for campaign in CampaignKind::FEATURES {
        let seed = (0..128)
            .find(|&seed| {
                let program = Program::generate_for(campaign, seed);
                let plan = FaultPlan::for_program(
                    campaign,
                    seed,
                    FaultProfile::Full,
                    &program,
                    FaultSchedule::default(),
                );
                !plan.feature.is_empty()
            })
            .unwrap_or_else(|| panic!("{campaign} has no selected feature-fault seed"));
        let output = std::process::Command::new(
            std::env::current_exe().expect("helper accounting test binary"),
        )
        .args([
            "runner_helper_accounting_child",
            "--ignored",
            "--exact",
            "--nocapture",
        ])
        .env("ZE_ADV_HELPER_CAMPAIGN", campaign.key())
        .env("ZE_ADV_HELPER_SEED", seed.to_string())
        .output()
        .expect("run helper accounting child");
        assert!(
            output.status.success(),
            "{campaign} helper accounting child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("HELPER_ACCOUNTING_CHILD_RAN"),
            "{campaign} helper accounting child did not run: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "isolated child for helper accounting across process-global family tests"]
fn runner_helper_accounting_child() {
    let campaign_key = std::env::var("ZE_ADV_HELPER_CAMPAIGN").expect("helper campaign");
    let campaign = CampaignKind::FEATURES
        .into_iter()
        .find(|campaign| campaign.key() == campaign_key)
        .expect("known helper campaign");
    let seed = std::env::var("ZE_ADV_HELPER_SEED")
        .expect("helper seed")
        .parse::<u64>()
        .expect("numeric helper seed");
    let artifacts = tempfile::tempdir().expect("helper accounting artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::Full, artifacts.path())
            .unwrap_or_else(|error| panic!("{campaign} seed {seed}: {error}"));
    let episode = artifacts
        .path()
        .join(campaign.key())
        .join(format!("seed-{seed}-full"));
    let operation_by_index = std::fs::read_to_string(episode.join("program.jsonl"))
        .expect("read helper program records")
        .lines()
        .filter_map(|line| {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(line).expect("parse helper program");
            Some((
                record["op"].as_u64()?,
                record["operation"].as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let selected_operations = std::fs::read_to_string(episode.join("faults.jsonl"))
        .expect("read helper fault records")
        .lines()
        .filter_map(|line| {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(line).expect("parse helper fault");
            (record["type"].as_str() == Some("feature") && record["fired"].as_bool() == Some(true))
                .then(|| record["op"].as_u64())
                .flatten()
        })
        .filter_map(|index| operation_by_index.get(&index).cloned())
        .collect::<BTreeSet<_>>();
    let controls = std::fs::read_to_string(episode.join("controls.jsonl"))
        .expect("read helper control records")
        .lines()
        .filter(|line| {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(line).expect("parse helper control");
            if campaign == CampaignKind::MetadataFilterPlanner {
                !record["fault"].is_null()
            } else {
                record["operation"]
                    .as_str()
                    .is_some_and(|operation| selected_operations.contains(operation))
            }
        })
        .count();
    assert!(
        controls > 0,
        "{campaign} produced 0 qualifying same-seed controls"
    );
    assert_eq!(
        usize::try_from(outcome.same_seed_clean_controls).expect("control count fits usize"),
        controls,
        "{campaign} did not count the control records written to disk"
    );
    println!("HELPER_ACCOUNTING_CHILD_RAN controls={controls}");
}

#[test]
fn scheduled_open_fault_reaches_a_sealed_segment_open() {
    let directory = tempfile::tempdir().expect("scheduled reopen directory");
    let scheduled = Arc::new(ScheduledVfs::new(
        StdVfs,
        FaultSchedule::single(FaultEvent {
            id: "stage-06-open-eio".to_owned(),
            op_index: 1,
            layer: Layer::Io,
            site: FaultSite::Open,
            mode: FaultMode::Eio,
            nth_match: 2,
            expected_matches: None,
            path_contains: Some(".zseg".to_owned()),
            fired: false,
            fire_count: 0,
            path: None,
        }),
    ));
    let clock = Arc::new(ManualMonotonicClock::new());
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        StoreTestDependencies::new(scheduled.clone(), clock.clone()),
    )
    .expect("open scheduled store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(6), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest sealed fixture");
    store.seal().expect("seal fixture");
    store.close().expect("close before scheduled reopen");

    scheduled.set_operation(1);
    let reopened = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        StoreTestDependencies::new(scheduled.clone(), clock),
    );
    assert!(
        matches!(
            reopened,
            Err(StoreError::Segment(SegmentError::Io { source, .. }))
                if source.raw_os_error() == Some(5)
        ),
        "sealed reopen did not preserve the typed Open/Eio error"
    );
    assert!(scheduled.events().into_iter().any(|event| event.fired));
}

#[test]
fn torn_graph_checkpoint_then_reopen_rebuilds_or_refuses() {
    let program = Program::generate_for(CampaignKind::VamanaGraph, 6);
    let maintain = program
        .ops
        .iter()
        .position(|operation| matches!(operation, Op::Maintain { .. }))
        .expect("Vamana graph program contains Maintain");
    let event = FaultEvent {
        id: "stage-06-torn-checkpoint".to_owned(),
        op_index: maintain,
        site: FaultSite::Write,
        layer: Layer::Content,
        mode: FaultMode::TornWrite,
        nth_match: 1,
        expected_matches: None,
        path_contains: Some(".graph.checkpoint.tmp".to_owned()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let scheduled = Arc::new(ScheduledVfs::new(StdVfs, FaultSchedule::single(event)));
    let directory = tempfile::tempdir().expect("torn checkpoint directory");
    let document = EmbeddingTower {
        model_id: "stage-06-sift-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x06],
        dims: 128,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    };
    let clock = Arc::new(ManualMonotonicClock::new());
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
        StoreTestDependencies::new(scheduled.clone(), clock.clone()),
    )
    .expect("open scheduled maintenance store");
    let documents = (0..128)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                (0..128)
                    .map(|dimension| {
                        if dimension % 2 == 0 {
                            row as f32
                        } else {
                            -(row as f32)
                        }
                    })
                    .collect(),
            )
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest checkpoint fixture");
    store.seal().expect("seal checkpoint fixture");

    scheduled.set_operation(maintain);
    let interrupted = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(600),
            bytes: 256 * 64,
        },
        TierThresholds { graph_min_rows: 1 },
    );
    assert!(matches!(
        interrupted.status,
        MaintenanceStatus::BudgetExhausted
    ));
    assert!(scheduled.events().into_iter().any(|event| event.fired));
    store.close().expect("close after torn checkpoint");

    let reopened = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_epoch(epoch),
        StoreTestDependencies::new(scheduled, clock),
    )
    .expect("reopen after torn checkpoint");
    let refused = reopened.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(600),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 1 },
    );
    assert!(
        matches!(
            &refused.status,
            MaintenanceStatus::Failed(MaintenanceError::Graph(GraphBuildError::CheckpointCorrupt(
                _
            )))
        ),
        "reopen accepted a torn graph-build checkpoint: {:?}",
        refused.status
    );
    reopened.close().expect("close refused checkpoint store");
}

#[test]
fn profile_for_seed_is_total_and_covers_every_preset_in_eight_seeds() {
    let observed = (0..8).map(profile_for_seed).collect::<Vec<_>>();
    assert_eq!(
        observed,
        vec![
            FaultProfile::None,
            FaultProfile::IoErrors,
            FaultProfile::Content,
            FaultProfile::Crash,
            FaultProfile::Disk,
            FaultProfile::Clock,
            FaultProfile::Full,
            FaultProfile::Random,
        ]
    );
}

#[test]
fn environment_for_random_profile_is_a_pure_function_of_seed() {
    let first = environment_for_profile(FaultProfile::Random, 91);
    let second = environment_for_profile(FaultProfile::Random, 91);
    let different_seed = environment_for_profile(FaultProfile::Random, 92);
    assert_eq!(first, second);
    assert_ne!(first, different_seed, "Random ignored the seed");
    assert_ne!(first, Environment::default());
    for rate in [
        first.io,
        first.content,
        first.crash,
        first.clock,
        first.cancel,
        first.busy,
    ] {
        assert!(rate <= 96, "random rate {rate} exceeds 96/256");
    }
}

#[test]
fn schedule_is_deterministic_for_seed_environment_and_program() {
    let program = Program::generate(7);
    let environment = environment_for_profile(FaultProfile::Full, 7);
    let first = plan_schedule(7, environment, &program);
    let second = plan_schedule(7, environment, &program);
    assert!(
        !first.events.is_empty(),
        "full environment planned no faults"
    );
    assert_eq!(first.events, second.events);
}

#[test]
fn schedule_places_events_on_random_ops_not_only_the_first() {
    let op_indices = (0..200)
        .flat_map(|seed| {
            let program = Program::generate(seed);
            plan_schedule(
                seed,
                environment_for_profile(FaultProfile::IoErrors, seed),
                &program,
            )
            .events
        })
        .filter(|event| event.site == FaultSite::Append && event.mode == FaultMode::Eio)
        .map(|event| event.op_index)
        .collect::<BTreeSet<_>>();
    assert!(
        op_indices.len() >= 3,
        "Append/Eio appeared at only {op_indices:?}"
    );
}

#[test]
fn schedule_never_targets_a_site_the_op_cannot_reach() {
    assert!(
        !stage_01_site_is_reachable(
            &Op::Ingest {
                first_id: 1,
                count: 1,
                revision: 1,
                timestamp: 0,
            },
            Layer::Content,
            FaultSite::Append,
        ),
        "Content/Ingest cannot reach Append"
    );
    assert!(
        !stage_01_site_is_reachable(&Op::Seal, Layer::Content, FaultSite::List),
        "Content/Seal cannot reach List"
    );
    assert!(
        !stage_01_site_is_reachable(
            &Op::DropPartition { start: 0, end: 1 },
            Layer::Content,
            FaultSite::Delete,
        ),
        "Content/DropPartition cannot reach Delete"
    );
    for seed in 0..200 {
        let program = Program::generate(seed);
        for event in plan_schedule(
            seed,
            environment_for_profile(FaultProfile::Full, seed),
            &program,
        )
        .events
        {
            assert!(
                stage_01_site_is_reachable(&program.ops[event.op_index], event.layer, event.site),
                "seed {seed} planned {:?}/{:?} for {}",
                event.layer,
                event.site,
                program.ops[event.op_index].kind()
            );
        }
    }
}

#[test]
fn schedule_uses_drawn_nth_match_under_each_stage_01_fault_preset() {
    let observed = [
        FaultProfile::IoErrors,
        FaultProfile::Content,
        FaultProfile::Disk,
        FaultProfile::Full,
        FaultProfile::Random,
    ]
    .into_iter()
    .any(|profile| {
        (0..200).any(|seed| {
            let program = Program::generate(seed);
            plan_schedule(seed, environment_for_profile(profile, seed), &program)
                .events
                .iter()
                .any(|event| (2..=4).contains(&event.nth_match))
        })
    });
    assert!(observed, "no preset planned a drawn nth_match in 2..=4");
}

#[test]
fn planned_content_writes_are_not_restricted_to_the_manifest() {
    let events = (0..200)
        .flat_map(|seed| {
            let program = Program::generate(seed);
            plan_schedule(
                seed,
                environment_for_profile(FaultProfile::Content, seed),
                &program,
            )
            .events
        })
        .filter(|event| event.layer == Layer::Content && event.site == FaultSite::Write)
        .collect::<Vec<_>>();
    assert!(!events.is_empty(), "Content preset planned no Write event");
    assert!(
        events.iter().all(|event| event.path_contains.is_none()),
        "Content/Write remained restricted to manifest.ze"
    );
}

#[test]
fn planned_content_write_can_target_the_segment_temp_file() {
    let event = (0..200)
        .find_map(|seed| {
            let program = Program::generate(seed);
            plan_schedule(
                seed,
                environment_for_profile(FaultProfile::Content, seed),
                &program,
            )
            .events
            .into_iter()
            .find(|event| {
                event.layer == Layer::Content
                    && event.site == FaultSite::Write
                    && event.nth_match == 1
            })
        })
        .expect("Content preset never planned the first Write match");
    let directory = tempfile::tempdir().expect("planned content Write directory");
    let segment = directory.path().join(".segment-0001.zseg.tmp");
    let manifest = directory.path().join(".manifest.ze.tmp");
    let op_index = event.op_index;
    let scheduled = ScheduledVfs::new(StdVfs, FaultSchedule::single(event));
    scheduled.set_operation(op_index);

    assert!(scheduled.write(&segment, b"segment payload").is_ok());
    assert!(scheduled.write(&manifest, b"manifest payload").is_ok());
    assert_eq!(
        scheduled.events()[0]
            .path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some(".segment-0001.zseg.tmp")
    );
}

fn stage_01_site_is_reachable(operation: &Op, layer: Layer, site: FaultSite) -> bool {
    if matches!(layer, Layer::Io | Layer::Content)
        && let Op::Crash { boundary, .. } = operation
    {
        return match boundary {
            CrashBoundary::MidWalGroup => site == FaultSite::Append,
            CrashBoundary::MidSeal => site == FaultSite::Write,
            CrashBoundary::PreManifestRename | CrashBoundary::PostManifestRename => {
                site == FaultSite::Rename
            }
            CrashBoundary::MidPurge => site == FaultSite::Delete,
        };
    }
    if layer == Layer::Crash {
        return match operation {
            Op::Ingest { .. }
            | Op::Upsert { .. }
            | Op::Revise { .. }
            | Op::Delete { .. }
            | Op::Purge { .. } => matches!(site, FaultSite::Append | FaultSite::Sync),
            Op::Seal | Op::Maintain { .. } => matches!(
                site,
                FaultSite::Write | FaultSite::Sync | FaultSite::Rename | FaultSite::Delete
            ),
            Op::DropPartition { .. } => site == FaultSite::Delete,
            _ => false,
        };
    }
    if !matches!(layer, Layer::Io | Layer::Content) {
        return false;
    }
    match operation {
        Op::Ingest { .. }
        | Op::Upsert { .. }
        | Op::Revise { .. }
        | Op::Delete { .. }
        | Op::Purge { .. } => {
            layer == Layer::Io && matches!(site, FaultSite::Append | FaultSite::Sync)
        }
        Op::Seal | Op::Maintain { .. } if layer == Layer::Content => site == FaultSite::Write,
        Op::Seal | Op::Maintain { .. } => matches!(
            site,
            FaultSite::Write
                | FaultSite::Rename
                | FaultSite::Sync
                | FaultSite::List
                | FaultSite::Delete
        ),
        Op::Open | Op::Reopen if layer == Layer::Io => matches!(
            site,
            FaultSite::Open | FaultSite::Read | FaultSite::ReadRange | FaultSite::List
        ),
        Op::Open | Op::Reopen => site == FaultSite::Read,
        Op::Search { .. }
        | Op::FilteredSearch { .. }
        | Op::PredicateSearch { .. }
        | Op::HybridSearch { .. }
        | Op::DeadlineProbe { .. }
            if layer == Layer::Io =>
        {
            matches!(
                site,
                FaultSite::Read | FaultSite::ReadRange | FaultSite::Open
            )
        }
        Op::Search { .. }
        | Op::FilteredSearch { .. }
        | Op::PredicateSearch { .. }
        | Op::HybridSearch { .. }
        | Op::DeadlineProbe { .. } => matches!(site, FaultSite::Read | FaultSite::ReadRange),
        Op::DropPartition { .. } => {
            layer == Layer::Io && matches!(site, FaultSite::Delete | FaultSite::List)
        }
        _ => false,
    }
}

#[test]
fn scheduled_vfs_fires_the_nth_match_not_the_first() {
    let directory = tempfile::tempdir().expect("nth-match ScheduledVfs directory");
    let path = directory.path().join("nth-match");
    let scheduled = ScheduledVfs::new(
        StdVfs,
        FaultSchedule::single(FaultEvent {
            id: "nth-match".to_owned(),
            op_index: 4,
            layer: Layer::Io,
            site: FaultSite::Write,
            mode: FaultMode::Eio,
            nth_match: 2,
            expected_matches: None,
            path_contains: None,
            fired: false,
            fire_count: 0,
            path: None,
        }),
    );
    scheduled.set_operation(4);

    assert!(scheduled.write(&path, b"first").is_ok());
    assert!(scheduled.write(&path, b"second").is_err());
    assert_eq!(
        scheduled
            .events()
            .into_iter()
            .next()
            .expect("scheduled event")
            .fire_count,
        1
    );
}

#[test]
fn scheduled_vfs_fires_the_last_match_not_the_first() {
    for write_count in 2..=4 {
        let directory = tempfile::tempdir().expect("last-match ScheduledVfs directory");
        let path = directory.path().join(format!("last-match-{write_count}"));
        let scheduled = ScheduledVfs::new(
            StdVfs,
            FaultSchedule::single(FaultEvent {
                id: format!("last-match-{write_count}"),
                op_index: 4,
                layer: Layer::Content,
                site: FaultSite::Write,
                mode: FaultMode::BitFlip,
                nth_match: LAST_MATCH,
                expected_matches: Some(write_count),
                path_contains: None,
                fired: false,
                fire_count: 0,
                path: None,
            }),
        );
        scheduled.set_operation(4);

        for write_index in 1..=write_count {
            assert!(scheduled.write(&path, &[write_index as u8]).is_ok());
            let expected_fire_count = usize::from(write_index == write_count);
            assert_eq!(
                scheduled.events()[0].fire_count,
                expected_fire_count,
                "LAST_MATCH did not select write {write_count} of {write_count}"
            );
        }
    }
}

#[test]
fn unfired_event_does_not_starve_ready_event_at_same_op_and_site() {
    let directory = tempfile::tempdir().expect("same-site ScheduledVfs directory");
    let path = directory.path().join("same-site");
    let event = |id: &str, nth_match, mode| FaultEvent {
        id: id.to_owned(),
        op_index: 4,
        layer: Layer::Io,
        site: FaultSite::Write,
        mode,
        nth_match,
        expected_matches: None,
        path_contains: None,
        fired: false,
        fire_count: 0,
        path: None,
    };
    let scheduled = ScheduledVfs::new(
        StdVfs,
        FaultSchedule {
            events: vec![
                event("second-match", 2, FaultMode::Eio),
                event("first-match", 1, FaultMode::Eacces),
            ],
        },
    );
    scheduled.set_operation(4);

    assert!(scheduled.write(&path, b"first").is_err());
    let after_first = scheduled.events();
    assert_eq!(after_first[0].fire_count, 0);
    assert_eq!(
        after_first[1].fire_count, 1,
        "ready second event was starved by the first unfired event"
    );
    assert!(scheduled.write(&path, b"second").is_err());
    assert_eq!(scheduled.events()[0].fire_count, 1);
}

#[test]
fn faults_jsonl_tags_generic_events_with_type_and_layer() {
    let event = FaultEvent {
        id: "tagged".to_owned(),
        op_index: 2,
        layer: Layer::Content,
        site: FaultSite::Write,
        mode: FaultMode::BitFlip,
        nth_match: 1,
        expected_matches: None,
        path_contains: None,
        fired: false,
        fire_count: 0,
        path: None,
    };
    let record: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_str(&event.json_line())
            .expect("parse generic fault JSON");

    assert_eq!(record["type"].as_str(), Some("generic"));
    assert_eq!(record["layer"].as_str(), Some("content"));
}

#[test]
fn runner_retries_after_io_layer_and_not_after_content_layer_at_same_op() {
    let event = |id: &str, layer, mode| FaultEvent {
        id: id.to_owned(),
        op_index: 9,
        layer,
        site: FaultSite::Append,
        mode,
        nth_match: 1,
        expected_matches: None,
        path_contains: None,
        fired: true,
        fire_count: 1,
        path: None,
    };
    let io = event("io", Layer::Io, FaultMode::PostCommitError);
    let content = event("content", Layer::Content, FaultMode::BitFlip);

    assert!(adversarial::runner::runner_retries_faulted_operation(
        std::slice::from_ref(&io),
        9
    ));
    assert!(!adversarial::runner::runner_retries_faulted_operation(
        &[io, content],
        9
    ));
}

#[test]
fn runner_records_violation_after_content_fault_at_earlier_operation() {
    let content = FaultEvent {
        id: "content-op-3".to_owned(),
        op_index: 3,
        layer: Layer::Content,
        site: FaultSite::Write,
        mode: FaultMode::BitFlip,
        nth_match: 1,
        expected_matches: None,
        path_contains: None,
        fired: true,
        fire_count: 1,
        path: None,
    };

    assert!(!adversarial::runner::runner_records_operation_error(
        std::slice::from_ref(&content),
        3
    ));
    assert!(
        adversarial::runner::runner_records_operation_error(&[content], 10),
        "content fault at op 3 suppressed a real invariant violation at op 10"
    );
}

#[test]
fn every_existing_fault_mode_and_site_key_is_still_emitted_under_full() {
    let expected = REQUIRED_SMOKE_COVERAGE
        .iter()
        .copied()
        .filter(|key| key.starts_with("fault.site.") || key.starts_with("fault.mode."))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::from([
        "fault.site.clock".to_owned(),
        "fault.mode.latency".to_owned(),
    ]);
    for seed in 0..200 {
        let program = Program::generate(seed);
        for event in plan_schedule(
            seed,
            environment_for_profile(FaultProfile::Full, seed),
            &program,
        )
        .events
        {
            observed.insert(format!("fault.site.{}", event.site.key()));
            observed.insert(format!("fault.mode.{}", event.mode.key()));
        }
    }

    assert_eq!(observed, expected);
}

#[test]
fn run_reproduction_line_is_seed_only_when_profile_is_not_overridden() {
    let line = adversarial::runner::reproduction_for_profile_override(
        CampaignKind::Overall,
        17,
        FaultProfile::Full,
        false,
    );
    assert_eq!(
        line,
        "ZE_ADV_SEED=17 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests run -- --ignored --exact --nocapture"
    );
}

#[test]
fn campaign_registry_is_complete_unique_and_smoke_bounded() {
    let specs = CampaignSpec::catalog();
    let keys = specs
        .iter()
        .map(|spec| spec.kind.key())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), specs.len(), "duplicate campaign key");
    assert_eq!(
        keys,
        [
            "diagnostics-health",
            "ffi-bindings",
            "fts",
            "hybrid-fusion",
            "ingest-retention",
            "lifecycle-accounting",
            "metadata-filter-planner",
            "overall",
            "storage-durability",
            "tiering-maintenance",
            "vamana-graph",
            "vector-execution",
        ]
        .into_iter()
        .collect()
    );
    for spec in specs {
        assert!(
            !spec.all_required_coverage().is_empty(),
            "{}",
            spec.kind.key()
        );
        assert!(
            spec.feature_faults.len() < 12,
            "{} has no clean slot in its 12-seed smoke cycle",
            spec.kind.key()
        );
        for fault in spec.feature_faults {
            assert_eq!(fault.campaign(), spec.kind, "typed fault crossed campaigns");
            assert!(
                spec.required_operations.contains(&fault.operation().key()),
                "{} fault {} targets undeclared operation {}",
                spec.kind.key(),
                fault.key(),
                fault.operation().key()
            );
        }
        if spec.kind != CampaignKind::Overall {
            let bound_ids = spec
                .invariant_specs
                .iter()
                .map(|binding| binding.invariant)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                bound_ids,
                spec.owned_invariants.iter().copied().collect(),
                "{} invariant bindings drifted",
                spec.kind.key()
            );
            for operation in adversarial::campaign::feature_operations(spec.kind) {
                assert!(
                    spec.invariant_specs
                        .iter()
                        .any(|binding| binding.operation == *operation),
                    "{} operation {} has no exact checker binding",
                    spec.kind.key(),
                    operation.key()
                );
            }
        }
    }
}

#[test]
fn invariant_registry_assigns_active_ids_once_and_reserves_epoch_ranges() {
    let specs = CampaignSpec::catalog();
    let mut assignments = BTreeMap::<InvariantId, &str>::new();
    for spec in specs {
        for invariant in spec.owned_invariants {
            assert!(
                assignments.insert(*invariant, spec.kind.key()).is_none(),
                "duplicate assignment for {}",
                invariant.key()
            );
            assert!(
                !invariant.label().is_empty(),
                "{} has no label",
                invariant.key()
            );
        }
    }
    assert_eq!(assignments.len(), 64);
    assert_eq!(
        assignments.keys().next().copied(),
        Some(InvariantId::new(1))
    );
    assert_eq!(
        assignments.keys().next_back().copied(),
        Some(InvariantId::new(70))
    );
    let reserved = adversarial::campaign::RESERVED_INVARIANTS
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        reserved,
        [12_u8, 14, 59, 60, 61, 62, 71, 72, 73, 74]
            .into_iter()
            .map(InvariantId::new)
            .collect()
    );
    assert!(
        assignments
            .keys()
            .all(|invariant| !reserved.contains(invariant))
    );
}

#[test]
fn feature_campaign_registry_owns_exact_ranges_without_generic_credit() {
    let expected = [
        (CampaignKind::StorageDurability, 15_u8, 19_u8),
        (CampaignKind::IngestRetention, 20, 23),
        (CampaignKind::VectorExecution, 24, 27),
        (CampaignKind::VamanaGraph, 28, 35),
        (CampaignKind::MetadataFilterPlanner, 36, 39),
        (CampaignKind::Fts, 40, 44),
        (CampaignKind::HybridFusion, 45, 49),
        (CampaignKind::TieringMaintenance, 50, 53),
        (CampaignKind::LifecycleAccounting, 54, 58),
        (CampaignKind::DiagnosticsHealth, 63, 65),
        (CampaignKind::FfiBindings, 66, 70),
    ];

    for (campaign, first, last) in expected {
        let spec = CampaignSpec::for_kind(campaign);
        assert!(
            spec.reused_invariants.is_empty(),
            "{} can earn generic invariant credit: {:?}",
            campaign.key(),
            spec.reused_invariants
        );
        assert_eq!(
            spec.owned_invariants,
            (first..=last).map(InvariantId::new).collect::<Vec<_>>(),
            "{} owns the wrong invariant range",
            campaign.key()
        );
        assert_eq!(
            spec.required_invariants(),
            spec.owned_invariants,
            "{} qualification includes an invariant it does not own",
            campaign.key()
        );
    }
}

#[test]
fn feature_campaigns_refuse_generic_storage_credit() {
    feature_campaign_registry_owns_exact_ranges_without_generic_credit();
    let storage = CampaignSpec::for_kind(CampaignKind::StorageDurability);
    assert_eq!(
        storage.owned_invariants,
        (15_u8..=19_u8).map(InvariantId::new).collect::<Vec<_>>()
    );
    assert!(storage.required_coverage.iter().all(|key| {
        !key.starts_with("invariant.I1.")
            && !key.starts_with("invariant.I2.")
            && !key.starts_with("invariant.I3.")
    }));
}

#[test]
fn clean_ingest_batch_runs_i20_exact_checker() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            let profile = profile_for_seed(*seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                profile,
                &program,
                plan_schedule(*seed, environment_for_profile(profile, *seed), &program),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("clean ingest-retention artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("clean ingest-retention episode");

    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    assert!(
        oracle.contains("\"checker_id\":\"I20.batch-atomicity.v1\""),
        "I20 operation-specific observation missing: {oracle}"
    );
    assert!(outcome.coverage.count("invariant.I20.checked") > 0);
}

#[test]
fn i20_batch_atomicity_checker_rejects_one_visible_row_from_failed_batch() {
    let error =
        zeppelin_embed_adversarial_oracle::ingest_retention::i20_visible_subset_plant_error()
            .expect("valid I20 independent checker plant");
    assert!(
        error.contains("I20.batch-atomicity.v1 visible subset 1/2"),
        "{error}"
    );
}

#[test]
fn i21_seal_checker_rejects_a_missing_document() {
    let error =
        zeppelin_embed_adversarial_oracle::ingest_retention::i21_missing_document_plant_error()
            .expect("valid I21 independent checker plant");
    assert!(
        error.contains("I21.seal-multiset.v1 missing document 21@2"),
        "{error}"
    );
}

#[test]
fn clean_seal_runs_i21_exact_multiset_checker() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("clean I21 artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("clean I21 episode");

    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    assert!(
        oracle.contains("\"checker_id\":\"I21.seal-multiset.v1\""),
        "I21 operation-specific observation missing: {oracle}"
    );
    assert!(outcome.coverage.count("invariant.I21.checked") > 0);
}

#[test]
fn i22_retention_checker_rejects_inclusive_cutoff_drop() {
    let error =
        zeppelin_embed_adversarial_oracle::ingest_retention::i22_inclusive_cutoff_drop_plant_error(
        )
        .expect("valid I22 independent checker plant");
    assert!(
        error.contains("I22.retention-boundary.v1 cutoff row was dropped"),
        "{error}"
    );
}

#[test]
fn clean_retention_runs_i22_exact_boundary_checker() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("clean I22 artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("clean I22 episode");

    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    assert!(
        oracle.contains("\"checker_id\":\"I22.retention-boundary.v1\""),
        "I22 operation-specific observation missing: {oracle}"
    );
    assert!(outcome.coverage.count("invariant.I22.checked") > 0);
}

#[test]
fn i23_purge_checker_rejects_completed_sentinel_hit() {
    let error =
        zeppelin_embed_adversarial_oracle::ingest_retention::i23_completed_sentinel_hit_plant_error()
            .expect("valid I23 independent checker plant");
    assert!(
        error.contains("I23.purge-proof.v1 completed purge left sentinel in segment-0001.zseg@128"),
        "{error}"
    );
}

#[test]
fn clean_physical_purge_runs_i23_byte_and_reopen_checker() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("clean I23 artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("clean I23 episode");

    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    assert!(
        oracle.contains("\"checker_id\":\"I23.purge-proof.v1\""),
        "I23 operation-specific observation missing: {oracle}"
    );
    assert!(outcome.coverage.count("invariant.I23.checked") > 0);
}

#[test]
fn ingest_retention_i20_oracle_is_std_only() {
    let manifest = include_str!("adversarial-oracle/Cargo.toml");
    let source = include_str!("adversarial-oracle/src/ingest_retention.rs");
    assert!(
        !manifest.contains("zeppelin-embed =")
            && !manifest.contains("zeppelin_embed =")
            && !source.contains("use zeppelin_embed")
            && !source.contains("PrimitiveObservation")
            && !source.contains("campaign_search_facts")
            && !source.contains("CheckerKind"),
        "I20 independent oracle imports or names a forbidden production/generic seam"
    );
}

#[test]
fn ingest_post_ack_retry_can_fire_at_batch_commit() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "post-ack-retry")
        })
        .expect("ingest-retention schedule reaches post-ack-retry");
    let artifacts = tempfile::tempdir().expect("post-ack retry campaign artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("post-ack retry campaign episode");

    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert_eq!(outcome.same_seed_clean_controls, 1);
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let receipts = String::from_utf8(outcome.receipts_bytes).expect("receipt JSON is UTF-8");
    assert!(receipts.contains("\"fault\":\"post-ack-retry\""));
    assert!(receipts.contains("\"site\":\"ingest.replay.no-wal-append\""));
}

#[test]
fn ingest_partial_batch_append_can_fire_at_batch_commit() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..24)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "partial-batch-append")
        })
        .expect("ingest-retention schedule reaches partial-batch-append");
    let artifacts = tempfile::tempdir().expect("partial-batch campaign artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("partial-batch campaign episode");

    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert_eq!(outcome.same_seed_clean_controls, 1);
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let receipts = String::from_utf8(outcome.receipts_bytes).expect("receipt JSON is UTF-8");
    assert!(receipts.contains("\"fault\":\"partial-batch-append\""));
    assert!(receipts.contains("\"site\":\"ingest.commit-many.append-error\""));
    assert!(receipts.contains("\"io_kind\":\"other\""));
}

#[test]
fn ingest_seal_cancellation_can_fire_at_seal() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..24)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "seal-cancellation")
        })
        .expect("ingest-retention schedule reaches seal-cancellation");
    let artifacts = tempfile::tempdir().expect("seal-cancellation campaign artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("seal-cancellation campaign episode");

    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert_eq!(outcome.same_seed_clean_controls, 1);
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let receipts = String::from_utf8(outcome.receipts_bytes).expect("receipt JSON is UTF-8");
    assert!(receipts.contains("\"fault\":\"seal-cancellation\""));
    assert!(receipts.contains("\"site\":\"seal.after-segment-write.before-manifest-commit\""));
    assert!(receipts.contains("\"temporary_segment_removed\":true"));
}

#[test]
fn ingest_retention_clock_boundary_can_fire_at_retention() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..24)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "retention-clock-boundary")
        })
        .expect("ingest-retention schedule reaches retention-clock-boundary");
    let artifacts = tempfile::tempdir().expect("retention-clock campaign artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("retention-clock campaign episode");

    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert_eq!(outcome.same_seed_clean_controls, 1);
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let receipts = String::from_utf8(outcome.receipts_bytes).expect("receipt JSON is UTF-8");
    assert!(receipts.contains("\"fault\":\"retention-clock-boundary\""));
    assert!(receipts.contains("\"site\":\"retention.policy-evaluated\""));
    assert!(receipts.contains("\"cutoff\":"));
}

#[test]
fn ingest_purge_unlink_error_can_fire_at_purge() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..48)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "purge-unlink-error")
        })
        .expect("ingest-retention schedule reaches purge-unlink-error");
    let artifacts = tempfile::tempdir().expect("purge-unlink campaign artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("purge-unlink campaign episode");

    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert_eq!(outcome.same_seed_clean_controls, 1);
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let receipts = String::from_utf8(outcome.receipts_bytes).expect("receipt JSON is UTF-8");
    assert!(receipts.contains("\"fault\":\"purge-unlink-error\""));
    assert!(receipts.contains("\"site\":\"purge.old-segment-unlink.error\""));
    assert!(receipts.contains("\"intent_present\":true"));
    assert!(receipts.contains("\"old_path_linked\":true"));
}

#[cfg(unix)]
#[test]
fn ingest_purge_crash_boundary_can_fire_at_purge() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..48)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "purge-crash-boundary")
        })
        .expect("ingest-retention schedule reaches purge-crash-boundary");
    let artifacts = tempfile::tempdir().expect("purge-crash campaign artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("purge-crash campaign episode");

    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert_eq!(outcome.same_seed_clean_controls, 1);
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let receipts = String::from_utf8(outcome.receipts_bytes).expect("receipt JSON is UTF-8");
    assert!(receipts.contains("\"fault\":\"purge-crash-boundary\""));
    assert!(receipts.contains("\"site\":\"purge.after-durable-intent.before-rewrite\""));
    assert!(receipts.contains("\"intent_durable\":true"));
    assert!(receipts.contains("\"artifact_rewrites\":0"));
    assert!(receipts.contains("\"child_aborted\":true"));
}

#[cfg(unix)]
#[test]
fn every_ingest_retention_fault_can_fire_at_its_store_operation() {
    ingest_post_ack_retry_can_fire_at_batch_commit();
    ingest_partial_batch_append_can_fire_at_batch_commit();
    ingest_seal_cancellation_can_fire_at_seal();
    ingest_retention_clock_boundary_can_fire_at_retention();
    ingest_purge_unlink_error_can_fire_at_purge();
    ingest_purge_crash_boundary_can_fire_at_purge();
}

#[test]
fn feature_credit_has_no_generic_or_harness_fabricated_path() {
    let sources = [
        ("campaign registry", include_str!("adversarial/campaign.rs")),
        ("oracle dispatch", include_str!("adversarial/oracle.rs")),
        ("runner", include_str!("adversarial/runner.rs")),
    ];
    for (name, source) in sources {
        for forbidden in [
            "CheckerKind",
            "PrimitiveObservation",
            "campaign_search_facts",
            "record_campaign_invariant_checks",
            "run_feature_fault_probe",
            "pub struct FeatureFaultReceipt",
            "FeatureFaultReceipt {",
            "independently derived execution semantics",
            "Err(_) | Ok(_)",
            ".is_err()",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} can still earn feature credit through `{forbidden}`"
            );
        }
    }
}

#[test]
fn independent_oracle_is_a_std_only_source_and_dependency_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("adversarial-oracle");
    let manifest_path = root.join("Cargo.toml");
    assert!(
        manifest_path.is_file(),
        "independent oracle package is missing: {}",
        manifest_path.display()
    );
    let manifest = std::fs::read_to_string(&manifest_path).expect("read oracle manifest");
    assert!(manifest.contains("name = \"zeppelin-embed-adversarial-oracle\""));
    assert_eq!(
        manifest
            .split_once("[dependencies]")
            .map(|(_, dependencies)| dependencies.trim()),
        Some(""),
        "independent oracle must remain std-only"
    );
    for forbidden in [
        "zeppelin-embed =",
        "zeppelin-embed-ffi =",
        "zeppelin-embed-bench =",
        "xxhash-rust =",
        "path = \"../../crates/",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "independent oracle manifest contains production dependency {forbidden}"
        );
    }

    let mut pending = vec![root.join("src")];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("walk oracle sources") {
            let entry = entry.expect("read oracle source entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read oracle source");
            for forbidden in [
                "zeppelin_embed",
                "zeppelin-embed",
                "crate::fts",
                "crate::fusion",
                "crate::planner",
                "xxhash_rust",
                "xxhash-rust",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{} imported production helper {forbidden}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn storage_oracle_is_std_only() {
    independent_oracle_is_a_std_only_source_and_dependency_boundary();
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adversarial-oracle/src/storage_durability.rs"),
    )
    .expect("read independent storage oracle source");
    for forbidden in ["zeppelin_embed", "crate::adversarial", "super::runner"] {
        assert!(
            !source.contains(forbidden),
            "storage oracle crossed the std-only boundary through {forbidden}"
        );
    }
}

#[test]
fn overall_campaign_dispatch_preserves_the_original_program_bytes() {
    for seed in [0, 1, 7, 11, 90_004] {
        assert_eq!(
            Program::generate_for(CampaignKind::Overall, seed).jsonl(),
            Program::generate(seed).jsonl(),
            "overall program drifted for seed {seed}"
        );
    }
}

#[test]
fn feature_programs_are_namespaced_and_emit_the_declared_operations() {
    for campaign in CampaignKind::FEATURES {
        let spec = CampaignSpec::for_kind(campaign);
        let program = Program::generate_for(campaign, 7);
        let emitted = program
            .ops
            .iter()
            .filter_map(|operation| match operation {
                Op::Feature(operation) if operation.campaign() == campaign => Some(operation.key()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            emitted,
            spec.required_operations.iter().copied().collect(),
            "{} dedicated operation grammar drifted",
            campaign.key()
        );
        assert_ne!(
            program.jsonl(),
            Program::generate(7).jsonl(),
            "{} reused overall bytes instead of its RNG namespace",
            campaign.key()
        );
    }
}

#[test]
fn feature_programs_preserve_declared_dependency_order() {
    for campaign in CampaignKind::FEATURES {
        let spec = CampaignSpec::for_kind(campaign);
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            let observed = program
                .ops
                .iter()
                .filter_map(|operation| match operation {
                    Op::Feature(operation) if operation.campaign() == campaign => {
                        Some(operation.key())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                observed,
                spec.required_operations,
                "{} seed={seed} shuffled an operation ahead of its prerequisite",
                campaign.key()
            );
        }
    }
}

#[test]
fn feature_programs_distribute_operations_across_lifecycle_phases() {
    for campaign in CampaignKind::FEATURES {
        let program = Program::generate_for(campaign, 7);
        let feature_positions = program
            .ops
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| match operation {
                Op::Feature(operation) if operation.campaign() == campaign => Some(index),
                _ => None,
            })
            .collect::<Vec<_>>();
        let first_seal = program
            .ops
            .iter()
            .position(|operation| matches!(operation, Op::Seal))
            .expect("feature program has an active-to-sealed boundary");
        let last_seal = program
            .ops
            .iter()
            .rposition(|operation| matches!(operation, Op::Seal))
            .expect("feature program has a final sealed phase");

        assert!(
            feature_positions
                .first()
                .is_some_and(|position| *position < first_seal),
            "{} has no active-phase feature operation",
            campaign.key()
        );
        assert!(
            feature_positions
                .last()
                .is_some_and(|position| *position > last_seal),
            "{} still batches every feature operation before its final sealed phase",
            campaign.key()
        );
        assert!(
            feature_positions
                .windows(2)
                .all(|positions| positions[0] < positions[1]),
            "{} feature phases are not strictly ordered",
            campaign.key()
        );
    }
}

#[test]
fn feature_programs_exclude_every_legacy_epoch_operation() {
    for campaign in CampaignKind::FEATURES {
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            assert!(
                program.ops.iter().all(|operation| !matches!(
                    operation,
                    Op::EpochMismatchProbe { .. }
                        | Op::PrepareEpochB
                        | Op::SwitchAliasToB
                        | Op::RollbackToA
                        | Op::DropEpochA
                        | Op::RollbackDroppedAProbe
                )),
                "{} seed={seed} retained an epoch operation",
                campaign.key()
            );
        }
    }
}

#[test]
fn feature_fault_plan_has_a_clean_slot_and_full_has_two_distinct_faults() {
    for campaign in CampaignKind::FEATURES {
        let spec = CampaignSpec::for_kind(campaign);
        assert!(
            spec.feature_faults.iter().all(|fault| {
                !fault.key().contains("epoch") && !fault.label().contains("epoch")
            }),
            "{} retained an epoch-specific feature fault",
            campaign.key()
        );
        let mut selected = std::collections::BTreeSet::new();
        let mut clean = 0;
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            let plan = FaultPlan::for_program(
                campaign,
                seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            );
            assert!(plan.feature.len() <= 1, "{} seed={seed}", campaign.key());
            if let Some(fault) = plan.feature.first() {
                selected.insert(fault.fault.key());
            } else {
                clean += 1;
            }

            let full = FaultPlan::for_program(
                campaign,
                seed,
                FaultProfile::Full,
                &program,
                FaultSchedule::default(),
            );
            assert_eq!(full.feature.len(), 2, "{} seed={seed}", campaign.key());
            assert_ne!(full.feature[0].fault, full.feature[1].fault);
        }
        assert!(
            clean > 0,
            "{} has no clean feature-fault seed",
            campaign.key()
        );
        assert_eq!(
            selected,
            spec.feature_faults
                .iter()
                .map(|fault| fault.key())
                .collect(),
            "{} smoke cannot select its complete feature-fault vocabulary",
            campaign.key()
        );
    }
}

#[test]
fn isolated_vfs_probes_cannot_issue_feature_fault_receipts() {
    let source = include_str!("adversarial/fault_vfs.rs");
    assert!(!source.contains("fire_feature_fault"));
    assert!(!source.contains("/feature.bin"));
    assert!(!source.contains("FeatureFaultReceipt"));
}

#[test]
fn production_feature_receipt_boundary_is_hidden_and_fact_complete() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/zeppelin-embed/src");
    let source_path = root.join("adversarial_test_support.rs");
    assert!(
        source_path.is_file(),
        "production receipt boundary is missing: {}",
        source_path.display()
    );
    let source = std::fs::read_to_string(&source_path).expect("read receipt boundary");
    for required in [
        "campaign",
        "operation",
        "fault",
        "site",
        "cardinality",
        "effect",
        "pub(crate) fn new",
    ] {
        assert!(source.contains(required), "receipt omitted {required}");
    }
    let lib = std::fs::read_to_string(root.join("lib.rs")).expect("read core lib");
    assert!(
        lib.contains("#[cfg(any(test, feature = \"test-support\"))]\n#[doc(hidden)]\npub mod adversarial_test_support;"),
        "receipt boundary escaped the hidden test-support gate"
    );
}

#[test]
fn selected_feature_fault_fires_once_at_its_declared_operation() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            !FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("storage campaign has a selected fault seed");
    let artifacts = tempfile::tempdir().expect("feature episode artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("the episode retains its failure evidence");
    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
}

#[test]
fn every_storage_fault_fires_through_its_exact_campaign_operation() {
    let campaign = CampaignKind::StorageDurability;
    let mut fired = BTreeSet::new();
    for seed in 0..12 {
        let program = Program::generate_for(campaign, seed);
        let plan = FaultPlan::for_program(
            campaign,
            seed,
            FaultProfile::None,
            &program,
            FaultSchedule::default(),
        );
        if plan.feature.is_empty() {
            continue;
        }
        let artifacts = tempfile::tempdir().expect("storage fault artifacts");
        let outcome = adversarial::runner::run_program_for(
            campaign,
            seed,
            FaultProfile::None,
            artifacts.path(),
        )
        .expect("storage feature fault episode");
        assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
        assert_eq!(
            outcome.feature_faults_scheduled, outcome.feature_faults_fired,
            "storage seed {seed} missed its selected fault"
        );
        assert_eq!(outcome.integrated_feature_fault_receipts, 1);
        assert_eq!(outcome.expected_feature_fault_receipts, 1);
        assert_eq!(outcome.same_seed_clean_controls, 1);
        for line in outcome
            .receipts_bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line)
                    .expect("parse typed storage receipt record");
            assert_eq!(record["campaign"], "storage-durability");
            assert!(record["operation"].is_string());
            assert!(record["fault"].is_string());
            assert!(record["site"].is_string());
            assert_eq!(record["cardinality"], 1);
            assert!(
                record["plan"].is_object(),
                "storage receipt retained an opaque plan"
            );
            assert!(
                record["observed"].is_object(),
                "storage receipt retained an opaque observation"
            );
            assert!(
                record["receipt_digest"]
                    .as_str()
                    .is_some_and(|digest| digest.starts_with("fnv1a64:")),
                "storage receipt omitted its canonical digest"
            );
        }
        fired.extend(plan.feature.into_iter().map(|event| event.fault.key()));
    }
    assert_eq!(
        fired,
        CampaignSpec::for_kind(campaign)
            .feature_faults
            .iter()
            .map(|fault| fault.key())
            .collect(),
        "storage campaign did not CAN-FIRE its full vocabulary"
    );
}

fn assert_storage_fault_can_fire(
    fault: adversarial::campaign::FeatureFault,
    seed_matches: impl Fn(u64) -> bool,
) {
    assert_storage_fault_can_fire_with_profile(fault, FaultProfile::None, seed_matches);
}

fn assert_storage_fault_can_fire_with_profile(
    fault: adversarial::campaign::FeatureFault,
    profile: FaultProfile,
    seed_matches: impl Fn(u64) -> bool,
) {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..4096)
        .find(|seed| {
            if !seed_matches(*seed) {
                return false;
            }
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(campaign, *seed, profile, &program, FaultSchedule::default())
                .feature
                .iter()
                .any(|event| event.fault == fault)
        })
        .unwrap_or_else(|| panic!("storage campaign never schedules {}", fault.key()));
    let artifacts = tempfile::tempdir().expect("storage CAN-FIRE artifacts");
    let outcome = adversarial::runner::run_program_for(campaign, seed, profile, artifacts.path())
        .expect("storage CAN-FIRE episode");
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert_eq!(outcome.feature_faults_scheduled, 1, "{}", fault.key());
    assert_eq!(outcome.feature_faults_fired, 1, "{}", fault.key());
    assert_eq!(
        outcome.expected_feature_fault_receipts,
        1,
        "{}",
        fault.key()
    );
    assert_eq!(
        outcome.integrated_feature_fault_receipts,
        1,
        "{}",
        fault.key()
    );
    assert_eq!(outcome.same_seed_clean_controls, 1, "{}", fault.key());
    assert!(outcome.missing_feature_faults.is_empty(), "{}", fault.key());
}

#[test]
fn storage_torn_wal_header_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageTornWalHeader,
        |_| true,
    );
}

#[test]
fn storage_torn_wal_body_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageTornWalBody,
        |_| true,
    );
}

#[test]
fn storage_torn_wal_checksum_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageTornWalChecksum,
        |_| true,
    );
}

#[test]
fn storage_post_commit_error_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StoragePostCommitError,
        |_| true,
    );
}

#[test]
fn storage_manifest_pre_rename_crash_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageManifestPreRenameCrash,
        |_| true,
    );
}

#[test]
fn storage_manifest_post_rename_crash_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageManifestPostRenameCrash,
        |_| true,
    );
}

#[test]
fn storage_corrupt_segment_region_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageCorruptSegmentRegion,
        |_| true,
    );
}

#[test]
fn storage_wrong_manifest_object_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageWrongManifestObject,
        |_| true,
    );
}

#[test]
fn storage_wrong_segment_object_can_fire() {
    let fault = adversarial::campaign::FeatureFault::StorageWrongSegmentObject;
    assert_storage_fault_can_fire(fault, |seed| seed & 1 == 0);
    assert_storage_fault_can_fire(fault, |seed| seed & 1 == 1);
}

#[test]
fn storage_list_omission_can_fire() {
    assert_storage_fault_can_fire(
        adversarial::campaign::FeatureFault::StorageListDeleteOmission,
        |seed| {
            adversarial::storage_durability::omission_case_for_schedule(seed, 0).subsite
                == zeppelin_embed_adversarial_oracle::storage_durability::OmissionSubsite::List
        },
    );
}

#[test]
fn storage_delete_omission_can_fire() {
    assert_storage_fault_can_fire_with_profile(
        adversarial::campaign::FeatureFault::StorageListDeleteOmission,
        FaultProfile::IoErrors,
        |seed| {
            adversarial::storage_durability::omission_case_for_schedule(seed, 1).subsite
                == zeppelin_embed_adversarial_oracle::storage_durability::OmissionSubsite::Delete
        },
    );
}

#[test]
fn storage_content_fault_refusal_does_not_skip_the_selected_feature_operation() {
    for seed in [3_u64, 5_u64] {
        let artifacts = tempfile::tempdir().expect("storage content fault artifacts");
        let outcome = adversarial::runner::run_program_for(
            CampaignKind::StorageDurability,
            seed,
            FaultProfile::Content,
            artifacts.path(),
        )
        .unwrap_or_else(|error| panic!("storage seed {seed} content episode: {error}"));

        assert_eq!(
            outcome.feature_faults_fired, outcome.feature_faults_scheduled,
            "storage seed {seed} skipped its selected feature operation after the generic content refusal; missing={:?}",
            outcome.missing_feature_faults,
        );
        assert!(
            outcome.missing_feature_faults.is_empty(),
            "storage seed {seed} left selected feature faults unfired: {:?}",
            outcome.missing_feature_faults,
        );
        assert!(
            outcome.violations.is_empty(),
            "storage seed {seed} content episode recorded violations after the expected refusal: {:?}",
            outcome.violations,
        );
    }
}

#[test]
fn storage_wal_damage_records_i16_and_the_exact_i18_projection_once() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..128)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault == adversarial::campaign::FeatureFault::StorageTornWalHeader)
        })
        .expect("storage campaign schedules torn WAL header");
    let artifacts = tempfile::tempdir().expect("storage WAL projection artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("storage WAL projection episode");

    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert_eq!(outcome.comparison_counts.get("I16"), Some(&1));
    assert_eq!(
        outcome.comparison_counts.get("I18"),
        Some(&2),
        "the WAL refusal must be checked once as I16 and once through I18 in addition to the format-check operation"
    );
    assert_eq!(outcome.integrated_feature_fault_receipts, 1);
    let observations =
        String::from_utf8(outcome.family_artifact_bytes["storage-observations.jsonl"].clone())
            .expect("storage observations are UTF-8");
    assert!(
        observations.contains("\"case\":\"wal-header\""),
        "WAL I18 projection was not retained: {observations}"
    );
}

#[test]
fn storage_shared_subcases_require_validated_production_receipts() {
    let campaign = CampaignKind::StorageDurability;
    let mut seeds = BTreeMap::<&'static str, u64>::new();
    for seed in 0..4096 {
        let program = Program::generate_for(campaign, seed);
        let plan = FaultPlan::for_program(
            campaign,
            seed,
            FaultProfile::None,
            &program,
            FaultSchedule::default(),
        );
        let Some(event) = plan.feature.first() else {
            continue;
        };
        let key = match event.fault {
            adversarial::campaign::FeatureFault::StorageWrongSegmentObject => {
                if seed & 1 == 0 {
                    "feature_fault.storage-durability.wrong-segment-object.site.family"
                } else {
                    "feature_fault.storage-durability.wrong-segment-object.site.identity"
                }
            }
            adversarial::campaign::FeatureFault::StorageListDeleteOmission => {
                if adversarial::storage_durability::omission_case_for_schedule(seed, 0).subsite
                    == zeppelin_embed_adversarial_oracle::storage_durability::OmissionSubsite::Delete
                {
                    "feature_fault.storage-durability.list-delete-omission.site.delete"
                } else {
                    "feature_fault.storage-durability.list-delete-omission.site.list"
                }
            }
            _ => continue,
        };
        seeds.entry(key).or_insert(seed);
        if seeds.len() == 4 {
            break;
        }
    }
    assert_eq!(seeds.len(), 4, "could not schedule every storage subcase");

    let mut coverage = CoverageRegistry::default();
    for (key, seed) in &seeds {
        let artifacts = tempfile::tempdir().expect("storage subcase artifacts");
        let outcome = adversarial::runner::run_program_for(
            campaign,
            *seed,
            FaultProfile::None,
            artifacts.path(),
        )
        .expect("storage subcase episode");
        assert!(
            outcome.violations.is_empty(),
            "{key}: {:?}",
            outcome.violations
        );
        assert_eq!(outcome.feature_faults_fired, 1, "{key}");
        coverage.merge(&outcome.coverage);
    }
    let missing = CampaignSpec::for_kind(campaign)
        .required_coverage
        .iter()
        .copied()
        .filter(|key| coverage.count(key) == 0)
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "missing storage subcase coverage: {missing:?}"
    );
}

#[test]
fn storage_observation_stream_retains_child_and_cleanup_intermediate_facts() {
    let campaign = CampaignKind::StorageDurability;
    let mut publication_seed = None;
    let mut cleanup_seed = None;
    for seed in 0..128 {
        let program = Program::generate_for(campaign, seed);
        let plan = FaultPlan::for_program(
            campaign,
            seed,
            FaultProfile::None,
            &program,
            FaultSchedule::default(),
        );
        match plan.feature.first().map(|event| event.fault) {
            Some(adversarial::campaign::FeatureFault::StorageManifestPreRenameCrash) => {
                publication_seed.get_or_insert(seed);
            }
            Some(adversarial::campaign::FeatureFault::StorageListDeleteOmission) => {
                cleanup_seed.get_or_insert(seed);
            }
            _ => {}
        }
        if publication_seed.is_some() && cleanup_seed.is_some() {
            break;
        }
    }
    for (label, seed, operation, kind, nested) in [
        (
            "publication child",
            publication_seed.expect("publication fault seed"),
            "publication",
            "child-abort",
            "child",
        ),
        (
            "cleanup intermediate",
            cleanup_seed.expect("cleanup fault seed"),
            "orphan-cleanup",
            "omission",
            "omission",
        ),
    ] {
        let root = tempfile::tempdir().expect("storage detail artifacts");
        let outcome =
            adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
                .expect("storage detail episode");
        let retained = outcome.family_artifact_bytes["storage-observations.jsonl"]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                zeppelin_embed_bench::harness_json::from_slice::<
                    zeppelin_embed_bench::harness_json::Value,
                >(line)
                .expect("parse storage observation")
            })
            .any(|record| {
                record["operation"] == operation
                    && record["operation_detail"]["kind"] == kind
                    && record["operation_detail"][nested].is_object()
            });
        assert!(
            retained,
            "{label} facts were omitted from typed storage observations"
        );
    }
}

#[test]
fn every_feature_fault_can_fire_once_at_its_declared_operation() {
    for campaign in CampaignKind::FEATURES {
        let mut fired = BTreeSet::new();
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            let plan = FaultPlan::for_program(
                campaign,
                seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            );
            if plan.feature.is_empty() {
                continue;
            }
            let artifacts = tempfile::tempdir().expect("feature fault artifacts");
            let outcome = adversarial::runner::run_program_for(
                campaign,
                seed,
                FaultProfile::None,
                artifacts.path(),
            )
            .expect("feature fault episode");
            assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
            assert_eq!(
                outcome.feature_faults_scheduled, outcome.feature_faults_fired,
                "{campaign} seed {seed} did not fire its selected fault",
            );
            assert!(outcome.missing_feature_faults.is_empty());
            fired.extend(plan.feature.into_iter().map(|event| event.fault.key()));
        }
        assert_eq!(
            fired,
            CampaignSpec::for_kind(campaign)
                .feature_faults
                .iter()
                .map(|fault| fault.key())
                .collect(),
            "{campaign} did not CAN-FIRE its full vocabulary",
        );
    }
}

#[test]
fn clean_feature_episode_executes_every_bound_checker() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("storage campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("clean feature artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("clean feature episode");

    assert_eq!(outcome.campaign, campaign);
    assert_eq!(outcome.feature_faults_scheduled, 0);
    assert_eq!(outcome.feature_faults_fired, 0);
    assert_eq!(outcome.expected_feature_fault_receipts, 0);
    assert_eq!(outcome.same_seed_clean_controls, 0);
    assert!(outcome.missing_feature_faults.is_empty());
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    for binding in CampaignSpec::for_kind(campaign).invariant_specs {
        assert!(
            oracle.contains(&format!("\"checker_id\":\"{}\"", binding.checker_id)),
            "{} did not emit its exact checker record: {oracle}",
            binding.checker_id,
        );
        assert!(
            oracle.contains(&format!("\"invariant\":\"{}\"", binding.invariant.key())),
            "{} did not name its invariant: {oracle}",
            binding.checker_id,
        );
    }
    for invariant in CampaignSpec::for_kind(campaign).owned_invariants {
        assert!(outcome.coverage.count(&invariant.checked_coverage_key()) > 0);
    }
}

#[test]
fn metadata_feature_episode_uses_exact_i36_i39_adapters() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("metadata campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("metadata feature artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("metadata clean feature episode");

    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert_eq!(outcome.feature_faults_scheduled, 0);
    assert_eq!(outcome.feature_faults_fired, 0);
    assert_eq!(outcome.expected_feature_fault_receipts, 0);
    assert_eq!(outcome.same_seed_clean_controls, 0);
    let metadata: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join(format!(
                "metadata-filter-planner/seed-{seed}-none/episode.json"
            )))
            .expect("metadata episode attestation"),
        )
        .expect("parse metadata episode attestation");
    assert_eq!(
        metadata["attestation"]["expected_feature_fault_receipts"],
        0
    );
    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    for checker in [
        "I36.column-roundtrip.v2",
        "I37.bitmap-algebra.v2",
        "I38.pruning-soundness.v2",
        "I39.executed-branch.v2",
    ] {
        assert!(
            oracle.contains(&format!("\"checker_id\":\"{checker}\"")),
            "metadata episode omitted {checker}: {oracle}"
        );
    }
    assert!(
        !outcome.receipts_bytes.is_empty(),
        "metadata execution attestations were not retained"
    );
}

#[test]
fn every_metadata_fault_uses_its_declared_production_receipt_cardinality() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let mut fired = BTreeSet::new();
    for seed in 0..12 {
        let program = Program::generate_for(campaign, seed);
        let plan = FaultPlan::for_program(
            campaign,
            seed,
            FaultProfile::None,
            &program,
            FaultSchedule::default(),
        );
        if plan.feature.is_empty() {
            continue;
        }
        let expected_receipts = plan
            .feature
            .iter()
            .map(|event| {
                u64::try_from(event.fault.required_receipt_cardinality())
                    .expect("feature receipt cardinality fits u64")
            })
            .sum::<u64>();
        let artifacts = tempfile::tempdir().expect("metadata fault artifacts");
        let outcome = adversarial::runner::run_program_for(
            campaign,
            seed,
            FaultProfile::None,
            artifacts.path(),
        )
        .expect("metadata feature fault episode");
        assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
        assert_eq!(
            outcome.feature_faults_scheduled, outcome.feature_faults_fired,
            "metadata seed {seed} missed its selected fault"
        );
        assert_eq!(
            outcome.integrated_feature_fault_receipts, expected_receipts,
            "metadata seed {seed} used the wrong receipt cardinality"
        );
        assert_eq!(
            outcome.expected_feature_fault_receipts, expected_receipts,
            "metadata seed {seed} recorded the wrong planned receipt cardinality"
        );
        assert_eq!(outcome.same_seed_clean_controls, 1);
        fired.extend(plan.feature.into_iter().map(|event| event.fault.key()));
    }
    assert_eq!(
        fired,
        CampaignSpec::for_kind(campaign)
            .feature_faults
            .iter()
            .map(|fault| fault.key())
            .collect(),
        "metadata campaign did not CAN-FIRE its full vocabulary"
    );
}

#[test]
fn metadata_i38_pruned_source_is_independently_proved_and_attested() {
    let evidence = adversarial::metadata_filter_planner::run_metadata_operation(
        adversarial::metadata_filter_planner::MetadataOperationKind::Planner,
        0x38,
        None,
    )
    .expect("metadata pruning evidence");
    let adversarial::metadata_filter_planner::MetadataInvariantEvidence::I38 { input, observed } =
        evidence.invariant
    else {
        panic!("metadata pruning operation returned the wrong invariant");
    };
    assert!(
        !observed.pruned_sources.is_empty(),
        "metadata I38 adapter did not exercise a real pruned source"
    );
    let mut planted = observed.clone();
    planted.pruned_sources.clear();
    let error =
        zeppelin_embed_adversarial_oracle::metadata_filter_planner::compare_i38(&input, &planted)
            .expect_err("removing a proven pruned source must reject its production receipt");
    assert!(
        error.contains("I38.pruning-soundness.v2")
            && error.contains("orphan Pruned execution receipt"),
        "wrong I38 plant failure: {error}"
    );
    zeppelin_embed_adversarial_oracle::metadata_filter_planner::compare_i38(&input, &observed)
        .expect("unmodified public pruning evidence must pass I38");
}

#[test]
fn every_vector_fault_uses_one_typed_production_receipt_and_same_seed_control() {
    let campaign = CampaignKind::VectorExecution;
    let mut fired = BTreeSet::new();
    let mut coverage = CoverageRegistry::default();
    for seed in 0..30 {
        let program = Program::generate_for(campaign, seed);
        let plan = FaultPlan::for_program(
            campaign,
            seed,
            FaultProfile::None,
            &program,
            FaultSchedule::default(),
        );
        if plan.feature.is_empty() {
            continue;
        }
        let artifacts = tempfile::tempdir().expect("vector fault artifacts");
        let outcome = adversarial::runner::run_program_for(
            campaign,
            seed,
            FaultProfile::None,
            artifacts.path(),
        )
        .expect("vector feature fault episode");
        assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
        assert_eq!(
            outcome.feature_faults_scheduled, outcome.feature_faults_fired,
            "vector seed {seed} missed its selected fault"
        );
        assert_eq!(
            outcome.integrated_feature_fault_receipts, 1,
            "vector seed {seed} emitted the wrong typed receipt count"
        );
        assert_eq!(outcome.expected_feature_fault_receipts, 1);
        assert_eq!(outcome.same_seed_clean_controls, 1);
        coverage.merge(&outcome.coverage);
        fired.extend(plan.feature.into_iter().map(|event| event.fault.key()));
    }
    assert_eq!(
        fired,
        CampaignSpec::for_kind(campaign)
            .feature_faults
            .iter()
            .map(|fault| fault.key())
            .collect(),
        "vector campaign did not CAN-FIRE its full typed vocabulary"
    );
    let missing = expected_vector_family_coverage()
        .into_iter()
        .filter(|key| coverage.count(key) == 0)
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "typed vector evidence missed exact coverage predicates: {missing:?}"
    );
}

#[test]
fn vector_forced_child_replay_retains_typed_binary_product_receipt() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..128)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| {
                event.fault == adversarial::campaign::FeatureFault::VectorForcedDispatchBackend
            })
        })
        .expect("vector campaign schedules forced backend");
    let artifacts = tempfile::tempdir().expect("vector forced child artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("vector forced child episode");
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["fixture.json"],
        )
        .expect("parse vector fixture");
    let kernel = fixture["operations"]
        .as_array()
        .expect("vector fixture operations")
        .iter()
        .find(|record| record["operation"] == "kernel-parity")
        .expect("kernel-parity fixture record");
    assert_eq!(kernel["forced_child"]["transport"], "typed-binary-v1");
    assert_eq!(
        kernel["forced_child"]["receipt"]["campaign"],
        "vector-execution"
    );
    assert!(kernel["forced_child"]["receipt"]["effect"].is_object());
    assert_eq!(kernel["forced_child"]["receipt"]["result_published"], true);
}

#[test]
fn vector_feature_episode_uses_only_exact_family_checkers() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("vector campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("vector feature artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("vector feature episode");

    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    for checker in [
        "I24.kernel-contract-parity.v1",
        "I25.quantization-contract.v1",
        "I26.exact-rescore-contract.v1",
        "I27.row-identity-lifecycle.v1",
    ] {
        assert!(
            oracle.contains(&format!("\"checker_id\":\"{checker}\"")),
            "vector episode omitted {checker}: {oracle}"
        );
    }
}

#[test]
fn vector_kernel_operation_uses_exact_i24_and_real_store_selection() {
    let campaign = CampaignKind::VectorExecution;
    let seed = 5;
    let artifacts = tempfile::tempdir().expect("I24 operation artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("I24 operation episode");
    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    let expected_records =
        zeppelin_embed::kernels::KernelVariant::available().count() * 15 * 17 + 1;
    assert_eq!(
        oracle
            .matches("\"checker_id\":\"I24.kernel-contract-parity.v1\"")
            .count(),
        expected_records
    );
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            outcome
                .family_artifact_bytes
                .get("fixture.json")
                .expect("vector fixture artifact"),
        )
        .expect("parse vector fixture artifact");
    let kernel = fixture["operations"]
        .as_array()
        .expect("vector fixture operations")
        .iter()
        .find(|operation| operation["operation"] == "kernel-parity")
        .expect("kernel-parity fixture operation");
    let inputs = kernel["inputs"].as_array().expect("I24 primitive inputs");
    let unique_case_ids = inputs
        .iter()
        .map(|input| input["case_id"].as_u64().expect("I24 case id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(inputs.len(), expected_records);
    assert_eq!(
        unique_case_ids.len(),
        inputs.len(),
        "I24 emitted a duplicate case id"
    );
    let mut cases_per_cell = BTreeMap::new();
    for input in inputs.iter().filter(|input| {
        !input["selected_for_store"]
            .as_bool()
            .expect("I24 Store-selection flag")
    }) {
        let cell = (
            input["backend"].as_str().expect("I24 backend"),
            input["kernel"].as_str().expect("I24 kernel"),
            input["dimension"].as_u64().expect("I24 dimension"),
            input["input_offset"].as_u64().expect("I24 input offset"),
        );
        *cases_per_cell.entry(cell).or_insert(0_usize) += 1;
    }
    for ((_, kernel, _, _), count) in cases_per_cell {
        let expected = if matches!(kernel, "dot-f32" | "dot-f16") {
            3
        } else {
            1
        };
        assert_eq!(count, expected, "I24 emitted the wrong cases for {kernel}");
    }
    assert!(
        outcome
            .controls_bytes
            .windows(b"\"operation\":\"kernel-parity\"".len())
            .any(|window| window == b"\"operation\":\"kernel-parity\""),
        "I24 omitted the production Store scoring observation"
    );
    assert!(
        outcome
            .violations
            .iter()
            .all(|violation| !violation.detail.contains("kernel-parity")),
        "I24 operation still failed: {:?}",
        outcome.violations
    );
}

fn expected_vector_family_coverage() -> BTreeSet<String> {
    const KERNELS: [&str; 11] = [
        "dot-i8",
        "hamming-u1",
        "dot-f32",
        "dot-f16",
        "dot-i8-batch",
        "hamming-u1-batch",
        "dot-bit4",
        "dot-bit4-prepared",
        "dot-bit4-batch",
        "score-bit4-prepared-batch",
        "score-bit4-ptrs",
    ];
    const DIMENSIONS: [u64; 17] = [
        0, 1, 2, 3, 7, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 768,
    ];
    const ALL_BACKENDS: [&str; 9] = [
        "scalar",
        "neon-widen",
        "neon-dotprod-u4",
        "neon-i8mm",
        "neon-dotprod-u2",
        "neon-dotprod-u6",
        "neon-dotprod-u8",
        "neon-dotprod-u4-prefetch",
        "avx2",
    ];
    let available = zeppelin_embed::kernels::KernelVariant::available()
        .map(|variant| variant.backend_id().as_str())
        .collect::<BTreeSet<_>>();
    let mut expected = BTreeSet::new();
    for backend in &available {
        for kernel in KERNELS {
            for (dimension_index, dimension) in DIMENSIONS.into_iter().enumerate() {
                expected.insert(format!(
                    "I24.kernel.{kernel}.backend.{backend}.dimension.{dimension}.offset.{}",
                    dimension_index % 2
                ));
            }
        }
    }
    for backend in ALL_BACKENDS {
        let availability = if available.contains(backend) {
            "available"
        } else {
            "unavailable"
        };
        expected.insert(format!("I24.backend.{availability}.{backend}"));
    }
    for precision in ["f32", "f16"] {
        for class in ["neg-zero", "subnormal", "pos-inf", "neg-inf", "nan"] {
            expected.insert(format!("I24.special.{precision}.{class}"));
        }
    }
    expected.insert("I24.f32.seeded-raw-finite".to_owned());
    expected.insert("I24.f32.cancellation-heavy-alternating-magnitude".to_owned());
    expected.insert(format!(
        "I24.store-selected.{}",
        zeppelin_embed::kernels::KernelVariant::selected()
            .backend_id()
            .as_str()
    ));

    for scheme in ["bit4", "int8"] {
        for boundary in [
            "empty",
            "dimension-65537",
            "output-short",
            "output-long",
            "code-short",
            "code-long",
        ] {
            expected.insert(format!("I25.{scheme}.{boundary}"));
        }
        for side in ["row", "query"] {
            for class in ["nan", "pos-inf", "neg-inf"] {
                for position in ["first", "middle", "last"] {
                    expected.insert(format!("I25.{scheme}.{side}.{class}.{position}"));
                }
            }
        }
        for cell in [
            "even",
            "odd",
            "constant",
            "signed-zero",
            "subnormal",
            "extreme-finite",
            "halfway",
            "threshold-tie",
        ] {
            expected.insert(format!("I25.{scheme}.positive.{cell}"));
        }
    }
    for key in [
        "I25.bit4.store-accepted-visible",
        "I25.int8.store-published",
        "I25.bit4.store-rejected-nonfinite",
        "I25.bit4.stochastic-query.distinct-four",
        "I26.mode.dense",
        "I26.mode.retained",
        "I26.reject.candidate-count",
        "I26.reject.candidate-out-of-range",
        "I26.reject.nonfinite-coarse",
        "I26.store.active.exact",
        "I26.store.active.scan",
        "I26.store.active.auto-estimated",
        "I26.store.sealed.exact",
        "I26.store.sealed.graph",
        "I26.store.sealed.auto-graph",
        "I26.store.active.exact.anti-correlated-document-tie",
        "I27.physical.active-row-zero",
        "I27.physical.first-sealed-row-zero",
        "I27.physical.second-sealed-row-zero",
        "I27.transition.replace",
        "I27.transition.delete",
        "I27.transition.reopen",
        "fault.forced-backend.kernel-dispatch-selected-scoring-table",
        "fault.quant.bit4-odd-padding.scan-bit4-code-view",
        "fault.quant.bit4-correction.scan-bit4-factor-view",
        "fault.quant.int8-scale.scan-int8-factor-view",
        "fault.rescore.exact.exact-rescore-rows",
        "fault.rescore.graph.query-rescore-rows",
        "fault.cancel.active-exact",
        "fault.cancel.sealed-bit4-scan",
        "fault.cancel.sealed-int8-scan",
        "fault.cancel.sealed-graph",
        "fault.allocation.exact.search-global-candidates",
    ] {
        expected.insert(key.to_owned());
    }
    for (phase, tiers) in [
        (0, &["auto", "exact", "scan"][..]),
        (1, &["auto", "exact", "scan"][..]),
        (2, &["auto", "exact", "scan", "graph"][..]),
        (3, &["auto", "exact", "scan"][..]),
        (4, &["auto", "exact", "scan", "graph"][..]),
    ] {
        for tier in tiers {
            expected.insert(format!("I27.phase.{phase}.tier.{tier}"));
        }
    }
    expected
}

#[test]
fn vector_campaign_requires_the_exact_typed_coverage_catalog() {
    let actual = CampaignSpec::for_kind(CampaignKind::VectorExecution)
        .all_required_coverage()
        .into_iter()
        .filter(|key| {
            key.starts_with("I24.")
                || key.starts_with("I25.")
                || key.starts_with("I26.")
                || key.starts_with("I27.")
                || key.starts_with("fault.")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected_vector_family_coverage());
}

#[test]
fn vector_campaign_requires_positive_quantization_and_document_tie_cells() {
    let required = CampaignSpec::for_kind(CampaignKind::VectorExecution)
        .all_required_coverage()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for scheme in ["bit4", "int8"] {
        for cell in [
            "even",
            "odd",
            "constant",
            "signed-zero",
            "subnormal",
            "extreme-finite",
            "halfway",
            "threshold-tie",
        ] {
            let key = format!("I25.{scheme}.positive.{cell}");
            assert!(required.contains(&key), "missing required coverage {key}");
        }
    }
    for key in [
        "I25.bit4.stochastic-query.distinct-four",
        "I26.store.active.exact.anti-correlated-document-tie",
    ] {
        assert!(required.contains(key), "missing required coverage {key}");
    }
}

#[test]
fn overall_dispatch_preserves_all_four_compatibility_artifact_streams() {
    for (seed, profile) in [(0, FaultProfile::None), (7, FaultProfile::Content)] {
        let old_root = tempfile::tempdir().expect("old overall artifacts");
        let new_root = tempfile::tempdir().expect("dispatched overall artifacts");
        let old = adversarial::runner::run_program(seed, profile, old_root.path())
            .expect("legacy overall run");
        let new = adversarial::runner::run_program_for(
            CampaignKind::Overall,
            seed,
            profile,
            new_root.path(),
        )
        .expect("dispatched overall run");
        assert_eq!(old.program_bytes, new.program_bytes);
        assert_eq!(old.faults_bytes, new.faults_bytes);
        assert_eq!(old.violations_bytes, new.violations_bytes);
        assert_eq!(old.coverage_bytes, new.coverage_bytes);
    }
}

#[test]
fn overall_content_fault_stops_after_typed_refusal_without_downstream_epoch_violations() {
    let artifacts = tempfile::tempdir().expect("overall content-fault artifacts");
    let outcome = adversarial::runner::run_program_for(
        CampaignKind::Overall,
        0,
        FaultProfile::Content,
        artifacts.path(),
    )
    .expect("overall content-fault episode");

    assert!(
        outcome.violations.is_empty(),
        "legacy overall continued through a poisoned Store: {:?}",
        outcome.violations,
    );
}

#[test]
fn feature_episode_writes_schema_v3_replay_metadata() {
    let root = tempfile::tempdir().expect("feature metadata root");
    let campaign = CampaignKind::Fts;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("FTS campaign has a clean feature-fault slot");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("feature metadata episode");
    assert_eq!(outcome.campaign, campaign);
    let directory = root.path().join(format!("fts/seed-{seed}-none"));
    let metadata: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(directory.join("episode.json")).expect("episode metadata"),
        )
        .expect("parse episode metadata");
    assert_eq!(metadata["version"], 3);
    assert_eq!(metadata["campaign"], "fts");
    assert_eq!(metadata["attestation"]["oracle_contract_version"], 1);
    assert!(
        metadata["attestation"]["harness_git_revision"]
            .as_str()
            .is_some_and(|revision| !revision.is_empty() && revision != "unknown")
    );
    assert!(metadata["attestation"]["comparison_counts"].is_object());
    assert!(metadata["attestation"]["same_seed_clean_controls"].is_u64());
    assert!(metadata["attestation"]["integrated_feature_fault_receipts"].is_u64());
    assert!(metadata["attestation"]["expected_feature_fault_receipts"].is_u64());
    assert!(metadata["attestation"]["evidence_digests"].is_object());
    assert_eq!(
        metadata["attestation"]["replay_artifacts"],
        zeppelin_embed_bench::harness_json::json!([
            "program.jsonl",
            "faults.jsonl",
            "violations.json",
            "coverage.json",
            "oracle.jsonl",
            "controls.jsonl",
            "receipts.jsonl",
            "mutations.jsonl",
            "episode.json",
        ])
    );
    for name in adversarial::artifacts::REPLAY_ARTIFACTS {
        assert!(
            directory.join(name).is_file(),
            "missing replay artifact {name}"
        );
    }
    assert_eq!(
        adversarial::campaign::campaign_from_replay_metadata(&directory)
            .expect("feature replay campaign"),
        CampaignKind::Fts
    );
}

#[test]
fn feature_replay_compares_every_attested_artifact() {
    assert_eq!(
        adversarial::artifacts::REPLAY_ARTIFACTS,
        [
            "program.jsonl",
            "faults.jsonl",
            "violations.json",
            "coverage.json",
            "oracle.jsonl",
            "controls.jsonl",
            "receipts.jsonl",
            "mutations.jsonl",
            "episode.json",
        ]
    );
}

#[test]
fn storage_replay_declares_every_family_evidence_stream() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("storage campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("storage replay metadata root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("storage feature episode");
    let directory = root
        .path()
        .join(format!("storage-durability/seed-{seed}-none"));
    let metadata: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(directory.join("episode.json")).expect("storage episode metadata"),
        )
        .expect("parse storage episode metadata");
    let declared = metadata["attestation"]["replay_artifacts"]
        .as_array()
        .expect("storage replay artifact list")
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<BTreeSet<_>>();
    for name in [
        "storage-fixture.json",
        "ack-ledger.jsonl",
        "storage-observations.jsonl",
        "feature-receipts.jsonl",
        "clean-controls.jsonl",
        "artifact-index.jsonl",
    ] {
        assert!(declared.contains(name), "storage replay omitted {name}");
        assert!(
            directory.join(name).is_file(),
            "storage episode omitted {name}"
        );
    }
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(directory.join("storage-fixture.json"))
                .expect("storage fixture evidence"),
        )
        .expect("parse storage fixture evidence");
    let documents = fixture["documents"]
        .as_array()
        .expect("storage fixture documents must be primitive JSON records");
    assert!(!documents.is_empty());
    for document in documents {
        assert!(document["doc_id"].as_str().is_some());
        assert!(document["revision"].is_u64());
        assert!(document["vector_bits"].is_array());
        assert!(document["columns"].is_array());
    }
    let mutations = fixture["mutations"]
        .as_array()
        .expect("storage fixture mutations must be primitive JSON records");
    assert!(!mutations.is_empty());
    for mutation in mutations {
        assert!(mutation["operation_id"].as_str().is_some());
        assert!(mutation["canonical_payload_digest"].as_str().is_some());
        assert!(mutation["first_seq"].is_u64());
        assert!(mutation["last_seq"].is_u64());
    }
    for name in ["storage-observations.jsonl", "clean-controls.jsonl"] {
        for line in outcome.family_artifact_bytes[name]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line)
                    .unwrap_or_else(|error| panic!("parse {name}: {error}"));
            if name == "storage-observations.jsonl" {
                assert!(
                    record["expected"].is_object(),
                    "expected retained Debug text"
                );
                assert!(
                    record["observed"].is_object(),
                    "observed retained Debug text"
                );
                assert!(
                    record["operation_detail"].is_object(),
                    "operation detail retained Debug text"
                );
                match record["operation"].as_str() {
                    Some("publication") => {
                        for side in ["old", "new"] {
                            for segment in record["expected"][side]["segments"]
                                .as_array()
                                .expect("publication model segments")
                            {
                                assert!(segment["file_length"].is_u64());
                                assert!(segment["header_checksum"].is_u64());
                                assert!(segment["whole_file_checksum"].is_u64());
                            }
                        }
                    }
                    Some("wal-prefix") => {
                        assert!(record["observed"]["clean_public"].is_object());
                    }
                    Some("retry") => {
                        assert!(record["expected"]["ambiguous_first"].is_object());
                        assert!(record["observed"]["ambiguous_first"].is_object());
                        assert!(record["observed"]["active_same_handle"].is_object());
                    }
                    Some("format-check") => {
                        assert!(record["expected"]["clean"].is_object());
                        assert!(record["observed"]["clean"].is_object());
                    }
                    Some("orphan-cleanup") => {
                        assert!(record["expected"]["expected_directory_syncs"].is_u64());
                        assert!(
                            record["expected"]["committed_purge_read_only_required"]
                                .as_bool()
                                .is_some()
                        );
                        assert!(record["observed"]["committed_purge_read_only"].is_object());
                    }
                    Some(operation) => panic!("unexpected storage operation {operation}"),
                    None => panic!("storage observation omitted operation"),
                }
            } else {
                assert!(record["control"].is_object(), "control retained Debug text");
            }
        }
    }
    for line in outcome.family_artifact_bytes["artifact-index.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse storage artifact index evidence");
        assert!(
            record["role"].as_str().is_some(),
            "artifact index omitted role"
        );
        assert!(
            record["path"].as_str().is_some(),
            "artifact index omitted path"
        );
        assert!(record["length"].is_u64(), "artifact index omitted length");
        assert!(
            record["digest"].as_str().is_some(),
            "artifact index omitted digest"
        );
        assert!(
            record["bytes_hex"].as_str().is_some(),
            "artifact index omitted retained bytes"
        );
        assert!(
            record["mutation"].is_null(),
            "artifact index accepted a mutation ledger row"
        );
    }
    for line in outcome
        .mutations_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse storage mutation evidence");
        if record["campaign"] == "storage-durability" {
            assert!(
                record["mutation"].is_object(),
                "mutation retained Debug text"
            );
        }
    }
    for line in outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse storage oracle evidence");
        assert!(
            record["expected"].is_object(),
            "storage oracle expected is opaque text"
        );
        assert!(
            record["observed"].is_object(),
            "storage oracle observed is opaque text"
        );
    }
    assert_eq!(
        adversarial::campaign::campaign_from_replay_metadata(&directory)
            .expect("storage replay metadata is complete"),
        campaign,
    );
}

#[cfg(unix)]
#[test]
fn storage_fixture_artifact_executes_literal_family_bytes() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("storage campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("storage retained-fixture root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("storage retained-fixture episode");
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["storage-fixture.json"],
        )
        .expect("parse storage fixture artifact");
    let operations = fixture["retained_operations"]
        .as_array()
        .expect("storage fixture omitted retained literal operations");
    assert_eq!(operations.len(), 5);
    for operation in operations {
        let retained = decode_storage_hex(
            operation["retained_fixture_hex"]
                .as_str()
                .expect("storage retained literal fixture bytes"),
        )
        .expect("decode retained storage fixture hex");
        let replayed = adversarial::storage_durability::run_storage_operation_from_fixture(
            &retained,
            adversarial::storage_durability::PUBLICATION_CHILD_TEST_NAME,
        )
        .expect("execute retained storage fixture");
        assert_eq!(replayed.operation().key(), operation["operation"]);
    }
}

#[test]
fn storage_oracle_i15_plant_is_rejected() {
    adversarial::storage_durability::tests::storage_oracle_i15_plant_is_rejected();
}

#[test]
fn storage_oracle_i16_plant_is_rejected() {
    adversarial::storage_durability::tests::storage_oracle_i16_plant_is_rejected();
}

#[test]
fn storage_oracle_i17_plant_is_rejected() {
    adversarial::storage_durability::tests::storage_oracle_i17_plant_is_rejected();
}

#[test]
fn storage_oracle_i18_plant_is_rejected() {
    adversarial::storage_durability::tests::storage_oracle_i18_plant_is_rejected();
}

#[test]
fn storage_oracle_i19_plant_is_rejected() {
    adversarial::storage_durability::tests::storage_oracle_i19_plant_is_rejected();
}

#[test]
fn metadata_campaign_rejects_generic_invariant_credit() {
    feature_campaign_registry_owns_exact_ranges_without_generic_credit();
}

#[test]
fn metadata_summary_requires_oracle_attestation() {
    metadata_summary_rejects_a_generic_feature_attestation_explicitly();
}

#[test]
fn ingest_retention_old_summary_without_oracle_attestation_is_rejected() {
    let error = validate_ingest_retention_oracle_attestation_shape(
        &zeppelin_embed_bench::harness_json::json!({}),
    )
    .expect_err("legacy generic ingest-retention summary was accepted");
    assert_eq!(
        error,
        "ingest-retention attestation missing: oracle_contract_version"
    );
    let config = RunConfig {
        campaign: CampaignKind::IngestRetention,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I20".to_owned(), 1),
            ("I21".to_owned(), 1),
            ("I22".to_owned(), 1),
            ("I23".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I20".to_owned(), 1),
            ("I21".to_owned(), 1),
            ("I22".to_owned(), 1),
            ("I23".to_owned(), 1),
        ]),
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let stream = |records| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{records:016x}"),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: BTreeMap::from([
            ("program".to_owned(), stream(1)),
            ("faults".to_owned(), stream(0)),
            ("violations".to_owned(), stream(1)),
            ("coverage".to_owned(), stream(1)),
            ("oracle".to_owned(), stream(4)),
            ("controls".to_owned(), stream(0)),
            ("receipts".to_owned(), stream(0)),
            ("mutations".to_owned(), stream(0)),
        ]),
    };
    let summary = campaign_attestation_json(&config, 1, &counters, Some(merged), None)
        .expect("ingest-retention campaign owns an attestation");

    assert!(
        summary["ingest_retention_oracle_attestation"].is_object(),
        "unattested ingest-retention summary was accepted"
    );
    validate_ingest_retention_oracle_attestation_shape(
        &summary["ingest_retention_oracle_attestation"],
    )
    .expect("generated ingest-retention summary has strict family attestation");
}

#[test]
fn ingest_retention_replay_compares_oracle_controls_and_receipts() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..24)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault.key() == "post-ack-retry")
        })
        .expect("ingest-retention replay seed has a same-seed control and receipt");
    let root = tempfile::tempdir().expect("ingest-retention replay root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("retained ingest-retention episode");
    let directory = episode_artifact_directory(root.path(), campaign, seed, FaultProfile::None);
    assert_eq!(
        replay_ingest_retained_episode(&directory).expect("literal retained ingest replay"),
        replay_evidence_digest(&outcome)
    );

    let oracle_path = directory.join("oracle.jsonl");
    let oracle_bytes = std::fs::read(&oracle_path).expect("read retained ingest oracle");
    let mut oracle_lines = oracle_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .expect("parse retained ingest oracle row")
        })
        .collect::<Vec<_>>();
    let i22 = oracle_lines
        .iter_mut()
        .find(|record| {
            record["checker_id"].as_str()
                == Some(zeppelin_embed_adversarial_oracle::ingest_retention::I22_CHECKER_ID)
        })
        .expect("retained ingest oracle includes I22");
    let mut observed = decode_storage_hex(
        i22["oracle_observed_bytes"]
            .as_str()
            .expect("retained I22 observed bytes"),
    )
    .expect("decode retained I22 observed bytes");
    let domain = b"ingest-retention/I22/observed/v1";
    let cutoff_byte = 8_usize
        .checked_add(domain.len())
        .and_then(|offset| offset.checked_add(8))
        .expect("I22 cutoff offset");
    observed[cutoff_byte] ^= 1;
    let observed_hex = observed
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    i22["oracle_observed_bytes"] = zeppelin_embed_bench::harness_json::Value::String(observed_hex);
    i22["oracle_observed_digest"] = zeppelin_embed_bench::harness_json::Value::String(format!(
        "ingest-v{}:{:016x}",
        zeppelin_embed_adversarial_oracle::ingest_retention::INGEST_CANONICAL_VERSION,
        zeppelin_embed_adversarial_oracle::ingest_retention::canonical_digest(&observed),
    ));
    let mut mutated_oracle = Vec::new();
    for line in &oracle_lines {
        mutated_oracle.extend_from_slice(
            &zeppelin_embed_bench::harness_json::to_vec(line)
                .expect("serialize mutated ingest oracle row"),
        );
        mutated_oracle.push(b'\n');
    }
    std::fs::write(&oracle_path, mutated_oracle).expect("write I22 cutoff-byte drift");
    let error = replay_ingest_retained_episode(&directory)
        .expect_err("replay accepted I22 observed cutoff-byte drift");
    assert!(error.contains("artifact=oracle.jsonl"), "{error}");
    std::fs::write(&oracle_path, &oracle_bytes).expect("restore retained ingest oracle");

    let controls_path = directory.join("controls.jsonl");
    let controls_bytes = std::fs::read(&controls_path).expect("read retained ingest controls");
    let controls_drift = String::from_utf8(controls_bytes.clone())
        .expect("retained ingest controls UTF-8")
        .replacen("clean_initial_digest", "clean_initial_drift", 1);
    std::fs::write(&controls_path, controls_drift).expect("write ingest control drift");
    let error = replay_ingest_retained_episode(&directory)
        .expect_err("replay accepted controls.jsonl drift");
    assert!(error.contains("artifact=controls.jsonl"), "{error}");
    std::fs::write(&controls_path, &controls_bytes).expect("restore retained ingest controls");

    let receipts_path = directory.join("receipts.jsonl");
    let receipts_bytes = std::fs::read(&receipts_path).expect("read retained ingest receipts");
    std::fs::write(&receipts_path, b"").expect("delete retained ingest receipt");
    let error = replay_ingest_retained_episode(&directory)
        .expect_err("replay accepted deleted receipts.jsonl record");
    assert!(error.contains("artifact=receipts.jsonl"), "{error}");
    std::fs::write(&receipts_path, receipts_bytes).expect("restore retained ingest receipts");
}

#[test]
fn ingest_retention_full_profile_replays_two_distinct_faults_on_one_operation() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..64)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            let plan = FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::Full,
                &program,
                FaultSchedule::default(),
            );
            plan.feature.len() == 2
                && plan.feature[0].fault.operation() == plan.feature[1].fault.operation()
        })
        .expect("ingest-retention Full profile reaches two faults on one operation");
    let program = Program::generate_for(campaign, seed);
    let plan = FaultPlan::for_program(
        campaign,
        seed,
        FaultProfile::Full,
        &program,
        FaultSchedule::default(),
    );
    assert_ne!(plan.feature[0].fault, plan.feature[1].fault);
    assert_eq!(
        plan.feature[0].fault.operation(),
        plan.feature[1].fault.operation()
    );

    let root = tempfile::tempdir().expect("ingest-retention Full replay root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::Full, root.path())
            .expect("retained Full ingest-retention episode");
    let directory = episode_artifact_directory(root.path(), campaign, seed, FaultProfile::Full);

    assert_eq!(
        replay_ingest_retained_episode(&directory)
            .expect("literal retained Full ingest replay with same-operation faults"),
        replay_evidence_digest(&outcome)
    );
}

#[test]
fn ingest_retention_oracle_rows_bind_operation_fault_invocation_identity() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..64)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            let plan = FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::Full,
                &program,
                FaultSchedule::default(),
            );
            plan.feature.len() == 2
                && plan.feature[0].fault.operation() == plan.feature[1].fault.operation()
        })
        .expect("ingest-retention identity seed reaches same-operation faults");
    let root = tempfile::tempdir().expect("ingest-retention case-identity root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::Full, root.path())
        .expect("ingest-retention case-identity episode");
    let directory = episode_artifact_directory(root.path(), campaign, seed, FaultProfile::Full);
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(directory.join("fixture.json"))
                .expect("read ingest case-identity fixture"),
        )
        .expect("parse ingest case-identity fixture");
    let fixture_cases = fixture["operations"]
        .as_array()
        .expect("ingest fixture operation cases")
        .iter()
        .map(|record| {
            record["case_identity"]
                .as_str()
                .expect("ingest fixture omitted case identity")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let oracle_cases = std::fs::read(directory.join("oracle.jsonl"))
        .expect("read ingest case-identity oracle")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line)
                    .expect("parse ingest case-identity oracle row");
            record["case_identity"]
                .as_str()
                .expect("ingest oracle omitted case identity")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(fixture_cases.len(), 5);
    assert_eq!(oracle_cases, fixture_cases);
}

#[test]
fn ingest_retention_attestation_rejects_receipt_credit_absent_from_merged_evidence() {
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::IngestRetention).all_required_coverage() {
        coverage.hit(key);
    }
    let comparisons = BTreeMap::from([
        ("I20".to_owned(), 1),
        ("I21".to_owned(), 1),
        ("I22".to_owned(), 1),
        ("I23".to_owned(), 1),
    ]);
    let counters = CampaignAttestationCounters {
        comparison_counts: comparisons.clone(),
        comparison_pass_counts: comparisons.clone(),
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let ingest =
        ingest_retention_oracle_attestation_json(1, &counters, &BTreeMap::new(), Some(&coverage));
    let pairs = CampaignSpec::for_kind(CampaignKind::IngestRetention)
        .feature_faults
        .iter()
        .map(|fault| (fault.key().to_owned(), 1_u64))
        .collect::<BTreeMap<_, _>>();
    let mut receipts = pairs.clone();
    receipts.insert("post-ack-retry".to_owned(), 0);
    let sites = ingest["receipt_sites"]
        .as_object()
        .expect("ingest receipt-site attestation")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_u64().unwrap_or(0)))
        .collect::<BTreeMap<_, _>>();
    let ledgers = IngestRetentionMergedLedgers {
        operations: BTreeMap::from([
            ("batch-commit".to_owned(), 1),
            ("seal".to_owned(), 1),
            ("retention".to_owned(), 1),
            ("purge".to_owned(), 1),
        ]),
        fault_pairs: pairs.clone(),
        same_seed_controls: pairs,
        production_receipts: receipts,
        receipt_sites: sites,
        retained_fixtures: 4,
    };

    let error = validate_ingest_retention_observed_ledgers(
        &ingest,
        &comparisons,
        &comparisons,
        &ledgers,
        1,
    )
    .expect_err("ingest attestation accepted fabricated production-receipt credit");
    assert!(
        error.contains("production receipt ledger mismatch"),
        "{error}"
    );
}

#[test]
fn ingest_retention_merged_ledger_is_derived_from_retained_rows() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..24)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            !FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention merged-ledger seed selects a feature fault");
    let root = tempfile::tempdir().expect("ingest-retention merged-ledger root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("ingest-retention merged-ledger episode");
    let mut merged = adversarial::artifacts::MergedEvidence::create(root.path())
        .expect("create ingest-retention merged evidence");
    merged
        .append_episode(
            outcome.seed,
            outcome.profile,
            &outcome.program_bytes,
            &outcome.faults_bytes,
            &outcome.violations_bytes,
            &outcome.coverage_bytes,
            &outcome.oracle_bytes,
            &outcome.controls_bytes,
            &outcome.receipts_bytes,
            &outcome.mutations_bytes,
            &outcome.family_artifact_bytes,
        )
        .expect("append ingest-retention merged evidence");
    let ledgers = read_ingest_retention_merged_ledgers(root.path())
        .expect("derive ingest-retention ledgers from merged rows");
    assert_eq!(ledgers.fault_pairs.values().sum::<u64>(), 1);
    assert_eq!(ledgers.fault_pairs, ledgers.same_seed_controls);
    assert_eq!(ledgers.fault_pairs, ledgers.production_receipts);
    assert_eq!(ledgers.retained_fixtures, 4);

    std::fs::write(root.path().join("merged-receipts.jsonl"), b"")
        .expect("delete merged ingest receipt");
    let error = read_ingest_retention_merged_ledgers(root.path())
        .expect_err("missing merged ingest receipt still earned credit");
    assert!(
        error.contains("production receipt ledger mismatch"),
        "{error}"
    );
}

#[test]
fn ingest_retention_receipt_checksum_rejects_effect_drift() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..64)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .first()
            .is_some_and(|event| event.fault.key() == "retention-clock-boundary")
        })
        .expect("ingest-retention checksum seed selects retention-clock-boundary");
    let root = tempfile::tempdir().expect("ingest-retention receipt-checksum root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("ingest-retention receipt-checksum episode");
    let mut merged = adversarial::artifacts::MergedEvidence::create(root.path())
        .expect("create ingest-retention receipt-checksum merged evidence");
    merged
        .append_episode(
            outcome.seed,
            outcome.profile,
            &outcome.program_bytes,
            &outcome.faults_bytes,
            &outcome.violations_bytes,
            &outcome.coverage_bytes,
            &outcome.oracle_bytes,
            &outcome.controls_bytes,
            &outcome.receipts_bytes,
            &outcome.mutations_bytes,
            &outcome.family_artifact_bytes,
        )
        .expect("append ingest-retention receipt-checksum evidence");
    read_ingest_retention_merged_ledgers(root.path())
        .expect("unaltered ingest-retention receipt checksum");

    let path = root.path().join("merged-receipts.jsonl");
    let bytes = std::fs::read(&path).expect("read merged ingest receipts");
    let mut lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .expect("parse merged ingest receipt")
        })
        .collect::<Vec<_>>();
    let cutoff = lines[0]["record"]["effect"]["cutoff"]
        .as_i64()
        .expect("retention receipt cutoff");
    lines[0]["record"]["effect"]["cutoff"] =
        zeppelin_embed_bench::harness_json::Value::from(cutoff.saturating_add(1));
    let mut mutated = Vec::new();
    for line in &lines {
        mutated.extend_from_slice(
            &zeppelin_embed_bench::harness_json::to_vec(line)
                .expect("serialize mutated ingest receipt"),
        );
        mutated.push(b'\n');
    }
    std::fs::write(&path, mutated).expect("write mutated ingest receipt");

    let error = read_ingest_retention_merged_ledgers(root.path())
        .expect_err("ingest receipt effect drift kept valid receipt credit");
    assert!(error.contains("receipt checksum"), "{error}");
}

#[test]
fn ingest_retention_observations_are_typed_canonical_records() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..32)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention typed-observation seed is clean");
    let root = tempfile::tempdir().expect("ingest-retention typed-observation root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
        .expect("ingest-retention typed-observation episode");
    let directory = episode_artifact_directory(root.path(), campaign, seed, FaultProfile::None);
    let bytes = std::fs::read(directory.join("observations.jsonl"))
        .expect("read retained ingest typed observations");
    let records = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .expect("parse retained ingest typed observation")
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 4);
    for record in records {
        assert_eq!(
            record["canonical_version"].as_u64(),
            Some(
                zeppelin_embed_adversarial_oracle::ingest_retention::INGEST_CANONICAL_VERSION
                    as u64
            ),
            "ingest observation omitted its canonical version"
        );
        let checker_id = record["checker_id"]
            .as_str()
            .expect("ingest observation omitted checker ID");
        let input = decode_storage_hex(
            record["oracle_input_bytes"]
                .as_str()
                .expect("ingest observation omitted canonical input bytes"),
        )
        .expect("decode ingest observation input bytes");
        let observed = decode_storage_hex(
            record["oracle_observed_bytes"]
                .as_str()
                .expect("ingest observation omitted canonical observed bytes"),
        )
        .expect("decode ingest observation observed bytes");
        let replay =
            zeppelin_embed_adversarial_oracle::ingest_retention::replay_canonical_comparison(
                checker_id, &input, &observed,
            )
            .expect("replay typed ingest observation");
        assert!(replay.first_difference.is_none());
        assert!(record["expected"].is_null());
        assert!(record["observed"].is_null());
        assert!(record["control"].is_null());
        assert!(record["receipts"].is_null());
    }
}

#[test]
fn ingest_retention_final_verifier_rejects_fabricated_receipt_credit() {
    let campaign = CampaignKind::IngestRetention;
    let seed = (0..24)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            !FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("ingest-retention verifier seed selects a feature fault");
    let artifacts = tempfile::tempdir().expect("ingest-retention verifier artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", campaign.key())
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", seed.to_string())
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run ingest-retention verifier campaign");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json")).unwrap_or_else(
                |error| panic!("missing ingest campaign summary: {error}; {transcript}"),
            ),
        )
        .expect("parse ingest campaign summary");
    let selected_fault = summary["attestation"]["ingest_retention_oracle_attestation"]["faults"]
        .as_object()
        .expect("ingest fault attestation")
        .iter()
        .find_map(|(fault, ledger)| {
            (ledger["same_seed_pairs"].as_u64() == Some(1)).then_some(fault.clone())
        })
        .unwrap_or_else(|| panic!("one-episode campaign selected no ingest fault: {transcript}"));
    summary["attestation"]["ingest_retention_oracle_attestation"]["faults"][&selected_fault]["production_receipts"] =
        zeppelin_embed_bench::harness_json::json!(0);

    let error = verify_feature_summary_attestation(artifacts.path(), campaign, 1, &summary)
        .expect_err("final verifier accepted fabricated ingest receipt credit");
    assert!(
        error.contains("production receipt ledger mismatch"),
        "{error}"
    );
}

#[test]
fn ingest_retention_oracle_rows_replay_the_family_canonical_bytes() {
    let campaign = CampaignKind::IngestRetention;
    let root = tempfile::tempdir().expect("ingest-retention canonical-row root");
    let outcome =
        adversarial::runner::run_program_for(campaign, 0, FaultProfile::None, root.path())
            .expect("ingest-retention canonical-row episode");
    let rows = outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .expect("parse ingest-retention oracle row")
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);
    for row in rows {
        let invariant = validate_feature_oracle_record(campaign, &row)
            .expect("validate ingest-retention family oracle row");
        let input = decode_storage_hex(
            row["oracle_input_bytes"]
                .as_str()
                .expect("ingest oracle input bytes"),
        )
        .expect("decode ingest oracle input bytes");
        let observed = decode_storage_hex(
            row["oracle_observed_bytes"]
                .as_str()
                .expect("ingest oracle observed bytes"),
        )
        .expect("decode ingest oracle observed bytes");
        assert!(
            !input.is_empty(),
            "{invariant} omitted retained input bytes"
        );
        assert!(
            !observed.is_empty(),
            "{invariant} omitted retained observed bytes"
        );
        let replay =
            zeppelin_embed_adversarial_oracle::ingest_retention::replay_canonical_comparison(
                row["checker_id"].as_str().expect("ingest checker id"),
                &input,
                &observed,
            )
            .expect("replay ingest-retention canonical row");
        assert!(replay.first_difference.is_none(), "{replay:?}");
    }
}

#[test]
fn column_corruption_requires_decoder_receipt() {
    adversarial::metadata_filter_planner::tests::column_corruption_requires_decoder_receipt();
}

#[test]
fn bitmap_truncation_requires_short_alive_region() {
    adversarial::metadata_filter_planner::tests::bitmap_truncation_requires_short_alive_region();
}

#[test]
fn selectivity_boundary_straddles_production_threshold() {
    adversarial::metadata_filter_planner::tests::selectivity_boundary_straddles_production_threshold();
}

#[test]
fn visited_budget_fault_executes_query_fallback() {
    adversarial::metadata_filter_planner::tests::visited_budget_fault_executes_query_fallback();
}

#[test]
fn vector_replay_declares_every_family_evidence_stream() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("vector campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("vector replay root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("vector replay episode");
    let directory = root
        .path()
        .join(format!("vector-execution/seed-{seed}-none"));

    for name in [
        "fixture.json",
        "backend-inventory.json",
        "quantization.jsonl",
        "rescore.jsonl",
        "identity.jsonl",
        "coverage.jsonl",
        "violations.jsonl",
        "episode-summary.json",
    ] {
        assert!(
            outcome.family_artifact_bytes.contains_key(name),
            "vector replay omitted {name}"
        );
        assert!(
            directory.join(name).is_file(),
            "vector episode did not write {name}"
        );
    }
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["fixture.json"],
        )
        .expect("parse canonical vector fixture");
    let operations = fixture["operations"]
        .as_array()
        .expect("vector fixture operations array");
    assert_eq!(operations.len(), 4);
    for operation in operations {
        assert!(operation["inputs"].is_array());
        assert!(!operation["inputs"].as_array().unwrap().is_empty());
        assert!(operation["documents"].is_array());
        assert!(operation["public_schedule"].is_array());
    }
    let inventory: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["backend-inventory.json"],
        )
        .expect("parse vector backend inventory");
    assert!(inventory["available"].is_array());
    assert!(inventory["unavailable"].is_array());
    assert!(inventory["selected"].is_array());
    assert!(inventory["invocation_counts"].is_object());
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&outcome.episode_bytes)
            .expect("parse vector episode summary");
    let vector_attestation = &episode["attestation"]["vector_oracle_attestation"];
    assert_eq!(
        vector_attestation["oracle_contract"],
        zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_ORACLE_CONTRACT
    );
    assert!(vector_attestation["fixture_digest"].is_string());
    assert!(vector_attestation["per_invariant_comparisons"].is_object());
    assert!(vector_attestation["same_seed_controls"].is_object());
    assert!(vector_attestation["integrated_receipts"].is_object());
    let features = vector_attestation["backend_inventory"]["features"]
        .as_object()
        .expect("vector backend feature booleans");
    for feature in ["neon", "dotprod", "fp16", "i8mm", "sme2", "avx2", "popcnt"] {
        assert!(
            features
                .get(feature)
                .is_some_and(|value| value.is_boolean()),
            "vector backend feature {feature} was not a detected boolean"
        );
    }
    for name in ["quantization.jsonl", "rescore.jsonl", "identity.jsonl"] {
        for line in outcome.family_artifact_bytes[name]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line)
                    .unwrap_or_else(|error| panic!("parse {name} record: {error}"));
            assert!(record["input"].is_object(), "{name} input is not typed");
            assert!(
                record["observed"].is_object(),
                "{name} observation is not typed"
            );
        }
    }
    for (name, bytes, field) in [
        (
            "controls.jsonl",
            outcome.controls_bytes.as_slice(),
            "control",
        ),
        (
            "mutations.jsonl",
            outcome.mutations_bytes.as_slice(),
            "mutation",
        ),
    ] {
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line)
                    .unwrap_or_else(|error| panic!("parse {name}: {error}"));
            if record["campaign"] == "vector-execution" {
                assert!(record[field].is_object(), "{name} retained debug text");
            }
        }
    }
    for line in outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse vector oracle evidence");
        assert!(
            record["expected"].is_object(),
            "oracle expected is opaque text"
        );
        assert!(
            record["observed"].is_object(),
            "oracle observed is opaque text"
        );
    }
    adversarial::campaign::campaign_from_replay_metadata(&directory)
        .expect("vector replay metadata rejected its family artifacts");
}

#[test]
fn vector_fixture_artifact_executes_literal_family_bytes() {
    let campaign = CampaignKind::VectorExecution;
    let root = tempfile::tempdir().expect("vector retained-fixture root");
    let outcome =
        adversarial::runner::run_program_for(campaign, 6, FaultProfile::None, root.path())
            .expect("vector retained-fixture episode");
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["fixture.json"],
        )
        .expect("parse vector fixture artifact");
    let operations = fixture["operations"]
        .as_array()
        .expect("vector fixture operations");
    assert_eq!(operations.len(), 4);
    for operation in operations {
        let retained_hex = operation["retained_fixture_hex"]
            .as_str()
            .expect("vector operation retained literal fixture bytes");
        let retained = decode_storage_hex(retained_hex).expect("decode retained vector fixture");
        let replayed = adversarial::vector_execution::run_vector_operation_from_fixture(&retained)
            .expect("execute retained vector fixture");
        assert_eq!(replayed.fixture.seed, 6);
        assert_eq!(
            replayed.comparisons.len(),
            replayed.fixture.comparisons.len()
        );
        assert!(
            replayed
                .comparisons
                .iter()
                .all(|comparison| comparison.first_difference.is_none()),
            "retained vector fixture failed an exact family checker"
        );
    }
}

#[test]
fn vector_retained_replay_rejects_a_mutated_literal_fixture() {
    let campaign = CampaignKind::VectorExecution;
    let root = tempfile::tempdir().expect("vector retained replay root");
    adversarial::runner::run_program_for(campaign, 6, FaultProfile::None, root.path())
        .expect("vector retained replay episode");
    let directory = root.path().join("vector-execution/seed-6-none");
    replay_vector_retained_episode(&directory).expect("replay retained vector fixture bytes");
    let fixture_path = directory.join("fixture.json");
    let mut fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(&fixture_path).expect("read retained vector fixture"),
        )
        .expect("parse retained vector fixture");
    let retained = fixture["operations"][0]["retained_fixture_hex"]
        .as_str()
        .expect("retained fixture hex");
    let mut planted = retained.as_bytes().to_vec();
    planted[16] = if planted[16] == b'0' { b'1' } else { b'0' };
    fixture["operations"][0]["retained_fixture_hex"] =
        zeppelin_embed_bench::harness_json::Value::String(
            String::from_utf8(planted).expect("planted fixture remains hex text"),
        );
    std::fs::write(
        &fixture_path,
        zeppelin_embed_bench::harness_json::to_vec(&fixture)
            .expect("serialize planted vector fixture"),
    )
    .expect("write planted vector fixture");

    let error = replay_vector_retained_episode(&directory)
        .expect_err("mutated retained vector fixture was accepted");
    assert!(error.contains("fixture"), "{error}");
}

#[test]
fn vector_oracle_record_rejects_a_stale_family_canonical_digest() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("vector campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("vector canonical record root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("vector canonical record episode");
    let line = outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .expect("vector episode emitted an oracle row");
    let mut record: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(line).expect("parse vector oracle row");
    record
        .as_object_mut()
        .expect("vector oracle row is an object")
        .insert(
            "oracle_input_digest".to_owned(),
            zeppelin_embed_bench::harness_json::Value::String(format!(
                "{}:{}",
                zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_CANONICAL_VERSION,
                "0".repeat(64)
            )),
        );

    let error = validate_feature_oracle_record(campaign, &record)
        .expect_err("stale vector family digest must be rejected");
    assert!(error.contains("family canonical attestation is stale"));
}

#[test]
fn vector_replay_executes_the_retained_independent_checker() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("vector campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("vector retained checker root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("vector retained checker episode");
    let line = outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .expect("vector episode emitted an oracle row");
    let mut record: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(line).expect("parse vector oracle row");
    let mut observed = decode_storage_hex(
        record["oracle_observed_bytes"]
            .as_str()
            .expect("retained vector observed bytes"),
    )
    .expect("decode retained vector observed bytes");
    let last = observed.last_mut().expect("nonempty canonical observation");
    *last ^= 1;
    let observed_hex = observed
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let observed_digest =
        zeppelin_embed_adversarial_oracle::vector_execution::canonical_sha256(&observed)
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
    let object = record.as_object_mut().expect("vector oracle row object");
    object.insert(
        "oracle_observed_bytes".to_owned(),
        zeppelin_embed_bench::harness_json::Value::String(observed_hex),
    );
    object.insert(
        "oracle_observed_digest".to_owned(),
        zeppelin_embed_bench::harness_json::Value::String(format!(
            "{}:{observed_digest}",
            zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_CANONICAL_VERSION,
        )),
    );

    let error = validate_feature_oracle_record(campaign, &record)
        .expect_err("digest-valid mutated vector observation bypassed retained checker replay");
    assert!(error.contains("retained vector checker replay"), "{error}");
}

#[test]
fn metadata_replay_executes_the_retained_independent_checker() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("metadata campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("metadata retained checker root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("metadata canonical record episode");
    let line = outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .find(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .is_ok_and(|record| record["invariant"] == "I36")
        })
        .expect("metadata episode emitted an I36 oracle row");
    let mut record: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(line).expect("parse metadata oracle row");
    let mut observed = decode_storage_hex(
        record["oracle_observed_bytes"]
            .as_str()
            .expect("retained metadata observed bytes"),
    )
    .expect("decode retained metadata observed bytes");
    let last = observed
        .last_mut()
        .expect("nonempty canonical metadata observation");
    *last ^= 1;
    let observed_hex = observed
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let observed_digest =
        zeppelin_embed_adversarial_oracle::metadata_filter_planner::canonical_digest(&observed);
    let object = record.as_object_mut().expect("metadata oracle row object");
    object.insert(
        "oracle_observed_bytes".to_owned(),
        zeppelin_embed_bench::harness_json::Value::String(observed_hex),
    );
    object.insert(
        "oracle_observed_digest".to_owned(),
        zeppelin_embed_bench::harness_json::Value::String(format!(
            "metadata-v1:{observed_digest:016x}",
        )),
    );

    let error = validate_feature_oracle_record(campaign, &record)
        .expect_err("digest-valid mutated metadata observation bypassed retained checker replay");
    assert!(
        error.contains("retained metadata checker replay"),
        "{error}"
    );
}

#[test]
fn vector_replay_metadata_rejects_a_stale_nested_canonical_contract() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("vector campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("vector nested attestation root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
        .expect("vector nested attestation episode");
    let directory = root
        .path()
        .join(format!("vector-execution/seed-{seed}-none"));
    let path = directory.join("episode.json");
    let mut episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(&path).expect("read vector episode metadata"),
        )
        .expect("parse vector episode metadata");
    episode["attestation"]["vector_oracle_attestation"]["per_invariant_comparisons"]["I24"]["canonical_version"] =
        zeppelin_embed_bench::harness_json::json!("stale");
    std::fs::write(
        &path,
        zeppelin_embed_bench::harness_json::to_vec_pretty(&episode)
            .expect("serialize planted vector episode metadata"),
    )
    .expect("write planted vector episode metadata");

    let error = adversarial::campaign::campaign_from_replay_metadata(&directory)
        .expect_err("stale nested vector canonical contract must be rejected");
    assert!(error.contains("independent-oracle attestation"), "{error}");
}

#[test]
fn metadata_replay_declares_fixture_and_query_evidence() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("metadata campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("metadata replay root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("metadata replay episode");
    let directory = root
        .path()
        .join(format!("metadata-filter-planner/seed-{seed}-none"));

    for name in [
        "metadata-fixture.json",
        "queries.jsonl",
        "fixture-mutations.jsonl",
    ] {
        assert!(
            outcome.family_artifact_bytes.contains_key(name),
            "metadata replay omitted {name}"
        );
        assert!(
            directory.join(name).is_file(),
            "metadata replay did not write {name}"
        );
    }
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["metadata-fixture.json"],
        )
        .expect("parse metadata fixture evidence");
    let operations = fixture["operations"]
        .as_array()
        .expect("metadata fixture operations");
    let bitmap_cases =
        usize::try_from(adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT)
            .expect("I37 predicate case count fits usize");
    assert_eq!(operations.len(), bitmap_cases + 3);
    for (operation, expected) in [
        ("metadata_columns_roundtrip", 1),
        ("metadata_bitmap_algebra", bitmap_cases),
        ("metadata_pruning_soundness", 1),
        ("metadata_execution_truth", 1),
    ] {
        assert_eq!(
            operations
                .iter()
                .filter(|record| record["operation"] == operation)
                .count(),
            expected,
            "metadata replay fixture count for {operation}",
        );
    }
    for operation in operations {
        assert!(
            operation["fixture"].is_object(),
            "fixture retained Debug text"
        );
        assert!(operation["fixture"]["kind"].as_str().is_some());
        assert!(
            operation["fixture"]["payload"].is_object()
                || operation["fixture"]["payload"].is_array()
        );
    }
    for line in outcome.family_artifact_bytes["queries.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let query: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse metadata query evidence");
        assert!(
            query["predicate"].is_object(),
            "predicate retained Debug text"
        );
        assert!(query["predicate"]["kind"].as_str().is_some());
        assert!(query["expected_sources"].is_array());
    }
    for line in outcome
        .controls_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let control: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse metadata control evidence");
        if control["campaign"] == "metadata-filter-planner" {
            assert!(
                control["control"].is_object(),
                "control retained Debug text"
            );
            assert!(control["control"]["clean_results"].is_array());
            assert!(control["control"]["clean_initial_directory"].is_object());
            assert!(
                control["control"]["normalized_schedule_digest"]
                    .as_u64()
                    .is_some_and(|digest| digest != 0),
                "metadata control omitted its normalized schedule digest"
            );
            assert!(
                control["control"]["directory_relation"].as_str().is_some(),
                "metadata control omitted its typed directory relation"
            );
            assert!(
                control["control"]["outcome"].as_str().is_some(),
                "metadata control omitted its typed public outcome"
            );
        }
    }
    for line in outcome
        .oracle_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .expect("parse metadata oracle evidence");
        assert!(record["expected"].is_object() || record["expected"].is_array());
        assert!(
            record["observed"].is_object(),
            "metadata oracle observed is opaque text"
        );
    }
    let fixture_mutations = outcome
        .family_artifact_bytes
        .get("fixture-mutations.jsonl")
        .expect("metadata fixture mutation stream")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .expect("parse metadata fixture mutation")
        })
        .collect::<Vec<_>>();
    assert_eq!(fixture_mutations.len(), 80);
    for (ordinal, record) in fixture_mutations.iter().enumerate() {
        assert_eq!(record["role"], "fixture-preparation");
        assert_eq!(record["ordinal"], ordinal as u64);
        assert!(record["mutation"].is_object());
        assert!(record["mutation"]["region"]["code"].is_u64());
        assert!(record["mutation"]["absolute_offset"].is_u64());
        assert!(record["mutation"]["before_hex"].is_string());
        assert!(record["mutation"]["after_hex"].is_string());
        assert!(
            record["mutation"]["checksum_rewrites"]
                .as_array()
                .is_some_and(|rewrites| !rewrites.is_empty()),
            "fixture mutation omitted its checksum rewrite ledger"
        );
        assert!(
            record["mutation"]["post_mutation_artifact_digest"]
                .as_u64()
                .is_some_and(|digest| digest != 0),
            "fixture mutation omitted its post-mutation artifact digest"
        );
    }
    adversarial::campaign::campaign_from_replay_metadata(&directory)
        .expect("metadata replay metadata rejected its family artifacts");
}

#[test]
fn metadata_fixture_artifact_executes_literal_family_bytes() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("metadata campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("metadata retained-fixture root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("metadata retained-fixture episode");
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &outcome.family_artifact_bytes["metadata-fixture.json"],
        )
        .expect("parse metadata fixture artifact");
    let operations = fixture["operations"]
        .as_array()
        .expect("metadata fixture operations");
    let bitmap_cases =
        usize::try_from(adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT)
            .expect("metadata Bitmap case count fits usize");
    assert_eq!(operations.len(), bitmap_cases + 3);
    for operation in operations {
        let retained = decode_storage_hex(
            operation["retained_fixture_hex"]
                .as_str()
                .expect("metadata operation retained literal fixture bytes"),
        )
        .expect("decode retained metadata fixture hex");
        let replayed =
            adversarial::metadata_filter_planner::run_metadata_operation_from_fixture(&retained)
                .expect("execute retained metadata fixture");
        assert!(
            replayed.replay.first_difference.is_none(),
            "retained metadata fixture failed its exact family checker"
        );
        let expected_operation = match replayed.operation {
            adversarial::metadata_filter_planner::MetadataOperationKind::Columns => {
                "metadata_columns_roundtrip"
            }
            adversarial::metadata_filter_planner::MetadataOperationKind::Bitmap => {
                "metadata_bitmap_algebra"
            }
            adversarial::metadata_filter_planner::MetadataOperationKind::Planner => {
                "metadata_pruning_soundness"
            }
            adversarial::metadata_filter_planner::MetadataOperationKind::Execution => {
                "metadata_execution_truth"
            }
        };
        assert_eq!(operation["operation"], expected_operation);
    }
}

#[test]
fn metadata_replay_executes_retained_literal_fixture_without_program_regeneration() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("metadata campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("metadata retained replay root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
        .expect("metadata retained replay episode");
    let directory = root
        .path()
        .join(format!("metadata-filter-planner/seed-{seed}-none"));
    replay_metadata_retained_episode(&directory)
        .expect("replay retained metadata product fixtures");
}

#[test]
fn metadata_retained_replay_rejects_a_mutated_same_seed_control_digest() {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("metadata campaign has a clean feature-fault slot");
    let root = tempfile::tempdir().expect("metadata control replay root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
        .expect("metadata control replay episode");
    let directory = root
        .path()
        .join(format!("metadata-filter-planner/seed-{seed}-none"));
    let path = directory.join("controls.jsonl");
    let mut records = std::fs::read(&path)
        .expect("read retained metadata controls")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .expect("parse retained metadata control")
        })
        .collect::<Vec<_>>();
    let control = records
        .iter_mut()
        .find(|record| record["campaign"] == campaign.key())
        .expect("metadata control record");
    let digest = control["control"]["normalized_schedule_digest"]
        .as_u64()
        .expect("metadata normalized schedule digest");
    control["control"]["normalized_schedule_digest"] =
        zeppelin_embed_bench::harness_json::json!(digest ^ 1);
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend_from_slice(
            &zeppelin_embed_bench::harness_json::to_vec(&record)
                .expect("serialize planted metadata control"),
        );
        bytes.push(b'\n');
    }
    std::fs::write(&path, bytes).expect("write planted metadata controls");

    let error = replay_metadata_retained_episode(&directory)
        .expect_err("mutated metadata same-seed control was accepted");
    assert!(error.contains("operation_evidence"), "{error}");
}

#[test]
fn metadata_replay_self_test_rejects_every_independent_stream_mutation() {
    let mismatches = adversarial::runner::metadata_replay_mutation_self_test()
        .expect("metadata replay accepted an independently mutated evidence stream");
    assert_eq!(
        mismatches,
        vec![
            "oracle.jsonl",
            "receipts.jsonl",
            "fixture-mutations.jsonl",
            "controls.jsonl",
        ],
        "metadata replay mutations were not rejected with their exact artifact identities"
    );
}

#[test]
fn metadata_verifier_rejects_a_zero_normalized_schedule_digest() {
    let directory = zeppelin_embed_bench::harness_json::json!({
        "digest": 17,
        "files": [{"relative_path": "wal.ze", "byte_length": 8, "digest": 19}],
    });
    let control = zeppelin_embed_bench::harness_json::json!({
        "normalized_schedule_digest": 0,
        "directory_relation": "distinct-byte-identical",
        "outcome": "clean-succeeded-fault-refused-retry-equivalent",
        "clean_results": [{"source":"active","row_id":0,"document_id":null,"score_bits":0}],
        "fault_results": [],
        "retry_results": [{"source":"active","row_id":0,"document_id":null,"score_bits":0}],
        "independent_expected_results": [{"source":"active","row_id":0,"document_id":null,"score_bits":0}],
        "fault_error": "typed refusal",
        "clean_generation": 1,
        "fault_generation": 1,
        "retry_generation": 1,
        "clean_initial_directory": directory.clone(),
        "fault_initial_directory": directory,
    });
    let error = validate_metadata_control_evidence(&control)
        .expect_err("zero metadata schedule digest was accepted");
    assert!(error.contains("normalized schedule digest"), "{error}");
}

#[test]
fn metadata_verifier_rejects_an_empty_checksum_rewrite_ledger() {
    let mutation = zeppelin_embed_bench::harness_json::json!({
        "source": "sealed-a",
        "region": {"code": 1, "name": "columns"},
        "region_offset": 64,
        "field_offset": 4,
        "absolute_offset": 68,
        "before_hex": "00",
        "after_hex": "01",
        "left_neighbor_before": null,
        "left_neighbor_after": null,
        "right_neighbor_before": null,
        "right_neighbor_after": null,
        "declared_bytes_before": 1,
        "declared_bytes_after": 1,
        "observed_bytes_after": 1,
        "checksum_rewrites": [],
        "post_mutation_artifact_digest": 23,
    });
    let error = validate_metadata_mutation_evidence(&mutation)
        .expect_err("empty metadata checksum rewrite ledger was accepted");
    assert!(
        error.contains("checksum rewrite ledger is empty"),
        "{error}"
    );
}

#[test]
fn metadata_merged_verifier_rejects_a_zero_schedule_digest() {
    let root = tempfile::tempdir().expect("metadata merged evidence root");
    std::fs::write(root.path().join("merged-faults.jsonl"), b"")
        .expect("write empty merged faults");
    std::fs::write(root.path().join("merged-receipts.jsonl"), b"")
        .expect("write empty merged receipts");
    std::fs::write(root.path().join("merged-mutations.jsonl"), b"")
        .expect("write empty merged mutations");
    std::fs::write(
        root.path()
            .join("merged-family-fixture-mutations.jsonl.jsonl"),
        b"",
    )
    .expect("write empty merged fixture mutations");
    let row = zeppelin_embed_bench::harness_json::json!({
        "record": {
            "campaign": "metadata-filter-planner",
            "control": {
                "normalized_schedule_digest": 0,
                "directory_relation": "unpaired-single-directory",
                "outcome": "clean-only",
                "clean_results": [],
                "fault_results": [],
                "retry_results": [],
                "independent_expected_results": [],
                "clean_initial_directory": {"digest": 1, "files": [
                    {"relative_path":"wal.ze","byte_length":0,"digest":1}
                ]},
                "fault_initial_directory": {"digest":0,"files":[]}
            }
        }
    });
    std::fs::write(
        root.path().join("merged-controls.jsonl"),
        format!("{row}\n"),
    )
    .expect("write planted metadata control");

    let error = read_metadata_merged_ledgers(root.path())
        .expect_err("merged verifier ignored a zero metadata schedule digest");
    assert!(error.contains("normalized schedule digest"), "{error}");
}

#[test]
fn storage_merged_verifier_rejects_a_missing_production_receipt() {
    let root = tempfile::tempdir().expect("storage merged evidence root");
    let fault = zeppelin_embed_bench::harness_json::json!({
        "seed": 7,
        "profile": "none",
        "record": {
            "type": "feature",
            "campaign": "storage-durability",
            "key": "torn-wal-header",
            "op": 3,
            "fired": true,
            "fire_count": 1
        }
    });
    std::fs::write(
        root.path().join("merged-faults.jsonl"),
        format!("{fault}\n"),
    )
    .expect("write selected storage fault");
    let control = zeppelin_embed_bench::harness_json::json!({
        "seed": 7,
        "profile": "none",
        "record": {
            "campaign": "storage-durability",
            "operation": "wal-prefix",
            "seed": 7,
            "control": {
                "namespace": "storage-durability-v1",
                "seed": 7,
                "operation_fixture_id": "fixture-7-wal-prefix",
                "clean_fault_pair_id": "pair-7-wal-prefix",
                "pre_clean_inventory": [],
                "pre_fault_inventory": [],
                "pre_clean_digest": "00",
                "pre_fault_digest": "00",
                "pre_clean_artifacts": [],
                "pre_fault_artifacts": []
            }
        }
    });
    std::fs::write(
        root.path().join("merged-controls.jsonl"),
        format!("{control}\n"),
    )
    .expect("write storage same-seed control");
    std::fs::write(root.path().join("merged-receipts.jsonl"), b"")
        .expect("write missing storage receipt stream");
    std::fs::write(
        root.path().join("merged-family-artifact-index.jsonl.jsonl"),
        b"",
    )
    .expect("write empty storage artifact index");

    let error = read_storage_merged_ledgers(root.path())
        .expect_err("storage verifier accepted a fired fault without a production receipt");
    assert!(
        error.contains("production receipt ledger mismatch"),
        "{error}"
    );
}

#[test]
fn storage_merged_verifier_rejects_a_mutated_receipt_digest() {
    let root = tempfile::tempdir().expect("storage merged receipt root");
    let fault = zeppelin_embed_bench::harness_json::json!({
        "seed": 7,
        "profile": "none",
        "record": {
            "type": "feature",
            "campaign": "storage-durability",
            "key": "torn-wal-header",
            "op": 3,
            "fired": true,
            "fire_count": 1
        }
    });
    std::fs::write(
        root.path().join("merged-faults.jsonl"),
        format!("{fault}\n"),
    )
    .expect("write selected storage fault");
    let control = zeppelin_embed_bench::harness_json::json!({
        "seed": 7,
        "profile": "none",
        "record": {
            "campaign": "storage-durability",
            "operation": "wal-prefix",
            "seed": 7,
            "control": {
                "namespace": "storage-durability-v1",
                "seed": 7,
                "operation_fixture_id": "fixture-7-wal-prefix",
                "clean_fault_pair_id": "pair-7-wal-prefix",
                "pre_clean_inventory": [],
                "pre_fault_inventory": [],
                "pre_clean_digest": "00",
                "pre_fault_digest": "00",
                "pre_clean_artifacts": [],
                "pre_fault_artifacts": []
            }
        }
    });
    std::fs::write(
        root.path().join("merged-controls.jsonl"),
        format!("{control}\n"),
    )
    .expect("write storage same-seed control");
    let receipt = zeppelin_embed_bench::harness_json::json!({
        "seed": 7,
        "profile": "none",
        "record": {
            "campaign": "storage-durability",
            "operation": "wal-prefix",
            "fault": "torn-wal-header",
            "site": "WalOpen.HeaderValidation",
            "cardinality": 1,
            "plan": {"op_index":3,"artifact":"wal.ze","offset":0,"segment":null,"region_kind":null,"chunk":null},
            "observed": {"kind":"wal-header","artifact":"wal.ze","reason":"truncated"},
            "receipt_digest": "fnv1a64:0000000000000000"
        }
    });
    std::fs::write(
        root.path().join("merged-receipts.jsonl"),
        format!("{receipt}\n"),
    )
    .expect("write planted storage receipt");
    let artifact_digest = adversarial::storage_durability::digest32(0x4649_4c45_4641_4354, &[])
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let artifact = zeppelin_embed_bench::harness_json::json!({
        "seed": 7,
        "profile": "none",
        "record": {
            "campaign":"storage-durability",
            "operation":"wal-prefix",
            "seed":7,
            "role":"fault-wal",
            "path":"wal.ze",
            "length":0,
            "digest":artifact_digest,
            "bytes_hex":""
        }
    });
    std::fs::write(
        root.path().join("merged-family-artifact-index.jsonl.jsonl"),
        format!("{artifact}\n"),
    )
    .expect("write retained storage artifact");

    let error = read_storage_merged_ledgers(root.path())
        .expect_err("storage verifier accepted a mutated receipt digest");
    assert!(error.contains("receipt digest mismatch"), "{error}");
}

#[test]
fn feature_replay_ledger_rejects_a_digest_mismatch() {
    let root = tempfile::tempdir().expect("replay ledger root");
    let row = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-replay",
        "version": 1,
        "campaign": "metadata-filter-planner",
        "seed": 0,
        "profile": "none",
        "artifact_count": adversarial::artifacts::replay_artifacts_for(
            CampaignKind::MetadataFilterPlanner,
        )
        .len(),
        "evidence_digest": "fnv1a64:0000000000000001",
        "expected_digest": "fnv1a64:0000000000000001",
        "observed_digest": "fnv1a64:0000000000000002",
    });
    std::fs::write(root.path().join("replayed-seeds.jsonl"), format!("{row}\n"))
        .expect("write planted replay ledger");

    let error =
        validate_feature_replay_ledger(root.path(), CampaignKind::MetadataFilterPlanner, 0, 1)
            .expect_err("mismatched replay digest was accepted");
    assert!(error.contains("digest"), "{error}");
}

#[test]
fn metadata_campaign_requires_the_typed_family_coverage_catalog() {
    let required = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
        .required_coverage
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        "metadata.control.byte-identical",
        "metadata.mutation.columns.dictionary-code",
        "metadata.mutation.columns.presence-tail",
        "metadata.mutation.columns.raw-string-length",
        "metadata.predicate.and",
        "metadata.predicate.eq",
        "metadata.predicate.exists",
        "metadata.predicate.in",
        "metadata.predicate.is-null",
        "metadata.predicate.not",
        "metadata.predicate.or",
        "metadata.predicate.range",
        "metadata.i38.predicate.eq",
        "metadata.i38.predicate.range-exclusive-lower",
        "metadata.i38.predicate.range-inclusive",
        "metadata.i38.source.active",
        "metadata.i38.source.all-tombstoned",
        "metadata.i38.source.empty",
        "metadata.i38.source.missing-bounds",
        "metadata.i38.source.public-delete-wal",
        "metadata.i38.source.sealed-three-plus",
        "metadata.i39.branch.exact-allow-list",
        "metadata.i39.branch.filtered-graph",
        "metadata.i39.branch.graph-exact-fallback",
        "metadata.i39.branch.masked-scan",
        "metadata.i39.branch.pruned",
        "metadata.i39.fallback.candidate-shortfall",
        "metadata.i39.fallback.none",
        "metadata.i39.fallback.visited-budget",
        "metadata.receipt.bitmap-truncation.cardinality-one",
        "metadata.receipt.column-corruption.cardinality-one",
        "metadata.receipt.selectivity-boundary.cardinality-two",
        "metadata.receipt.visited-budget.cardinality-one",
    ]);
    assert_eq!(required, expected);
}

#[test]
fn metadata_campaign_requires_every_i37_predicate_matrix_cell() {
    let required = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
        .all_required_coverage()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for seed in 0..adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT {
        let key = format!(
            "metadata.i37.matrix.{}",
            adversarial::metadata_filter_planner::i37_predicate_case_key(seed)
        );
        assert!(
            required.contains(&key),
            "missing required I37 matrix cell {key}"
        );
    }
}

#[test]
fn storage_campaign_requires_both_shared_fault_subcases() {
    let required = CampaignSpec::for_kind(CampaignKind::StorageDurability)
        .required_coverage
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        required,
        BTreeSet::from([
            "feature_fault.storage-durability.list-delete-omission.site.delete",
            "feature_fault.storage-durability.list-delete-omission.site.list",
            "feature_fault.storage-durability.wrong-segment-object.site.family",
            "feature_fault.storage-durability.wrong-segment-object.site.identity",
        ])
    );
}

#[test]
fn merged_evidence_retains_every_family_stream_before_rotation() {
    let root = tempfile::tempdir().expect("merged family evidence root");
    let mut merged = adversarial::artifacts::MergedEvidence::create(root.path())
        .expect("create merged family evidence");
    let family = BTreeMap::from([
        (
            "metadata-fixture.json".to_owned(),
            b"{\"fixture\":1}\n".to_vec(),
        ),
        (
            "queries.jsonl".to_owned(),
            b"{\"query\":1}\n{\"query\":2}\n".to_vec(),
        ),
        (
            "fixture-mutations.jsonl".to_owned(),
            b"{\"mutation\":1}\n{\"mutation\":2}\n".to_vec(),
        ),
    ]);
    merged
        .append_episode(
            7,
            FaultProfile::None,
            b"{\"op\":1}\n",
            b"{\"fault\":null}\n",
            b"[]\n",
            b"{\"coverage\":1}\n",
            b"{\"invariant\":\"I36\",\"passed\":true}\n",
            b"{\"control\":1}\n",
            b"",
            b"{\"mutation\":null}\n",
            &family,
        )
        .expect("append merged family evidence");
    let stats = merged.stats();

    for stream in ["program", "faults", "violations", "coverage"] {
        assert!(
            stats.streams.contains_key(stream),
            "merged evidence omitted {stream}"
        );
    }
    assert_eq!(stats.streams["family/metadata-fixture.json"].records, 1);
    assert_eq!(stats.streams["family/queries.jsonl"].records, 2);
    assert_eq!(stats.streams["family/fixture-mutations.jsonl"].records, 2);
    for name in [
        "merged-family-metadata-fixture.json.jsonl",
        "merged-family-queries.jsonl.jsonl",
        "merged-family-fixture-mutations.jsonl.jsonl",
    ] {
        assert!(root.path().join(name).is_file(), "missing {name}");
    }
}

#[test]
fn merged_evidence_rejects_a_duplicate_seed_profile_before_rotation() {
    let root = tempfile::tempdir().expect("duplicate merged evidence root");
    let mut merged = adversarial::artifacts::MergedEvidence::create(root.path())
        .expect("create duplicate merged evidence");
    let family = BTreeMap::new();
    let append = |merged: &mut adversarial::artifacts::MergedEvidence| {
        merged.append_episode(
            7,
            FaultProfile::None,
            b"{\"op\":1}\n",
            b"",
            b"[]\n",
            b"{\"coverage\":1}\n",
            b"{\"checker_id\":\"I36.column-roundtrip.v2\",\"case_identity\":\"query-7\"}\n",
            b"",
            b"",
            b"",
            &family,
        )
    };
    append(&mut merged).expect("append first merged seed/profile");
    let error = append(&mut merged)
        .expect_err("duplicate merged seed/profile was appended before rotation");
    assert!(error.contains("duplicate merged episode"), "{error}");
}

#[test]
fn merged_evidence_rejects_a_duplicate_oracle_identity_before_any_append() {
    let root = tempfile::tempdir().expect("duplicate merged oracle root");
    let mut merged = adversarial::artifacts::MergedEvidence::create(root.path())
        .expect("create duplicate merged oracle evidence");
    let row = b"{\"checker_id\":\"I37.bitmap-algebra.v2\",\"case_identity\":\"eq-u64-present\"}\n";
    let mut oracle = row.to_vec();
    oracle.extend_from_slice(row);
    let error = merged
        .append_episode(
            11,
            FaultProfile::None,
            b"{\"op\":1}\n",
            b"",
            b"[]\n",
            b"{\"coverage\":1}\n",
            &oracle,
            b"",
            b"",
            b"",
            &BTreeMap::new(),
        )
        .expect_err("duplicate merged oracle identity was appended before rotation");
    assert!(
        error.contains("duplicate merged oracle identity"),
        "{error}"
    );
    assert_eq!(
        merged.stats().streams["program"].records,
        0,
        "duplicate oracle preflight partially appended another merged stream"
    );
}

#[test]
fn merged_evidence_reopens_and_rejects_a_corrupted_durable_stream() {
    let root = tempfile::tempdir().expect("durable merged evidence root");
    let mut merged = adversarial::artifacts::MergedEvidence::create(root.path())
        .expect("create durable merged evidence");
    merged
        .append_episode(
            9,
            FaultProfile::None,
            b"{\"op\":1}\n",
            b"",
            b"[]\n",
            b"{\"coverage\":1}\n",
            b"{\"checker_id\":\"I36.column-roundtrip.v2\",\"case_identity\":\"query-9\"}\n",
            b"",
            b"",
            b"",
            &BTreeMap::new(),
        )
        .expect("append durable merged evidence");
    let oracle_path = root.path().join("merged-oracle.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&oracle_path)
        .expect("open merged oracle for corruption plant")
        .write_all(b"{}\n")
        .expect("append corruption plant");

    let error = merged
        .verify_durable()
        .expect_err("corrupted durable merged evidence was accepted");
    assert!(error.contains("merged-oracle.jsonl"), "{error}");
    assert!(error.contains("reopen verification"), "{error}");
}

#[test]
fn legacy_replay_artifacts_are_implicit_overall() {
    for fixture in [
        zeppelin_embed_bench::harness_json::json!({"version": 2}),
        zeppelin_embed_bench::harness_json::json!({"schema": "zeppelin-embed-adversarial-failure", "version": 1}),
    ] {
        let directory = tempfile::tempdir().expect("legacy replay directory");
        std::fs::write(
            directory.path().join("legacy.json"),
            zeppelin_embed_bench::harness_json::to_vec(&fixture).expect("legacy fixture JSON"),
        )
        .expect("write legacy fixture");
        assert_eq!(
            adversarial::campaign::campaign_from_replay_metadata(directory.path())
                .expect("legacy replay campaign"),
            CampaignKind::Overall
        );
    }
}

#[test]
fn feature_replay_rejects_schema_v3_without_independent_oracle_attestation() {
    let directory = tempfile::tempdir().expect("feature replay directory");
    std::fs::write(
        directory.path().join("episode.json"),
        zeppelin_embed_bench::harness_json::to_vec(&zeppelin_embed_bench::harness_json::json!({
            "schema": "zeppelin-embed-adversarial-episode",
            "version": 3,
            "campaign": "fts",
            "seed": 7,
            "profile": "none",
        }))
        .expect("feature fixture JSON"),
    )
    .expect("write feature fixture");
    let error = adversarial::campaign::campaign_from_replay_metadata(directory.path())
        .expect_err("unattested feature replay must be rejected");
    assert!(error.contains("oracle attestation"), "{error}");
}

#[test]
fn completed_feature_summary_rejects_pre_attestation_schema_v3() {
    let root = tempfile::tempdir().expect("old feature summary root");
    let summary = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-campaign",
        "version": 3,
        "campaign": "fts",
        "complete": true,
        "episodes": 1,
        "qualification_passed": true,
    });
    let error = verify_feature_summary_attestation(root.path(), CampaignKind::Fts, 1, &summary)
        .expect_err("pre-attestation summary must be rejected");
    assert!(error.contains("oracle_contract_version"), "{error}");
}

#[test]
fn metadata_summary_rejects_a_generic_feature_attestation_explicitly() {
    let root = tempfile::tempdir().expect("generic metadata summary root");
    let summary = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-campaign",
        "version": 3,
        "campaign": "metadata-filter-planner",
        "complete": true,
        "episodes": 1,
        "start_seed": 0,
        "attestation": {
            "oracle_contract_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
            "oracle_contract": "metadata-filter-planner-oracle-v2",
            "harness_git_revision": adversarial::artifacts::harness_git_revision(),
        },
    });
    let error = verify_feature_summary_attestation(
        root.path(),
        CampaignKind::MetadataFilterPlanner,
        1,
        &summary,
    )
    .expect_err("generic attestation was accepted for metadata qualification");
    assert!(
        error.contains("missing metadata oracle attestation"),
        "{error}"
    );
}

#[test]
fn feature_oracle_attestation_rejects_a_wrong_checker_version() {
    let record = zeppelin_embed_bench::harness_json::json!({
        "invariant": "I36",
        "checker_id": "generic-marker-v1",
        "operation": "columns",
        "expected": {"row": 1},
        "observed": {"row": 1},
        "input_digest": "fnv1a64:0000000000000000",
        "observed_digest": "fnv1a64:0000000000000000",
        "passed": true,
        "first_difference": null,
    });
    let error = validate_feature_oracle_record(CampaignKind::MetadataFilterPlanner, &record)
        .expect_err("a generic checker version earned I36 credit");
    assert!(error.contains("checker_id"), "{error}");
}

#[test]
fn scheduled_open_fault_reaches_store_directory_admission() {
    let directory = tempfile::tempdir().expect("scheduled directory admission");
    let scheduled = Arc::new(ScheduledVfs::new(
        StdVfs,
        FaultSchedule::single(FaultEvent {
            id: "stage-06-directory-open-eio".to_owned(),
            op_index: 0,
            layer: Layer::Io,
            site: FaultSite::Open,
            mode: FaultMode::Eio,
            nth_match: 1,
            expected_matches: None,
            path_contains: None,
            fired: false,
            fire_count: 0,
            path: None,
        }),
    ));
    scheduled.set_operation(0);

    let opened = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        StoreTestDependencies::new(scheduled.clone(), Arc::new(ManualMonotonicClock::new())),
    );
    assert!(
        matches!(
            opened,
            Err(StoreError::Io { path, source })
                if path == directory.path() && source.raw_os_error() == Some(5)
        ),
        "store-directory admission bypassed the scheduled Open/Eio fault"
    );
    assert_eq!(
        scheduled
            .events()
            .into_iter()
            .next()
            .and_then(|event| event.path),
        Some(PathBuf::from("."))
    );
}

#[test]
fn injected_store_vfs_reaches_open_and_wal_creation() {
    let directory = tempfile::tempdir().expect("injected VFS store");
    let scheduled = Arc::new(ScheduledVfs::new(
        StdVfs,
        FaultSchedule::single(FaultEvent {
            id: "open-write".to_owned(),
            op_index: 0,
            layer: Layer::Io,
            site: FaultSite::Write,
            mode: FaultMode::Eio,
            nth_match: 1,
            expected_matches: None,
            path_contains: None,
            fired: false,
            fire_count: 0,
            path: None,
        }),
    ));
    scheduled.set_operation(0);
    let dependencies =
        StoreTestDependencies::new(scheduled.clone(), Arc::new(ManualMonotonicClock::new()));
    let result = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_schema(Schema::timestamp_only()),
        dependencies,
    );
    if let Ok(store) = result {
        store.close().expect("close unexpectedly opened store");
    }
    assert!(
        scheduled.events().into_iter().any(|event| event.fired),
        "the Store-owned VFS did not observe initial manifest publication"
    );
}

#[test]
fn self_test_dropped_acknowledged_write_trips_i1() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::DropAcknowledgedWrite);
    assert_eq!(violation.invariant, Invariant::I1);
    println!("{}", violation.report());
}

#[test]
fn self_test_wrong_document_trips_i3() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::WrongDocument);
    assert_eq!(violation.invariant, Invariant::I3);
    println!("{}", violation.report());
}

#[test]
fn self_test_tombstone_leak_trips_i2() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::LeakTombstone);
    assert_eq!(violation.invariant, Invariant::I2);
    println!("{}", violation.report());
}

#[test]
fn self_test_generation_lie_trips_i9() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::MisreportGeneration);
    assert_eq!(violation.invariant, Invariant::I9);
    println!("{}", violation.report());
}

#[test]
fn self_test_filtered_result_outside_predicate_trips_i5() {
    let violation = adversarial::runner::planted_counterexample(Invariant::I5);
    assert_eq!(violation.invariant, Invariant::I5);
    println!("{}", violation.report());
}

#[test]
fn self_test_diagnostics_lie_trips_i13() {
    let violation = adversarial::runner::planted_counterexample(Invariant::I13);
    assert_eq!(violation.invariant, Invariant::I13);
    println!("{}", violation.report());
}

#[test]
fn self_test_mixed_alias_segments_trip_i14() {
    let violation = adversarial::runner::planted_counterexample(Invariant::I14);
    assert_eq!(violation.invariant, Invariant::I14);
    println!("{}", violation.report());
}

#[test]
fn every_implemented_invariant_has_a_counterexample_that_trips_it() {
    for invariant in [
        Invariant::I1,
        Invariant::I2,
        Invariant::I3,
        Invariant::I4,
        Invariant::I5,
        Invariant::I6,
        Invariant::I7,
        Invariant::I8,
        Invariant::I9,
        Invariant::I10,
        Invariant::I11,
        Invariant::I12,
        Invariant::I13,
        Invariant::I14,
        Invariant::I54,
    ] {
        let violation = adversarial::runner::planted_counterexample(invariant);
        assert_eq!(violation.invariant, invariant);
    }
}

#[test]
fn same_seed_is_byte_identical_and_has_identical_outcome() {
    let first = tempfile::tempdir().expect("first artifact root");
    let second = tempfile::tempdir().expect("second artifact root");
    let left = adversarial::runner::run_program(11, FaultProfile::None, first.path())
        .expect("first deterministic run");
    let right = adversarial::runner::run_program(11, FaultProfile::None, second.path())
        .expect("second deterministic run");
    assert_eq!(left.program_bytes, right.program_bytes);
    assert_eq!(left.faults_bytes, right.faults_bytes);
    assert_eq!(left.violations_bytes, right.violations_bytes);
    assert_eq!(left.coverage_bytes, right.coverage_bytes);
    assert_eq!(left.violations, right.violations);
}

#[test]
fn adversarial_program_can_delete_a_pre_seal_id() {
    let program = Program::generate(11);
    let (first_id, count) = program
        .ops
        .iter()
        .find_map(|op| match op {
            Op::Ingest {
                first_id, count, ..
            } => Some((*first_id, *count)),
            _ => None,
        })
        .expect("generated program starts with an ingest range");
    let first_seal = program
        .ops
        .iter()
        .position(|op| matches!(op, Op::Seal))
        .expect("generated program seals its initial range");
    let was_in_initial_range =
        |doc_id: u32| doc_id >= first_id && doc_id < first_id.saturating_add(count);
    let after_first_seal = &program.ops[first_seal.saturating_add(1)..];

    assert!(
        after_first_seal
            .iter()
            .any(|op| matches!(op, Op::Delete { doc_id } if was_in_initial_range(*doc_id)))
    );
    assert!(
        program.ops[..first_seal]
            .iter()
            .any(|op| matches!(op, Op::FilteredSearch { .. }))
    );
    assert!(
        after_first_seal
            .iter()
            .any(|op| matches!(op, Op::Revise { doc_id, .. } if was_in_initial_range(*doc_id)))
    );
    assert!(
        after_first_seal
            .iter()
            .any(|op| matches!(op, Op::Upsert { doc_id, .. } if was_in_initial_range(*doc_id)))
    );
    assert!(
        program
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Seal))
            .count()
            > 1
    );
}

#[test]
fn the_emitted_sweep_program_interleaves_an_epoch_mismatch_probe_with_real_writes() {
    let root = tempfile::tempdir().expect("epoch probe artifact root");
    let outcome = adversarial::runner::run_program(11, FaultProfile::None, root.path())
        .expect("emit adversarial program");
    let emitted = std::str::from_utf8(&outcome.program_bytes).expect("program artifact is UTF-8");
    let lines = emitted.lines().collect::<Vec<_>>();
    let probe = lines
        .iter()
        .position(|line| line.contains("\"kind\":\"epoch_mismatch_probe\""))
        .expect("emitted program contains the epoch probe");

    assert!(
        lines[..probe]
            .iter()
            .any(|line| line.contains("\"kind\":\"ingest\""))
    );
    assert!(lines[probe.saturating_add(1)..].iter().any(|line| {
        line.contains("\"kind\":\"upsert\"") || line.contains("\"kind\":\"revise\"")
    }));
}

#[test]
fn an_emitted_program_executes_alias_switch_rollback_drop_and_typed_rejection() {
    let root = tempfile::tempdir().expect("epoch-transition artifact root");
    let outcome = adversarial::runner::run_program(11, FaultProfile::None, root.path())
        .expect("execute emitted epoch-transition program");
    assert!(
        outcome.violations.is_empty(),
        "epoch-transition program found violations: {:?}",
        outcome.violations
    );
    assert_eq!(outcome.epoch_preparations, 1);
    assert_eq!(outcome.epoch_alias_switches, 2);
    assert_eq!(outcome.epoch_rollbacks, 1);
    assert_eq!(outcome.epoch_drops, 1);
    assert_eq!(outcome.rejected_dropped_epoch_rollbacks, 1);

    let emitted = std::str::from_utf8(&outcome.program_bytes).expect("program artifact is UTF-8");
    for operation in [
        "prepare_epoch_b",
        "switch_alias_to_b",
        "rollback_to_a",
        "drop_epoch_a",
        "rollback_dropped_a_probe",
    ] {
        assert!(
            emitted
                .lines()
                .any(|line| line.contains(&format!("\"kind\":\"{operation}\""))),
            "executed artifact omitted {operation}"
        );
    }
}

#[test]
fn an_emitted_program_executes_a_filtered_graph_query() {
    let program = Program::generate(0);
    assert!(
        program
            .ops
            .iter()
            .any(|op| matches!(op, Op::FilteredSearch { .. })),
        "the emitted program must contain a filtered operation"
    );
    let artifacts = tempfile::tempdir().expect("filtered graph artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("filtered graph reachability program");
    assert!(
        outcome.violations.is_empty(),
        "filtered graph reachability found violations: {:?}",
        outcome.violations
    );
    assert!(
        outcome.filtered_graph_searches > 0,
        "the emitted program ran no in-traversal filtered graph branch"
    );
}

#[test]
fn an_emitted_hybrid_operation_fuses_sealed_vectors_and_lexical_content() {
    let program = Program::generate(0);
    let hybrid = program
        .ops
        .iter()
        .position(|op| matches!(op, Op::HybridSearch { .. }))
        .expect("the emitted program must contain a hybrid operation");
    assert!(
        program.ops[..hybrid]
            .iter()
            .any(|op| matches!(op, Op::Seal)),
        "hybrid reachability must cross a seal boundary"
    );
    let artifacts = tempfile::tempdir().expect("hybrid artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("hybrid reachability program");
    assert!(
        outcome.violations.is_empty(),
        "hybrid reachability found violations: {:?}",
        outcome.violations
    );
    assert!(outcome.hybrid_searches > 0, "no hybrid operation executed");
    assert!(
        outcome.text_documents_ingested > 0,
        "hybrid reachability ingested no text through Store"
    );
    assert!(
        outcome.store_lexical_searches > 0,
        "hybrid reachability bypassed Store::search_lexical"
    );
    assert!(
        outcome.store_hybrid_searches > 0,
        "hybrid reachability bypassed Store::search_hybrid"
    );
    assert!(
        outcome.hybrid_sealed_vector_documents > 0,
        "hybrid operation reached no sealed vector documents"
    );
    assert!(
        outcome.hybrid_lexical_documents > 0,
        "hybrid operation reached no lexical content"
    );
}

#[test]
fn an_emitted_fts_probe_reaches_every_shipped_lexical_extra() {
    let artifacts = tempfile::tempdir().expect("FTS extras artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("FTS extras reachability program");
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert!(outcome.phrase_searches > 0, "phrase path was unreachable");
    assert!(outcome.prefix_searches > 0, "prefix path was unreachable");
    assert!(outcome.fuzzy_searches > 0, "fuzzy path was unreachable");
    assert!(
        outcome.phonetic_encodes > 0,
        "phonetic path was unreachable"
    );
    assert!(outcome.snippets_built > 0, "snippet path was unreachable");
}

#[test]
fn one_episode_records_successful_public_path_coverage() {
    let artifacts = tempfile::tempdir().expect("coverage artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("coverage episode");
    assert!(
        outcome.coverage.count("op.open") > 0,
        "open was not registered"
    );
    assert!(
        outcome.coverage.count("store.hybrid_search") > 0,
        "Store hybrid path was not registered"
    );
    assert!(
        !outcome.coverage_bytes.is_empty(),
        "coverage artifact is empty"
    );
}

#[test]
fn crash_seam_audit_does_not_credit_injected_fault_coverage() {
    let artifacts = tempfile::tempdir().expect("crash audit coverage artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("crash audit episode");

    assert_eq!(
        outcome.coverage.count("fault.layer.crash"),
        0,
        "crash-seam audit was credited as an injected Crash-layer fault"
    );
    assert_eq!(outcome.coverage.count("fault.site.write"), 0);
    assert_eq!(outcome.coverage.count("fault.mode.torn_write"), 0);
    assert_eq!(outcome.coverage.count("fault.layer.count.1"), 0);
    assert_eq!(outcome.faults_fired, 0);
}

#[test]
fn audit_crash_seam_no_longer_pushes_a_synthetic_event() {
    let artifacts = tempfile::tempdir().expect("crash audit artifact root");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("run crash audit episode");

    assert!(
        outcome.faults_bytes.is_empty(),
        "crash audit emitted a synthetic fault event: {}",
        String::from_utf8_lossy(&outcome.faults_bytes)
    );
}

#[test]
fn simulated_crash_event_fires_at_a_sampled_ingest_and_store_reopens() {
    let (seed, op_index) = (0..512)
        .find_map(|seed| {
            let program = Program::generate(seed);
            plan_schedule(
                seed,
                environment_for_profile(FaultProfile::Crash, seed),
                &program,
            )
            .events
            .iter()
            .find(|event| {
                event.layer == Layer::Crash
                    && matches!(program.ops[event.op_index], Op::Ingest { .. })
            })
            .map(|event| (seed, event.op_index))
        })
        .expect("Crash preset never planned a sampled ingest");
    let artifacts = tempfile::tempdir().expect("simulated crash artifacts");
    let outcome = adversarial::runner::run_program(seed, FaultProfile::Crash, artifacts.path())
        .expect("run sampled ingest crash episode");

    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert!(outcome.coverage.count("fault.layer.crash") > 0);
    assert!(
        std::str::from_utf8(&outcome.faults_bytes)
            .expect("fault records are UTF-8")
            .lines()
            .any(|line| {
                let record: zeppelin_embed_bench::harness_json::Value =
                    zeppelin_embed_bench::harness_json::from_str(line)
                        .expect("parse crash fault record");
                record["op"].as_u64() == Some(op_index as u64)
                    && record["layer"].as_str() == Some("crash")
                    && record["fired"].as_bool() == Some(true)
            }),
        "sampled ingest crash event did not fire"
    );
    assert_eq!(outcome.operations, Program::generate(seed).ops.len());
}

#[test]
fn simulated_crash_after_torn_seal_write_yields_clean_prefix_or_refusal() {
    let (seed, content_op, crash_op) = (0..20_000)
        .find_map(|seed| {
            let program = Program::generate(seed);
            let schedule = plan_schedule(
                seed,
                environment_for_profile(FaultProfile::Full, seed),
                &program,
            );
            let content = schedule.events.iter().find(|event| {
                event.layer == Layer::Content
                    && event.site == FaultSite::Write
                    && event.mode == FaultMode::TornWrite
                    && matches!(program.ops[event.op_index], Op::Seal)
            })?;
            let crash = schedule.events.iter().find(|event| {
                event.layer == Layer::Crash
                    && event.op_index > content.op_index
                    && event.op_index <= content.op_index.saturating_add(3)
            })?;
            Some((seed, content.op_index, crash.op_index))
        })
        .expect("no seed planned TornWrite/Seal followed by Crash within three ops");
    let artifacts = tempfile::tempdir().expect("torn seal crash artifacts");
    let outcome = adversarial::runner::run_program(seed, FaultProfile::Full, artifacts.path())
        .expect("torn seal crash episode must return a typed outcome");

    assert!(
        outcome.violations.iter().all(|violation| {
            violation.invariant != Invariant::I4 || violation.op_index != crash_op
        }),
        "seed {seed} violated I4 after ops {content_op}->{crash_op}: {:?}",
        outcome.violations
    );
    assert!(
        outcome.coverage.count("crash.after.torn_write") > 0,
        "seed {seed} did not credit torn-write -> crash composition"
    );
}

#[test]
fn real_crash_child_rebuilds_the_schedule_from_seed() {
    let (seed, op_index, boundary, event_id) =
        (0..20_000)
            .find_map(|seed| {
                let program = Program::generate(seed);
                let (op_index, boundary) = program.ops.iter().enumerate().find_map(
                    |(index, operation)| match operation {
                        Op::Crash { boundary, .. }
                            if *boundary == adversarial::program::CrashBoundary::MidWalGroup =>
                        {
                            Some((index, *boundary))
                        }
                        _ => None,
                    },
                )?;
                let event = plan_schedule(
                    seed,
                    environment_for_profile(FaultProfile::Full, seed),
                    &program,
                )
                .events
                .into_iter()
                .find(|event| {
                    event.op_index == op_index
                        && matches!(event.layer, Layer::Io | Layer::Content)
                        && event.site == FaultSite::Append
                })?;
                Some((seed, op_index, boundary, event.id))
            })
            .expect("no seed planned a child-visible fault at MidWalGroup");
    let directory = tempfile::tempdir().expect("real crash child directory");
    let marker = directory.path().join("crash-marker");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["crash_child", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CRASH_CHILD_PATH", directory.path())
        .env("ZE_ADV_CRASH_CHILD_MARKER", &marker)
        .env("ZE_ADV_CRASH_CHILD_DOC", "1")
        .env("ZE_ADV_CRASH_CHILD_REV", "1")
        .env("ZE_ADV_CRASH_CHILD_TS", "10")
        .env("ZE_ADV_CRASH_BOUNDARY", boundary.key())
        .env("ZE_ADV_CRASH_CHILD_OP", op_index.to_string())
        .env("ZE_ADV_SEED", seed.to_string())
        .env("ZE_ADV_CAMPAIGN", CampaignKind::Overall.key())
        .env("ZE_ADV_PROFILE", FaultProfile::Full.key())
        .output()
        .expect("spawn real crash child");
    assert!(
        !output.status.success() && output.status.code().is_none(),
        "child did not die by signal: {} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let faults = std::fs::read_to_string(directory.path().join("faults.jsonl"))
        .expect("child wrote faults.jsonl");
    assert!(
        faults.lines().any(|line| {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(line)
                    .expect("parse child fault record");
            record["id"].as_str() == Some(event_id.as_str())
                && record["fired"].as_bool() == Some(true)
                && record["fire_count"].as_u64() == Some(1)
        }),
        "child fault {event_id} did not fire: {faults}"
    );
}

#[test]
fn crash_layer_can_catch_a_missing_sync_before_manifest_rename() {
    let artifacts = tempfile::tempdir().expect("crash CAN-CATCH artifacts");
    let mut publication_sync_witnesses = 0_usize;
    for seed in 0..96 {
        let program = Program::generate(seed);
        let required_publication_syncs = plan_schedule(
            seed,
            environment_for_profile(FaultProfile::Crash, seed),
            &program,
        )
        .events
        .into_iter()
        .filter(|event| {
            event.layer == Layer::Crash
                && event.site == FaultSite::Sync
                && event.nth_match == 4
                && event.expected_matches.is_none()
                && matches!(program.ops[event.op_index], Op::Seal)
        })
        .map(|event| event.id)
        .collect::<Vec<_>>();
        let outcome = adversarial::runner::run_program(seed, FaultProfile::Crash, artifacts.path())
            .unwrap_or_else(|error| panic!("crash episode {seed} failed: {error}"));
        assert!(
            outcome.violations.is_empty(),
            "crash episode {seed} found violations: {:?}; faults={}; program={}",
            outcome.violations,
            String::from_utf8_lossy(&outcome.faults_bytes),
            String::from_utf8_lossy(&outcome.program_bytes)
        );
        let fired = std::str::from_utf8(&outcome.faults_bytes)
            .expect("crash CAN-CATCH faults are UTF-8")
            .lines()
            .filter_map(|line| {
                let record: zeppelin_embed_bench::harness_json::Value =
                    zeppelin_embed_bench::harness_json::from_str(line)
                        .expect("parse crash CAN-CATCH fault");
                (record["fired"].as_bool() == Some(true))
                    .then(|| record["id"].as_str().map(str::to_owned))
                    .flatten()
            })
            .collect::<BTreeSet<_>>();
        for event_id in required_publication_syncs {
            publication_sync_witnesses = publication_sync_witnesses.saturating_add(1);
            assert!(
                fired.contains(&event_id),
                "crash episode {seed} did not reach required manifest publication sync {event_id}"
            );
        }
    }
    assert!(
        publication_sync_witnesses > 0,
        "96 crash episodes scheduled no manifest publication sync witness"
    );
}

#[test]
fn twelve_seed_sweep_emits_every_typed_predicate() {
    let mut seen = std::collections::BTreeSet::new();
    for seed in 0..12 {
        for operation in Program::generate(seed).ops {
            if let Op::PredicateSearch { predicate, .. } = operation {
                seen.insert(predicate.key());
            }
        }
    }
    let expected = PredicateKind::ALL
        .into_iter()
        .map(PredicateKind::key)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(seen, expected);
}

#[test]
fn twelve_seed_sweep_emits_every_process_crash_boundary() {
    let seen = (0..12)
        .flat_map(|seed| Program::generate(seed).ops)
        .filter_map(|operation| match operation {
            Op::Crash { boundary, .. } => Some(boundary.key()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let expected = adversarial::program::CrashBoundary::ALL
        .into_iter()
        .map(adversarial::program::CrashBoundary::key)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(seen, expected);
}

#[test]
fn smoke() {
    let config = RunConfig::from_env().expect("valid adversarial run configuration");
    let root = config.artifacts.clone();
    let mut failures = Vec::new();
    let mut unfired = Vec::new();
    let mut coverage = CoverageRegistry::default();
    let mut smoke_episodes = 0_u64;
    let mut generic_multi_event_episodes = 0_u64;
    let smoke_seeds = CampaignSpec::for_kind(config.campaign).smoke_seeds;
    for profile in FaultProfile::ALL {
        for offset in smoke_seeds {
            let seed = config
                .start_seed
                .checked_add(*offset)
                .expect("smoke seed range fits u64");
            let outcome =
                adversarial::runner::run_program_for(config.campaign, seed, profile, &root)
                    .unwrap_or_else(|error| {
                        panic!(
                            "campaign={} seed={seed} profile={}: {error}",
                            config.campaign.key(),
                            profile.key()
                        )
                    });
            println!(
                "ADV campaign={} seed={seed} profile={} ops={} faults={} scheduled_faults={} feature_faults={}/{} graph_searches={} filtered_searches={} filtered_graph_searches={} predicate_searches={} hybrid_searches={} hybrid_sealed_vector_documents={} hybrid_lexical_documents={} violations={}",
                config.campaign.key(),
                profile.key(),
                outcome.operations,
                outcome.faults_fired,
                outcome.scheduled_faults_fired,
                outcome.feature_faults_fired,
                outcome.feature_faults_scheduled,
                outcome.graph_searches,
                outcome.filtered_searches,
                outcome.filtered_graph_searches,
                outcome.predicate_searches,
                outcome.hybrid_searches,
                outcome.hybrid_sealed_vector_documents,
                outcome.hybrid_lexical_documents,
                outcome.violations.len()
            );
            smoke_episodes = smoke_episodes.saturating_add(1);
            generic_multi_event_episodes = generic_multi_event_episodes
                .saturating_add(u64::from(outcome.scheduled_faults_fired >= 2));
            if !outcome.missing_feature_faults.is_empty() {
                unfired.push(format!(
                    "seed={seed} profile={} feature={:?}",
                    profile.key(),
                    outcome.missing_feature_faults
                ));
            }
            coverage.merge(&outcome.coverage);
            for violation in outcome.violations {
                println!("{}", violation.report_for(config.campaign));
                failures.push(violation);
            }
        }
    }
    assert!(
        generic_multi_event_episodes.saturating_mul(10) >= smoke_episodes.saturating_mul(3),
        "generic CAN-FIRE failed: {generic_multi_event_episodes}/{smoke_episodes} smoke episodes fired at least two scheduled faults; require >=30%"
    );
    assert!(
        failures.is_empty(),
        "adversarial smoke found {} invariant violations",
        failures.len()
    );
    assert!(
        unfired.is_empty(),
        "adversarial smoke selected faults that did not fire: {unfired:?}"
    );
    let missing = missing_campaign_coverage(config.campaign, &coverage);
    assert!(
        missing.is_empty(),
        "adversarial smoke missed required coverage: {missing:?}"
    );
}

#[test]
#[ignore = "explicit seeded adversarial replay entry point"]
fn run() {
    let config = RunConfig::from_env().expect("valid adversarial run configuration");
    let outcome = adversarial::runner::run_program_for(
        config.campaign,
        config.seed,
        config.profile,
        &config.artifacts,
    )
    .expect("seeded adversarial run");
    for violation in &outcome.violations {
        println!("{}", violation.report_for(config.campaign));
    }
    assert!(outcome.violations.is_empty());
    assert!(
        outcome.missing_feature_faults.is_empty(),
        "scheduled feature faults did not fire: {:?}",
        outcome.missing_feature_faults
    );
}

#[test]
#[ignore = "explicit byte-identical artifact replay entry point"]
fn replay() {
    let config = RunConfig::from_env().expect("valid adversarial replay configuration");
    let expected = config
        .replay_directory
        .as_ref()
        .expect("ZE_ADV_REPLAY_DIR must name one run directory");
    let stored_campaign = adversarial::campaign::campaign_from_replay_metadata(expected)
        .expect("campaign from replay metadata");
    if std::env::var_os("ZE_ADV_CAMPAIGN").is_some() {
        assert_eq!(
            config.campaign, stored_campaign,
            "explicit replay campaign differs from retained episode"
        );
    }
    match stored_campaign {
        CampaignKind::StorageDurability => {
            #[cfg(unix)]
            replay_storage_retained_episode(expected).unwrap_or_else(|error| panic!("{error}"));
            #[cfg(not(unix))]
            panic!("retained storage replay requires Unix");
            return;
        }
        CampaignKind::VectorExecution => {
            replay_vector_retained_episode(expected).unwrap_or_else(|error| panic!("{error}"));
            return;
        }
        CampaignKind::MetadataFilterPlanner => {
            replay_metadata_retained_episode(expected).unwrap_or_else(|error| panic!("{error}"));
            return;
        }
        _ => {}
    }
    let metadata_path = expected.join("episode.json");
    let (seed, profile) = if metadata_path.is_file() {
        let metadata: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(
                &std::fs::read(&metadata_path).expect("read retained replay metadata"),
            )
            .expect("parse retained replay metadata");
        let seed = metadata["seed"]
            .as_u64()
            .expect("retained replay metadata seed");
        let profile = metadata["profile"]
            .as_str()
            .ok_or_else(|| "retained replay metadata profile is absent".to_owned())
            .and_then(FaultProfile::from_key)
            .expect("retained replay metadata profile");
        (seed, profile)
    } else {
        (config.seed, config.profile)
    };
    let actual_root = tempfile::tempdir().expect("replay output root");
    let outcome =
        adversarial::runner::run_program_for(stored_campaign, seed, profile, actual_root.path())
            .expect("replayed adversarial run");
    compare_replay_artifacts(expected, stored_campaign, &outcome)
        .unwrap_or_else(|error| panic!("{error}"));
}

fn compare_replay_artifacts(
    expected: &Path,
    campaign: CampaignKind,
    outcome: &adversarial::runner::RunOutcome,
) -> Result<(), String> {
    let mut replay_artifacts = vec![
        ("program.jsonl".to_owned(), outcome.program_bytes.as_slice()),
        ("faults.jsonl".to_owned(), outcome.faults_bytes.as_slice()),
        (
            "violations.json".to_owned(),
            outcome.violations_bytes.as_slice(),
        ),
        (
            "coverage.json".to_owned(),
            outcome.coverage_bytes.as_slice(),
        ),
    ];
    if campaign != CampaignKind::Overall {
        replay_artifacts.extend([
            ("oracle.jsonl".to_owned(), outcome.oracle_bytes.as_slice()),
            (
                "controls.jsonl".to_owned(),
                outcome.controls_bytes.as_slice(),
            ),
            (
                "receipts.jsonl".to_owned(),
                outcome.receipts_bytes.as_slice(),
            ),
            (
                "mutations.jsonl".to_owned(),
                outcome.mutations_bytes.as_slice(),
            ),
            ("episode.json".to_owned(), outcome.episode_bytes.as_slice()),
        ]);
        replay_artifacts.extend(
            outcome
                .family_artifact_bytes
                .iter()
                .map(|(name, bytes)| (name.clone(), bytes.as_slice())),
        );
    }
    let observed_names = replay_artifacts
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<BTreeSet<_>>();
    let expected_names = if campaign == CampaignKind::Overall {
        [
            "program.jsonl",
            "faults.jsonl",
            "violations.json",
            "coverage.json",
        ]
        .into_iter()
        .collect()
    } else {
        adversarial::artifacts::replay_artifacts_for(campaign)
            .into_iter()
            .collect()
    };
    if observed_names != expected_names {
        return Err(format!(
            "replay artifact set mismatch expected={expected_names:?} observed={observed_names:?}"
        ));
    }
    for (name, actual) in replay_artifacts {
        let expected_bytes = std::fs::read(expected.join(&name))
            .map_err(|error| format!("read replay artifact {name}: {error}"))?;
        if actual != expected_bytes {
            let byte_offset = expected_bytes
                .iter()
                .zip(actual.iter())
                .position(|(expected, observed)| expected != observed)
                .unwrap_or_else(|| expected_bytes.len().min(actual.len()));
            let prefix = &expected_bytes[..byte_offset.min(expected_bytes.len())];
            let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
            let column = prefix
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(byte_offset + 1, |newline| byte_offset - newline);
            let expected_byte = expected_bytes
                .get(byte_offset)
                .map_or_else(|| "<eof>".to_owned(), |byte| format!("0x{byte:02x}"));
            let observed_byte = actual
                .get(byte_offset)
                .map_or_else(|| "<eof>".to_owned(), |byte| format!("0x{byte:02x}"));
            return Err(format!(
                "replay drifted artifact={name} byte_offset={byte_offset} line={line} column={column} expected={expected_byte} observed={observed_byte}"
            ));
        }
    }
    Ok(())
}

fn replay_evidence_digest(outcome: &adversarial::runner::RunOutcome) -> String {
    let mut evidence = vec![
        outcome.program_bytes.as_slice(),
        outcome.faults_bytes.as_slice(),
        outcome.violations_bytes.as_slice(),
        outcome.coverage_bytes.as_slice(),
        outcome.oracle_bytes.as_slice(),
        outcome.controls_bytes.as_slice(),
        outcome.receipts_bytes.as_slice(),
        outcome.mutations_bytes.as_slice(),
        outcome.episode_bytes.as_slice(),
    ];
    evidence.extend(outcome.family_artifact_bytes.values().map(Vec::as_slice));
    adversarial::artifacts::evidence_digest(&evidence)
}

#[cfg(unix)]
fn replay_storage_retained_episode(expected: &Path) -> Result<String, String> {
    let campaign = CampaignKind::StorageDurability;
    let mut artifacts = BTreeMap::<String, Vec<u8>>::new();
    for name in adversarial::artifacts::replay_artifacts_for(campaign) {
        let bytes = std::fs::read(expected.join(name))
            .map_err(|error| format!("read retained storage artifact {name}: {error}"))?;
        artifacts.insert(name.to_owned(), bytes);
    }
    for name in [
        "violations.json",
        "coverage.json",
        "episode.json",
        "storage-fixture.json",
    ] {
        zeppelin_embed_bench::harness_json::from_slice::<
            zeppelin_embed_bench::harness_json::Value,
        >(&artifacts[name])
        .map_err(|error| format!("parse retained storage artifact {name}: {error}"))?;
    }
    for name in [
        "program.jsonl",
        "faults.jsonl",
        "oracle.jsonl",
        "controls.jsonl",
        "receipts.jsonl",
        "mutations.jsonl",
        "ack-ledger.jsonl",
        "storage-observations.jsonl",
        "feature-receipts.jsonl",
        "clean-controls.jsonl",
        "artifact-index.jsonl",
    ] {
        for (index, line) in artifacts[name]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .map_err(|error| {
                format!(
                    "parse retained storage artifact {name} line {}: {error}",
                    index + 1
                )
            })?;
        }
    }
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["episode.json"])
            .map_err(|error| format!("parse retained storage episode: {error}"))?;
    if episode["campaign"].as_str() != Some(campaign.key()) {
        return Err("retained storage episode campaign differs".to_owned());
    }
    let seed = episode["seed"]
        .as_u64()
        .ok_or_else(|| "retained storage episode seed is absent".to_owned())?;
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["storage-fixture.json"])
            .map_err(|error| format!("parse retained storage fixture: {error}"))?;
    if fixture["seed"].as_u64() != Some(seed)
        || fixture["namespace"].as_str() != Some("adversarial::storage-durability::v1")
    {
        return Err("retained storage fixture identity differs".to_owned());
    }
    let operations = fixture["retained_operations"]
        .as_array()
        .ok_or_else(|| "retained storage fixture operations are absent".to_owned())?;
    if operations.len() < 5 {
        return Err(format!(
            "retained storage fixture omitted operation schedules: {}",
            operations.len()
        ));
    }
    let mut observed_operations = BTreeSet::new();
    for operation in operations {
        if operation["schema"].as_str()
            != Some(adversarial::storage_durability::STORAGE_RETAINED_FIXTURE_SCHEMA)
        {
            return Err("retained storage fixture schema differs".to_owned());
        }
        let retained = decode_storage_hex(
            operation["retained_fixture_hex"]
                .as_str()
                .ok_or_else(|| "retained storage fixture bytes are absent".to_owned())?,
        )?;
        let decoded = adversarial::storage_durability::decode_storage_fixture(&retained)?;
        if decoded.fixture.seed != seed
            || operation["operation"].as_str() != Some(decoded.operation.key())
            || operation["op_index"].as_u64() != Some(u64::from(decoded.op_index))
        {
            return Err("retained storage fixture decoded identity differs".to_owned());
        }
        observed_operations.insert(decoded.operation.key());
        let replayed = adversarial::storage_durability::run_storage_operation_from_fixture(
            &retained,
            adversarial::storage_durability::PUBLICATION_CHILD_TEST_NAME,
        )?;
        match replayed {
            adversarial::storage_durability::RetainedStorageOperationEvidence::WalPrefix(
                evidence,
            ) => {
                zeppelin_embed_adversarial_oracle::storage_durability::check_i16(
                    &evidence.expected,
                    &evidence.observed,
                )
                .map_err(|error| format!("retained storage I16 checker failed: {error:?}"))?;
                if decoded.format_case.is_some() {
                    let (expected, observed) =
                        adversarial::storage_durability::format_dtos_from_wal_prefix(&evidence)?;
                    zeppelin_embed_adversarial_oracle::storage_durability::check_i18(
                        &expected, &observed,
                    )
                    .map_err(|error| format!("retained storage I18 checker failed: {error:?}"))?;
                }
            }
            adversarial::storage_durability::RetainedStorageOperationEvidence::Publication(
                evidence,
            ) => zeppelin_embed_adversarial_oracle::storage_durability::check_i15(
                &evidence.expected,
                &evidence.observed,
            )
            .map_err(|error| format!("retained storage I15 checker failed: {error:?}"))?,
            adversarial::storage_durability::RetainedStorageOperationEvidence::Retry(evidence) => {
                zeppelin_embed_adversarial_oracle::storage_durability::check_i17(
                    &evidence.expected,
                    &evidence.observed,
                )
                .map_err(|error| format!("retained storage I17 checker failed: {error:?}"))?;
            }
            adversarial::storage_durability::RetainedStorageOperationEvidence::FormatCheck(
                evidence,
            ) => zeppelin_embed_adversarial_oracle::storage_durability::check_i18(
                &evidence.expected,
                &evidence.observed,
            )
            .map_err(|error| format!("retained storage I18 checker failed: {error:?}"))?,
            adversarial::storage_durability::RetainedStorageOperationEvidence::OrphanCleanup(
                evidence,
            ) => zeppelin_embed_adversarial_oracle::storage_durability::check_i19(
                &evidence.expected,
                &evidence.observed,
            )
            .map_err(|error| format!("retained storage I19 checker failed: {error:?}"))?,
        }
    }
    let expected_operations = adversarial::storage_durability::STORAGE_OPERATION_KINDS
        .into_iter()
        .map(adversarial::storage_durability::StorageOperationKind::key)
        .collect::<BTreeSet<_>>();
    if observed_operations != expected_operations {
        return Err(format!(
            "retained storage fixture operation set differs expected={expected_operations:?} observed={observed_operations:?}"
        ));
    }
    let expected_operations = adversarial::storage_durability::STORAGE_OPERATION_KINDS
        .iter()
        .map(|operation| operation.key())
        .collect::<BTreeSet<_>>();
    if observed_operations != expected_operations {
        return Err(format!(
            "retained storage fixture operation inventory differs expected={expected_operations:?} observed={observed_operations:?}"
        ));
    }
    for line in artifacts["oracle.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse retained storage oracle row: {error}"))?;
        validate_feature_oracle_record(campaign, &record)
            .map_err(|error| format!("replay drifted artifact=oracle.jsonl: {error}"))?;
    }
    let mut evidence = adversarial::artifacts::REPLAY_ARTIFACTS
        .iter()
        .map(|name| artifacts[*name].as_slice())
        .collect::<Vec<_>>();
    let mut family_names = adversarial::artifacts::STORAGE_REPLAY_ARTIFACTS.to_vec();
    family_names.sort_unstable();
    evidence.extend(family_names.iter().map(|name| artifacts[*name].as_slice()));
    Ok(adversarial::artifacts::evidence_digest(&evidence))
}

fn replay_metadata_retained_episode(expected: &Path) -> Result<String, String> {
    let campaign = CampaignKind::MetadataFilterPlanner;
    let mut artifacts = BTreeMap::<String, Vec<u8>>::new();
    for name in adversarial::artifacts::replay_artifacts_for(campaign) {
        let bytes = std::fs::read(expected.join(name))
            .map_err(|error| format!("read retained metadata artifact {name}: {error}"))?;
        artifacts.insert(name.to_owned(), bytes);
    }
    for name in [
        "violations.json",
        "coverage.json",
        "episode.json",
        "metadata-fixture.json",
    ] {
        zeppelin_embed_bench::harness_json::from_slice::<
            zeppelin_embed_bench::harness_json::Value,
        >(&artifacts[name])
        .map_err(|error| format!("parse retained metadata artifact {name}: {error}"))?;
    }
    for name in [
        "program.jsonl",
        "faults.jsonl",
        "oracle.jsonl",
        "controls.jsonl",
        "receipts.jsonl",
        "mutations.jsonl",
        "queries.jsonl",
        "fixture-mutations.jsonl",
    ] {
        for (index, line) in artifacts[name]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .map_err(|error| {
                format!(
                    "parse retained metadata artifact {name} line {}: {error}",
                    index + 1
                )
            })?;
        }
    }
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["episode.json"])
            .map_err(|error| format!("parse retained metadata episode: {error}"))?;
    if episode["campaign"].as_str() != Some(campaign.key()) {
        return Err("retained metadata episode campaign differs".to_owned());
    }
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["metadata-fixture.json"])
            .map_err(|error| format!("parse retained metadata fixture: {error}"))?;
    let operations = fixture["operations"]
        .as_array()
        .ok_or_else(|| "retained metadata fixture operations are absent".to_owned())?;
    let expected_operation_count =
        usize::try_from(adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT)
            .map_err(|_| "metadata I37 case count exceeds usize".to_owned())?
            .checked_add(3)
            .ok_or_else(|| "metadata retained operation count overflowed".to_owned())?;
    if operations.len() < expected_operation_count {
        return Err(format!(
            "retained metadata fixture operation count is incomplete expected-at-least={expected_operation_count} observed={}",
            operations.len()
        ));
    }

    let to_hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let mut retained_comparisons = BTreeMap::<(String, String, String), usize>::new();
    for operation in operations {
        if operation["retained_fixture_schema"].as_str()
            != Some(adversarial::metadata_filter_planner::METADATA_RETAINED_FIXTURE_SCHEMA)
        {
            return Err("retained metadata fixture schema differs".to_owned());
        }
        let retained_hex = operation["retained_fixture_hex"]
            .as_str()
            .ok_or_else(|| "retained metadata fixture bytes are absent".to_owned())?;
        let retained = decode_storage_hex(retained_hex)
            .map_err(|error| format!("decode retained metadata fixture: {error}"))?;
        if operation["retained_fixture_bytes"].as_u64() != Some(retained.len() as u64) {
            return Err("retained metadata fixture byte length differs".to_owned());
        }
        let decoded = adversarial::metadata_filter_planner::decode_metadata_fixture(&retained)?;
        if operation["seed"].as_u64() != Some(decoded.seed) {
            return Err("retained metadata fixture seed identity differs".to_owned());
        }
        let expected_operation = match decoded.operation {
            adversarial::metadata_filter_planner::MetadataOperationKind::Columns => {
                "metadata_columns_roundtrip"
            }
            adversarial::metadata_filter_planner::MetadataOperationKind::Bitmap => {
                "metadata_bitmap_algebra"
            }
            adversarial::metadata_filter_planner::MetadataOperationKind::Planner => {
                "metadata_pruning_soundness"
            }
            adversarial::metadata_filter_planner::MetadataOperationKind::Execution => {
                "metadata_execution_truth"
            }
        };
        if operation["operation"].as_str() != Some(expected_operation) {
            return Err("retained metadata fixture operation identity differs".to_owned());
        }
        let replayed =
            adversarial::metadata_filter_planner::run_metadata_operation_from_fixture(&retained)?;
        if replayed.replay.first_difference.is_some() {
            return Err(format!(
                "retained metadata fixture checker {} failed",
                replayed.replay.checker_id
            ));
        }
        let key = (
            replayed.replay.checker_id.to_owned(),
            to_hex(&decoded.input_bytes),
            to_hex(&decoded.observed_bytes),
        );
        let count = retained_comparisons.entry(key).or_default();
        *count = count.saturating_add(1);
    }
    for line in artifacts["oracle.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse retained metadata oracle row: {error}"))?;
        validate_feature_oracle_record(campaign, &record)?;
        let key = (
            record["checker_id"]
                .as_str()
                .ok_or_else(|| "retained metadata oracle checker is absent".to_owned())?
                .to_owned(),
            record["oracle_input_bytes"]
                .as_str()
                .ok_or_else(|| "retained metadata oracle input bytes are absent".to_owned())?
                .to_owned(),
            record["oracle_observed_bytes"]
                .as_str()
                .ok_or_else(|| "retained metadata oracle observed bytes are absent".to_owned())?
                .to_owned(),
        );
        let count = retained_comparisons.get_mut(&key).ok_or_else(|| {
            format!(
                "retained metadata oracle row has no literal fixture checker={}",
                key.0
            )
        })?;
        *count = count.saturating_sub(1);
        if *count == 0 {
            retained_comparisons.remove(&key);
        }
    }
    if !retained_comparisons.is_empty() {
        return Err(format!(
            "retained metadata fixture comparisons are absent from oracle stream: {}",
            retained_comparisons.values().copied().sum::<usize>()
        ));
    }
    let mut family_names = adversarial::artifacts::METADATA_REPLAY_ARTIFACTS.to_vec();
    family_names.sort_unstable();
    let family_evidence = family_names
        .iter()
        .map(|name| artifacts[*name].as_slice())
        .collect::<Vec<_>>();
    let mut operation_evidence = vec![
        artifacts["program.jsonl"].as_slice(),
        artifacts["controls.jsonl"].as_slice(),
    ];
    operation_evidence.extend(family_evidence.iter().copied());
    let mut fault_evidence = vec![
        artifacts["faults.jsonl"].as_slice(),
        artifacts["receipts.jsonl"].as_slice(),
        artifacts["mutations.jsonl"].as_slice(),
    ];
    fault_evidence.extend(family_evidence.iter().copied());
    let evidence_digests = &episode["attestation"]["evidence_digests"];
    for (name, observed) in [
        (
            "operation_evidence",
            adversarial::artifacts::evidence_digest(&operation_evidence),
        ),
        (
            "checker_evidence",
            adversarial::artifacts::evidence_digest(&[&artifacts["oracle.jsonl"]]),
        ),
        (
            "fault_evidence",
            adversarial::artifacts::evidence_digest(&fault_evidence),
        ),
    ] {
        if evidence_digests[name].as_str() != Some(observed.as_str()) {
            return Err(format!("retained metadata {name} digest differs"));
        }
    }
    if adversarial::campaign::campaign_from_replay_metadata(expected)? != campaign {
        return Err("retained metadata replay campaign differs".to_owned());
    }

    let mut evidence = adversarial::artifacts::REPLAY_ARTIFACTS
        .iter()
        .map(|name| artifacts[*name].as_slice())
        .collect::<Vec<_>>();
    evidence.extend(family_names.iter().map(|name| artifacts[*name].as_slice()));
    Ok(adversarial::artifacts::evidence_digest(&evidence))
}

fn replay_ingest_retained_episode(expected: &Path) -> Result<String, String> {
    let campaign = CampaignKind::IngestRetention;
    let mut artifacts = BTreeMap::<String, Vec<u8>>::new();
    for name in adversarial::artifacts::replay_artifacts_for(campaign) {
        let bytes = std::fs::read(expected.join(name))
            .map_err(|error| format!("read retained ingest artifact {name}: {error}"))?;
        artifacts.insert(name.to_owned(), bytes);
    }
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["episode.json"])
            .map_err(|error| format!("parse retained ingest episode: {error}"))?;
    if episode["campaign"].as_str() != Some(campaign.key()) {
        return Err("retained ingest episode campaign differs".to_owned());
    }
    let seed = episode["seed"]
        .as_u64()
        .ok_or_else(|| "retained ingest episode seed is absent".to_owned())?;
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["fixture.json"])
            .map_err(|error| format!("parse retained ingest fixture: {error}"))?;
    if fixture["campaign"].as_str() != Some(campaign.key()) {
        return Err("retained ingest fixture campaign differs".to_owned());
    }
    let operations = fixture["operations"]
        .as_array()
        .ok_or_else(|| "retained ingest fixture operations are absent".to_owned())?;
    if operations.len() < 4 {
        return Err(format!(
            "retained ingest fixture omitted a required operation: {} records",
            operations.len()
        ));
    }
    let observation_lines = artifacts["observations.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if observation_lines.len() != operations.len() {
        return Err(format!(
            "retained ingest observations count differs operations={} observations={}",
            operations.len(),
            observation_lines.len()
        ));
    }
    let mut observed_operations = BTreeSet::new();
    let mut observed_cases = BTreeSet::new();
    let mut replayed_observations = Vec::new();
    let mut replayed_controls = Vec::new();
    let mut replayed_receipts = Vec::new();
    for (index, operation_record) in operations.iter().enumerate() {
        if operation_record["schema"].as_str()
            != Some(adversarial::ingest_retention::INGEST_RETAINED_FIXTURE_SCHEMA)
        {
            return Err("retained ingest fixture schema differs".to_owned());
        }
        let retained_bytes = decode_storage_hex(
            operation_record["retained_fixture_hex"]
                .as_str()
                .ok_or_else(|| "retained ingest fixture bytes are absent".to_owned())?,
        )?;
        let decoded = adversarial::ingest_retention::decode_ingest_fixture(&retained_bytes)?;
        if operation_record["seed"].as_u64() != Some(seed)
            || operation_record["operation"].as_str() != Some(decoded.operation.key())
            || operation_record["invocation_id"].as_u64() != Some(decoded.invocation_id)
        {
            return Err("retained ingest fixture decoded identity differs".to_owned());
        }
        let expected_record =
            adversarial::ingest_retention::retained_ingest_fixture_record_json(&decoded)?;
        let expected_record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&expected_record)
                .map_err(|error| format!("parse replayed ingest fixture record: {error}"))?;
        if &expected_record != operation_record {
            return Err(format!(
                "retained ingest fixture record differs for {}",
                decoded.operation.key()
            ));
        }
        observed_operations.insert(decoded.operation.key());
        let case_identity = (
            decoded.operation.key().to_owned(),
            decoded.fault.map(|fault| fault.key().to_owned()),
            decoded.invocation_id,
        );
        if !observed_cases.insert(case_identity) {
            return Err(format!(
                "retained ingest fixture duplicated operation/fault/invocation case {}",
                decoded.operation.key()
            ));
        }
        let replayed =
            adversarial::ingest_retention::run_ingest_operation_from_fixture(&retained_bytes)?;
        match &replayed {
            adversarial::ingest_retention::IngestOperationEvidence::I20(evidence) => {
                zeppelin_embed_adversarial_oracle::ingest_retention::compare_i20(
                    &evidence.expected,
                    &evidence.observed,
                )?;
            }
            adversarial::ingest_retention::IngestOperationEvidence::I21(evidence) => {
                zeppelin_embed_adversarial_oracle::ingest_retention::compare_i21(
                    &evidence.expected,
                    &evidence.observed,
                )?;
            }
            adversarial::ingest_retention::IngestOperationEvidence::I22(evidence) => {
                zeppelin_embed_adversarial_oracle::ingest_retention::compare_i22(
                    &evidence.expected,
                    &evidence.observed,
                )?;
            }
            adversarial::ingest_retention::IngestOperationEvidence::I23(evidence) => {
                zeppelin_embed_adversarial_oracle::ingest_retention::compare_i23(
                    &evidence.expected,
                    &evidence.observed,
                )?;
            }
        }
        let replayed_observation =
            adversarial::ingest_retention::retained_ingest_observation_json(&decoded, &replayed)?;
        if replayed_observation.as_bytes() != observation_lines[index] {
            return Err(format!(
                "replay drifted artifact=observations.jsonl operation={}",
                decoded.operation.key()
            ));
        }
        let observation: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&replayed_observation)
                .map_err(|error| format!("parse replayed ingest observation: {error}"))?;
        replayed_observations.push(observation);
        if let Some(control) = adversarial::runner::ingest_retention_control_json_for_evidence(
            seed,
            decoded.operation,
            decoded.fault,
            &replayed,
        )? {
            replayed_controls.push(control);
        }
        let receipts = match &replayed {
            adversarial::ingest_retention::IngestOperationEvidence::I20(evidence) => {
                &evidence.receipts
            }
            adversarial::ingest_retention::IngestOperationEvidence::I21(evidence) => {
                &evidence.receipts
            }
            adversarial::ingest_retention::IngestOperationEvidence::I22(evidence) => {
                &evidence.receipts
            }
            adversarial::ingest_retention::IngestOperationEvidence::I23(evidence) => {
                &evidence.receipts
            }
        };
        replayed_receipts.extend(
            receipts
                .iter()
                .map(adversarial::runner::ingest_retention_receipt_json),
        );
    }
    let expected_operations = ["batch-commit", "seal", "retention", "purge"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if observed_operations != expected_operations {
        return Err(format!(
            "retained ingest operation inventory differs expected={expected_operations:?} observed={observed_operations:?}"
        ));
    }
    let render_jsonl = |records: &[String]| {
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(record.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    };
    if render_jsonl(&replayed_controls) != artifacts["controls.jsonl"] {
        return Err("replay drifted artifact=controls.jsonl".to_owned());
    }
    if render_jsonl(&replayed_receipts) != artifacts["receipts.jsonl"] {
        return Err("replay drifted artifact=receipts.jsonl".to_owned());
    }
    let mut oracle_count = 0_usize;
    for (index, line) in artifacts["oracle.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse retained ingest oracle row: {error}"))?;
        validate_feature_oracle_record(campaign, &record)
            .map_err(|error| format!("replay drifted artifact=oracle.jsonl: {error}"))?;
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "retained ingest oracle row omitted operation".to_owned())?;
        let observation = replayed_observations.get(index).ok_or_else(|| {
            format!("retained ingest oracle row has no replayed case at index {index}")
        })?;
        if observation["operation"].as_str() != Some(operation) {
            return Err(format!(
                "retained ingest oracle row operation differs from replayed case at index {index}"
            ));
        }
        if record["checker_id"] != observation["checker_id"]
            || record["canonical_version"] != observation["canonical_version"]
            || record["oracle_input_digest"] != observation["oracle_input_digest"]
            || record["oracle_observed_digest"] != observation["oracle_observed_digest"]
            || record["oracle_input_bytes"] != observation["oracle_input_bytes"]
            || record["oracle_observed_bytes"] != observation["oracle_observed_bytes"]
            || record["passed"].as_bool() != Some(true)
        {
            return Err(format!(
                "replay drifted artifact=oracle.jsonl operation={operation}"
            ));
        }
        oracle_count = oracle_count.saturating_add(1);
    }
    if oracle_count != operations.len() {
        return Err(format!(
            "retained ingest oracle comparison count differs operations={} comparisons={oracle_count}",
            operations.len()
        ));
    }
    if adversarial::campaign::campaign_from_replay_metadata(expected)? != campaign {
        return Err("retained ingest replay metadata campaign differs".to_owned());
    }
    let mut evidence = adversarial::artifacts::REPLAY_ARTIFACTS
        .iter()
        .map(|name| artifacts[*name].as_slice())
        .collect::<Vec<_>>();
    let mut family_names = adversarial::artifacts::INGEST_REPLAY_ARTIFACTS.to_vec();
    family_names.sort_unstable();
    evidence.extend(family_names.iter().map(|name| artifacts[*name].as_slice()));
    Ok(adversarial::artifacts::evidence_digest(&evidence))
}

fn replay_vector_retained_episode(expected: &Path) -> Result<String, String> {
    let campaign = CampaignKind::VectorExecution;
    let mut artifacts = BTreeMap::<String, Vec<u8>>::new();
    for name in adversarial::artifacts::replay_artifacts_for(campaign) {
        let bytes = std::fs::read(expected.join(name))
            .map_err(|error| format!("read retained vector artifact {name}: {error}"))?;
        artifacts.insert(name.to_owned(), bytes);
    }
    for name in [
        "violations.json",
        "coverage.json",
        "episode.json",
        "fixture.json",
        "backend-inventory.json",
        "episode-summary.json",
    ] {
        zeppelin_embed_bench::harness_json::from_slice::<
            zeppelin_embed_bench::harness_json::Value,
        >(&artifacts[name])
        .map_err(|error| format!("parse retained vector artifact {name}: {error}"))?;
    }
    for name in [
        "program.jsonl",
        "faults.jsonl",
        "oracle.jsonl",
        "controls.jsonl",
        "receipts.jsonl",
        "mutations.jsonl",
        "quantization.jsonl",
        "rescore.jsonl",
        "identity.jsonl",
        "coverage.jsonl",
        "violations.jsonl",
    ] {
        for (index, line) in artifacts[name]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .map_err(|error| {
                format!(
                    "parse retained vector artifact {name} line {}: {error}",
                    index + 1
                )
            })?;
        }
    }
    if artifacts["episode-summary.json"] != artifacts["episode.json"] {
        return Err(
            "retained vector episode summary differs byte-for-byte from episode.json".to_owned(),
        );
    }
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["episode.json"])
            .map_err(|error| format!("parse retained vector episode: {error}"))?;
    if episode["campaign"].as_str() != Some(campaign.key()) {
        return Err("retained vector episode campaign differs".to_owned());
    }
    let seed = episode["seed"]
        .as_u64()
        .ok_or_else(|| "retained vector episode seed is absent".to_owned())?;
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&artifacts["fixture.json"])
            .map_err(|error| format!("parse retained vector fixture: {error}"))?;
    let operations = fixture["operations"]
        .as_array()
        .ok_or_else(|| "retained vector fixture operations are absent".to_owned())?;
    if operations.len() < 4 {
        return Err(format!(
            "retained vector fixture operation count differs: {}",
            operations.len()
        ));
    }
    let mut retained_comparisons = BTreeMap::<(String, u64), Vec<(String, String)>>::new();
    for operation in operations {
        if operation["campaign"].as_str() != Some(campaign.key())
            || operation["namespace"].as_str() != Some("vector-execution-v1")
            || operation["seed"].as_u64() != Some(seed)
            || operation["retained_fixture_version"].as_str()
                != Some(adversarial::vector_execution::VECTOR_FIXTURE_CODEC_VERSION)
        {
            return Err("retained vector fixture operation identity differs".to_owned());
        }
        let retained_hex = operation["retained_fixture_hex"]
            .as_str()
            .ok_or_else(|| "retained vector fixture bytes are absent".to_owned())?;
        let retained = decode_storage_hex(retained_hex)
            .map_err(|error| format!("decode retained vector fixture: {error}"))?;
        if operation["retained_fixture_bytes"].as_u64() != Some(retained.len() as u64) {
            return Err("retained vector fixture byte length differs".to_owned());
        }
        let replayed = adversarial::vector_execution::run_vector_operation_from_fixture(&retained)?;
        if replayed.fixture.seed != seed
            || operation["operation"].as_str() != Some(replayed.fixture.operation.key())
        {
            return Err("retained vector fixture decoded identity differs".to_owned());
        }
        let sha256 = replayed
            .fixture
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if operation["retained_fixture_sha256"].as_str() != Some(sha256.as_str()) {
            return Err("retained vector fixture SHA-256 ledger differs".to_owned());
        }
        for (literal, comparison) in replayed
            .fixture
            .comparisons
            .iter()
            .zip(&replayed.comparisons)
        {
            if comparison.first_difference.is_some() {
                return Err(format!(
                    "retained vector fixture checker {} case {} failed",
                    comparison.checker_id, comparison.case_id
                ));
            }
            let key = (comparison.checker_id.to_owned(), comparison.case_id);
            let input = literal
                .input_bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let observed = literal
                .observed_bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            retained_comparisons
                .entry(key)
                .or_default()
                .push((input, observed));
        }
    }
    for line in artifacts["oracle.jsonl"]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse retained vector oracle row: {error}"))?;
        validate_feature_oracle_record(campaign, &record)?;
        let input_hex = record["oracle_input_bytes"]
            .as_str()
            .ok_or_else(|| "retained vector oracle row omitted input bytes".to_owned())?;
        let observed_hex = record["oracle_observed_bytes"]
            .as_str()
            .ok_or_else(|| "retained vector oracle row omitted observed bytes".to_owned())?;
        let replayed =
            zeppelin_embed_adversarial_oracle::vector_execution::replay_canonical_comparison(
                &decode_storage_hex(input_hex)?,
                &decode_storage_hex(observed_hex)?,
            )?;
        let key = (replayed.checker_id.to_owned(), replayed.case_id);
        let literals = retained_comparisons.get_mut(&key).ok_or_else(|| {
            format!(
                "retained vector oracle row has no fixture comparison checker={} case={}",
                key.0, key.1
            )
        })?;
        let Some(position) = literals
            .iter()
            .position(|literal| literal.0 == input_hex && literal.1 == observed_hex)
        else {
            return Err(format!(
                "retained vector oracle bytes differ from fixture checker={} case={}",
                key.0, key.1
            ));
        };
        literals.remove(position);
        if literals.is_empty() {
            retained_comparisons.remove(&key);
        }
    }
    if !retained_comparisons.is_empty() {
        return Err(format!(
            "retained vector fixture comparisons omitted from oracle stream: {:?}",
            retained_comparisons.keys().collect::<Vec<_>>()
        ));
    }

    let mut family_names = adversarial::artifacts::VECTOR_REPLAY_ARTIFACTS
        .into_iter()
        .filter(|name| *name != "episode-summary.json")
        .collect::<Vec<_>>();
    family_names.sort_unstable();
    let family_evidence = family_names
        .iter()
        .map(|name| artifacts[*name].as_slice())
        .collect::<Vec<_>>();
    let mut operation_evidence = vec![
        artifacts["program.jsonl"].as_slice(),
        artifacts["controls.jsonl"].as_slice(),
    ];
    operation_evidence.extend(family_evidence.iter().copied());
    let mut fault_evidence = vec![
        artifacts["faults.jsonl"].as_slice(),
        artifacts["receipts.jsonl"].as_slice(),
        artifacts["mutations.jsonl"].as_slice(),
    ];
    fault_evidence.extend(family_evidence.iter().copied());
    let evidence_digests = &episode["attestation"]["evidence_digests"];
    for (name, observed) in [
        (
            "operation_evidence",
            adversarial::artifacts::evidence_digest(&operation_evidence),
        ),
        (
            "checker_evidence",
            adversarial::artifacts::evidence_digest(&[&artifacts["oracle.jsonl"]]),
        ),
        (
            "fault_evidence",
            adversarial::artifacts::evidence_digest(&fault_evidence),
        ),
    ] {
        if evidence_digests[name].as_str() != Some(observed.as_str()) {
            return Err(format!("retained vector {name} digest differs"));
        }
    }
    if adversarial::campaign::campaign_from_replay_metadata(expected)? != campaign {
        return Err("retained vector replay metadata campaign differs".to_owned());
    }

    let mut evidence = vec![
        artifacts["program.jsonl"].as_slice(),
        artifacts["faults.jsonl"].as_slice(),
        artifacts["violations.json"].as_slice(),
        artifacts["coverage.json"].as_slice(),
        artifacts["oracle.jsonl"].as_slice(),
        artifacts["controls.jsonl"].as_slice(),
        artifacts["receipts.jsonl"].as_slice(),
        artifacts["mutations.jsonl"].as_slice(),
        artifacts["episode.json"].as_slice(),
    ];
    let mut all_family_names = adversarial::artifacts::VECTOR_REPLAY_ARTIFACTS.to_vec();
    all_family_names.sort_unstable();
    evidence.extend(
        all_family_names
            .iter()
            .map(|name| artifacts[*name].as_slice()),
    );
    Ok(adversarial::artifacts::evidence_digest(&evidence))
}

fn replay_campaign_episode(
    root: &Path,
    campaign: CampaignKind,
    outcome: &adversarial::runner::RunOutcome,
) -> Result<(), String> {
    if campaign == CampaignKind::Overall {
        return Ok(());
    }
    let expected = episode_artifact_directory(root, campaign, outcome.seed, outcome.profile);
    let expected_digest = replay_evidence_digest(outcome);
    let observed_digest = match campaign {
        CampaignKind::StorageDurability => {
            #[cfg(unix)]
            {
                replay_storage_retained_episode(&expected)?
            }
            #[cfg(not(unix))]
            {
                return Err("retained storage replay requires Unix".to_owned());
            }
        }
        CampaignKind::VectorExecution => replay_vector_retained_episode(&expected)?,
        CampaignKind::MetadataFilterPlanner => replay_metadata_retained_episode(&expected)?,
        CampaignKind::IngestRetention => replay_ingest_retained_episode(&expected)?,
        _ => {
            let actual_root = tempfile::tempdir()
                .map_err(|error| format!("create feature replay root: {error}"))?;
            let replayed = adversarial::runner::run_program_for(
                campaign,
                outcome.seed,
                outcome.profile,
                actual_root.path(),
            )?;
            compare_replay_artifacts(&expected, campaign, &replayed)?;
            replay_evidence_digest(&replayed)
        }
    };
    let row = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-replay",
        "version": 1,
        "campaign": campaign.key(),
        "seed": outcome.seed,
        "profile": outcome.profile.key(),
        "artifact_count": adversarial::artifacts::replay_artifacts_for(campaign).len(),
        "evidence_digest": expected_digest,
        "expected_digest": expected_digest,
        "observed_digest": observed_digest,
    });
    let mut bytes = zeppelin_embed_bench::harness_json::to_vec(&row)
        .map_err(|error| format!("serialize feature replay ledger row: {error}"))?;
    bytes.push(b'\n');
    let path = root.join("replayed-seeds.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("append {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    std::fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync replay ledger directory {}: {error}", root.display()))
}

#[test]
fn storage_replay_compares_every_evidence_stream() {
    let campaign = CampaignKind::StorageDurability;
    let seed = 6;
    let expected_root = tempfile::tempdir().expect("storage expected replay root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, expected_root.path())
        .expect("storage expected replay episode");
    let expected_directory = expected_root
        .path()
        .join(format!("storage-durability/seed-{seed}-none"));
    let actual_root = tempfile::tempdir().expect("storage actual replay root");
    let actual = adversarial::runner::run_program_for(
        campaign,
        seed,
        FaultProfile::None,
        actual_root.path(),
    )
    .expect("storage actual replay episode");
    compare_replay_artifacts(&expected_directory, campaign, &actual)
        .expect("regenerated storage replay evidence");

    let fixture_path = expected_directory.join("storage-fixture.json");
    let mut planted = std::fs::read(&fixture_path).expect("storage fixture evidence");
    planted.push(b' ');
    std::fs::write(&fixture_path, planted).expect("plant storage fixture replay mismatch");
    let error = compare_replay_artifacts(&expected_directory, campaign, &actual)
        .expect_err("mutated storage-fixture.json was not compared");
    assert!(error.contains("storage-fixture.json"), "{error}");
    assert!(error.contains("byte_offset="), "{error}");
    assert!(error.contains("line="), "{error}");
    assert!(error.contains("column="), "{error}");
    assert!(error.contains("expected=0x20"), "{error}");
    assert!(error.contains("observed=<eof>"), "{error}");
}

#[cfg(unix)]
#[test]
fn storage_replay_executes_the_retained_literal_fixture() {
    let campaign = CampaignKind::StorageDurability;
    let seed = 6;
    let root = tempfile::tempdir().expect("storage retained replay root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("storage retained replay episode");
    let directory = root
        .path()
        .join(format!("storage-durability/seed-{seed}-none"));
    let replayed = replay_storage_retained_episode(&directory)
        .expect("execute retained storage operation fixtures");
    assert_eq!(replayed, replay_evidence_digest(&outcome));

    let fixture_path = directory.join("storage-fixture.json");
    let mut fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(&fixture_path).expect("read retained storage fixture"),
        )
        .expect("parse retained storage fixture");
    let retained = fixture["retained_operations"][0]["retained_fixture_hex"]
        .as_str()
        .expect("retained storage fixture hex");
    let mut planted = retained.as_bytes().to_vec();
    planted[32] = if planted[32] == b'0' { b'1' } else { b'0' };
    fixture["retained_operations"][0]["retained_fixture_hex"] =
        zeppelin_embed_bench::harness_json::Value::String(
            String::from_utf8(planted).expect("planted storage fixture remains hex"),
        );
    std::fs::write(
        &fixture_path,
        zeppelin_embed_bench::harness_json::to_vec(&fixture)
            .expect("serialize planted storage fixture"),
    )
    .expect("write planted storage fixture");
    let error = replay_storage_retained_episode(&directory)
        .expect_err("mutated retained storage fixture was accepted");
    assert!(error.contains("fixture"), "{error}");
}

#[test]
fn vector_replay_compares_every_artifact() {
    let campaign = CampaignKind::VectorExecution;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("vector campaign has a clean feature-fault slot");
    let expected_root = tempfile::tempdir().expect("vector expected replay root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, expected_root.path())
        .expect("vector expected replay episode");
    let expected_directory = expected_root
        .path()
        .join(format!("vector-execution/seed-{seed}-none"));
    let actual_root = tempfile::tempdir().expect("vector actual replay root");
    let actual = adversarial::runner::run_program_for(
        campaign,
        seed,
        FaultProfile::None,
        actual_root.path(),
    )
    .expect("vector actual replay episode");
    compare_replay_artifacts(&expected_directory, campaign, &actual)
        .expect("regenerated vector replay evidence");

    let inventory_path = expected_directory.join("backend-inventory.json");
    let mut planted = std::fs::read(&inventory_path).expect("vector backend inventory");
    planted.push(b' ');
    std::fs::write(&inventory_path, planted).expect("plant vector replay mismatch");
    let error = compare_replay_artifacts(&expected_directory, campaign, &actual)
        .expect_err("mutated backend-inventory.json was not compared");
    assert!(error.contains("backend-inventory.json"), "{error}");
}

#[test]
#[ignore = "manual release campaign: at least eight hours and 10,000 episodes"]
fn campaign() {
    let test_mode = std::env::var("ZE_ADV_CAMPAIGN_TEST_MODE").as_deref() == Ok("1");
    let config = RunConfig::from_env().expect("valid adversarial campaign configuration");
    config
        .validate_campaign_thresholds(test_mode)
        .expect("valid adversarial campaign thresholds");
    let root = config.artifacts.clone();
    std::fs::create_dir_all(&root).expect("campaign artifact root");
    let started = Instant::now();
    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_secs();
    let injected_failures = campaign_test_failures(test_mode);
    let mut seed = config.start_seed;
    let mut episodes = 0_u64;
    let mut operations = 0_u64;
    let mut faults_fired = 0_u64;
    let mut violations = 0_u64;
    let mut execution_errors = 0_u64;
    let mut panics = 0_u64;
    let mut unfired_scheduled_faults = 0_u64;
    let mut generic_multi_event_episodes = 0_u64;
    let mut failures = Vec::<CampaignFailure>::new();
    let mut successful_artifacts = VecDeque::<PathBuf>::new();
    let mut coverage = CoverageRegistry::default();
    let mut attestation_counters = CampaignAttestationCounters::default();
    let mut merged_evidence = if config.campaign == CampaignKind::Overall {
        None
    } else {
        Some(
            adversarial::artifacts::MergedEvidence::create(&root)
                .expect("create fresh merged feature evidence"),
        )
    };
    let required_duration = Duration::from_secs(config.minimum_seconds);

    while episodes < config.minimum_episodes || started.elapsed() < required_duration {
        let profile = if std::env::var_os("ZE_ADV_PROFILE").is_some() {
            config.profile
        } else {
            profile_for_seed(seed)
        };
        let injected = injected_failures.get(&seed).copied();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                injected != Some(CampaignInjectedFailure::Panic),
                "test-mode injected episode panic"
            );
            adversarial::runner::run_program_for(config.campaign, seed, profile, &root)
        }));
        let result = match result {
            Ok(result) => result,
            Err(payload) => {
                panics = panics.saturating_add(1);
                Err(format!("runner panicked: {}", panic_detail(&payload)))
            }
        };
        episodes = episodes.saturating_add(1);
        let failure = match result {
            Ok(_) if injected == Some(CampaignInjectedFailure::ExecutionError) => {
                execution_errors = execution_errors.saturating_add(1);
                Some((
                    CampaignFailureKind::ExecutionError,
                    "test-mode injected runner error after artifact production".to_owned(),
                ))
            }
            Ok(outcome) => {
                operations = operations.saturating_add(outcome.operations as u64);
                faults_fired = faults_fired.saturating_add(outcome.faults_fired as u64);
                generic_multi_event_episodes = generic_multi_event_episodes
                    .saturating_add(u64::from(outcome.scheduled_faults_fired >= 2));
                coverage.merge(&outcome.coverage);
                let merge_error = merged_evidence.as_mut().and_then(|merged| {
                    merged
                        .append_episode(
                            outcome.seed,
                            outcome.profile,
                            &outcome.program_bytes,
                            &outcome.faults_bytes,
                            &outcome.violations_bytes,
                            &outcome.coverage_bytes,
                            &outcome.oracle_bytes,
                            &outcome.controls_bytes,
                            &outcome.receipts_bytes,
                            &outcome.mutations_bytes,
                            &outcome.family_artifact_bytes,
                        )
                        .err()
                });
                if let Some(error) = merge_error {
                    execution_errors = execution_errors.saturating_add(1);
                    Some((
                        CampaignFailureKind::ExecutionError,
                        format!("merged evidence append failed: {error}"),
                    ))
                } else {
                    attestation_counters.merge(&outcome);
                    let replay_error = if config.campaign == CampaignKind::Overall {
                        None
                    } else {
                        match replay_campaign_episode(&root, config.campaign, &outcome) {
                            Ok(()) => {
                                attestation_counters.mark_replayed();
                                None
                            }
                            Err(error) => Some(error),
                        }
                    };
                    let episode_violations = (outcome.violations.len() as u64).saturating_add(
                        u64::from(injected == Some(CampaignInjectedFailure::Violation)),
                    );
                    violations = violations.saturating_add(episode_violations);
                    let scheduled_missing = !outcome.missing_feature_faults.is_empty()
                        || injected == Some(CampaignInjectedFailure::UnfiredScheduledFault);
                    unfired_scheduled_faults =
                        unfired_scheduled_faults.saturating_add(u64::from(scheduled_missing));
                    let episode_failure = match (episode_violations > 0, scheduled_missing) {
                        (false, false) => None,
                        (true, false) => Some((
                            CampaignFailureKind::InvariantViolation,
                            format!("{episode_violations} invariant violation(s)"),
                        )),
                        (false, true) => Some((
                            CampaignFailureKind::UnfiredScheduledFault,
                            "selected scheduled fault did not fire".to_owned(),
                        )),
                        (true, true) => Some((
                            CampaignFailureKind::ViolationAndUnfiredFault,
                            format!(
                                "{episode_violations} invariant violation(s) and the selected scheduled fault did not fire"
                            ),
                        )),
                    };
                    if let Some(error) = replay_error {
                        execution_errors = execution_errors.saturating_add(1);
                        let detail = episode_failure.map_or_else(
                            || format!("feature replay failed: {error}"),
                            |(_, detail)| format!("{detail}; feature replay failed: {error}"),
                        );
                        Some((CampaignFailureKind::ExecutionError, detail))
                    } else {
                        episode_failure
                    }
                }
            }
            Err(error) => {
                execution_errors = execution_errors.saturating_add(1);
                Some((CampaignFailureKind::ExecutionError, error))
            }
        };

        if let Some((kind, detail)) = failure {
            let artifact_directory =
                preserve_campaign_failure(&root, config.campaign, seed, profile, kind, &detail);
            println!(
                "ADV_CAMPAIGN_FAILURE seed={seed} profile={} campaign={} kind={} artifacts={}",
                profile.key(),
                config.campaign.key(),
                kind.key(),
                artifact_directory.display()
            );
            failures.push(CampaignFailure {
                seed,
                profile,
                kind,
                detail,
                artifact_directory,
            });
        } else {
            successful_artifacts.push_back(episode_artifact_directory(
                &root,
                config.campaign,
                seed,
                profile,
            ));
            while successful_artifacts.len() > config.retain_successful {
                let old_directory = successful_artifacts
                    .pop_front()
                    .expect("successful artifact queue is nonempty");
                if old_directory.is_dir() {
                    std::fs::remove_dir_all(&old_directory).unwrap_or_else(|error| {
                        panic!("rotate {}: {error}", old_directory.display())
                    });
                }
            }
        }

        write_campaign_summary(
            &root,
            &config,
            false,
            false,
            started_unix,
            started.elapsed(),
            episodes,
            operations,
            faults_fired,
            seed,
            &coverage,
            violations,
            execution_errors,
            panics,
            unfired_scheduled_faults,
            &failures,
            &attestation_counters,
            merged_evidence.as_ref().map(|merged| merged.stats()),
        );
        if episodes.is_multiple_of(100) {
            println!(
                "ADV_CAMPAIGN episodes={episodes} elapsed_seconds={:.1} seed={seed} profile={} operations={operations} faults={faults_fired} failed_episodes={}",
                started.elapsed().as_secs_f64(),
                profile.key(),
                failures.len()
            );
        }
        seed = seed.checked_add(1).expect("campaign seed exhausted u64");
    }

    let mut missing = missing_campaign_coverage(config.campaign, &coverage);
    missing.extend(empty_family_evidence_streams(
        config.campaign,
        merged_evidence
            .as_ref()
            .map(|merged| merged.stats())
            .as_ref(),
    ));
    let run_passed = failures.is_empty();
    let attestation_complete = config.campaign == CampaignKind::Overall
        || feature_attestation_complete(
            &config,
            episodes,
            &attestation_counters,
            merged_evidence.as_ref().map(|merged| merged.stats()),
        );
    let qualification_passed = run_passed
        && attestation_complete
        && (config.campaign == CampaignKind::Overall
            && config.qualification == Qualification::Exploratory
            || missing.is_empty());
    write_campaign_summary(
        &root,
        &config,
        true,
        qualification_passed,
        started_unix,
        started.elapsed(),
        episodes,
        operations,
        faults_fired,
        seed.saturating_sub(1),
        &coverage,
        violations,
        execution_errors,
        panics,
        unfired_scheduled_faults,
        &failures,
        &attestation_counters,
        merged_evidence.as_ref().map(|merged| merged.stats()),
    );
    validate_completed_campaign_summary(&root, &config);
    println!(
        "ADV_CAMPAIGN_COMPLETE episodes={episodes} qualification={} campaign={} elapsed_seconds={:.1} operations={operations} faults={faults_fired} failed_episodes={}",
        if qualification_passed {
            "passed"
        } else {
            "failed"
        },
        config.campaign.key(),
        started.elapsed().as_secs_f64(),
        failures.len()
    );
    // Exploratory runs are red only for a product failure or a broken
    // attestation. Missing coverage stays visible in the summary and in the
    // qualification flag, but it is not a failure of the run.
    let exploratory_passed = run_passed && attestation_complete;
    if config.qualification == Qualification::Exploratory && !missing.is_empty() {
        println!(
            "ADV_CAMPAIGN_COVERAGE_INCOMPLETE campaign={} missing={}",
            config.campaign.key(),
            missing.join(",")
        );
    }
    if std::env::var_os("ZE_ADV_PROFILE").is_none() && episodes >= 30 {
        assert!(
            generic_multi_event_episodes.saturating_mul(10) >= episodes.saturating_mul(3),
            "generic CAN-FIRE failed: {generic_multi_event_episodes}/{episodes} campaign episodes fired at least two scheduled faults; require >=30%"
        );
    }
    assert!(
        if config.qualification == Qualification::Exploratory {
            exploratory_passed
        } else {
            qualification_passed
        },
        "campaign completed both thresholds but failed qualification: mode={} failed_episodes={} attestation_complete={attestation_complete} missing_coverage={missing:?}",
        config.qualification.key(),
        failures.len()
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CampaignInjectedFailure {
    Violation,
    ExecutionError,
    UnfiredScheduledFault,
    Panic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CampaignFailureKind {
    InvariantViolation,
    ExecutionError,
    UnfiredScheduledFault,
    ViolationAndUnfiredFault,
}

#[test]
fn campaign_executes_consecutive_seed_numbers() {
    let artifacts = tempfile::tempdir().expect("consecutive campaign artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "overall")
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "3")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "3")
        .env("ZE_ADV_CAMPAIGN_START_SEED", "40")
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run consecutive campaign probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{transcript}");
    for seed in 40..=42 {
        assert!(
            artifacts
                .path()
                .join(format!("seed-{seed}-{}", profile_for_seed(seed).key()))
                .is_dir(),
            "seed {seed} was skipped: {transcript}"
        );
    }
}

#[test]
fn campaign_replays_each_feature_episode_before_attestation() {
    let artifacts = tempfile::tempdir().expect("feature replay campaign artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "metadata-filter-planner")
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", "0")
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run one feature campaign episode");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json")).unwrap_or_else(
                |error| panic!("missing feature campaign summary: {error}; {transcript}"),
            ),
        )
        .expect("parse feature campaign summary");
    assert_eq!(
        summary["attestation"]["metadata_oracle_attestation"]["completed_seeds"], 1,
        "{transcript}"
    );
    assert_eq!(
        summary["attestation"]["metadata_oracle_attestation"]["replayed_seeds"], 1,
        "{transcript}"
    );
    let replay_ledger = std::fs::read_to_string(artifacts.path().join("replayed-seeds.jsonl"))
        .unwrap_or_else(|error| panic!("missing durable replay ledger: {error}; {transcript}"));
    let rows = replay_ledger.lines().collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "{transcript}");
    let row: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_str(rows[0]).expect("parse replay ledger row");
    assert_eq!(row["campaign"], "metadata-filter-planner");
    assert_eq!(row["seed"], 0);
    assert_eq!(row["profile"], "none");
    assert!(row["evidence_digest"].is_string());
    assert!(row["expected_digest"].is_string());
    assert!(row["observed_digest"].is_string());
    assert_eq!(row["expected_digest"], row["observed_digest"]);
}

impl CampaignFailureKind {
    const fn key(self) -> &'static str {
        match self {
            Self::InvariantViolation => "invariant_violation",
            Self::ExecutionError => "execution_error",
            Self::UnfiredScheduledFault => "unfired_scheduled_fault",
            Self::ViolationAndUnfiredFault => "invariant_violation_and_unfired_scheduled_fault",
        }
    }
}

#[derive(Debug)]
struct CampaignFailure {
    seed: u64,
    profile: FaultProfile,
    kind: CampaignFailureKind,
    detail: String,
    artifact_directory: PathBuf,
}

#[derive(Debug, Default)]
struct CampaignAttestationCounters {
    comparison_counts: BTreeMap<String, u64>,
    comparison_pass_counts: BTreeMap<String, u64>,
    same_seed_clean_controls: u64,
    integrated_feature_fault_receipts: u64,
    expected_feature_fault_receipts: u64,
    selected_feature_fault_events: u64,
    replayed_seeds: u64,
}

impl CampaignAttestationCounters {
    fn merge(&mut self, outcome: &adversarial::runner::RunOutcome) {
        for (invariant, count) in &outcome.comparison_counts {
            let total = self.comparison_counts.entry(invariant.clone()).or_default();
            *total = total.saturating_add(*count);
        }
        for (invariant, count) in &outcome.comparison_pass_counts {
            let total = self
                .comparison_pass_counts
                .entry(invariant.clone())
                .or_default();
            *total = total.saturating_add(*count);
        }
        self.same_seed_clean_controls = self
            .same_seed_clean_controls
            .saturating_add(outcome.same_seed_clean_controls);
        self.integrated_feature_fault_receipts = self
            .integrated_feature_fault_receipts
            .saturating_add(outcome.integrated_feature_fault_receipts);
        self.expected_feature_fault_receipts = self
            .expected_feature_fault_receipts
            .saturating_add(outcome.expected_feature_fault_receipts);
        self.selected_feature_fault_events = self
            .selected_feature_fault_events
            .saturating_add(outcome.feature_faults_scheduled as u64);
    }

    fn mark_replayed(&mut self) {
        self.replayed_seeds = self.replayed_seeds.saturating_add(1);
    }
}

impl CampaignFailure {
    fn json(&self) -> zeppelin_embed_bench::harness_json::Value {
        zeppelin_embed_bench::harness_json::json!({
            "seed": self.seed,
            "profile": self.profile.key(),
            "kind": self.kind.key(),
            "detail": self.detail,
            "artifact_directory": self.artifact_directory.display().to_string(),
        })
    }
}

#[test]
fn campaign_records_failed_seeds_continues_and_fails_qualification_at_the_end() {
    let artifacts = tempfile::tempdir().expect("campaign failure artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "89")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", "0")
        .env(
            "ZE_ADV_CAMPAIGN_TEST_FAILURES",
            "84:violation,85:error,86:unfired,87:panic",
        )
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run campaign failure probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "a campaign with failed seeds must fail qualification after it finishes: {transcript}"
    );

    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("completed campaign summary"),
        )
        .expect("valid campaign summary JSON");
    assert_eq!(summary["complete"], true, "{transcript}");
    assert_eq!(summary["qualification_passed"], false, "{transcript}");
    assert_eq!(
        summary["episodes"], 89,
        "campaign stopped at the first failure"
    );
    assert_eq!(summary["failed_episodes"], 4);
    assert_eq!(summary["execution_errors"], 2);
    assert_eq!(summary["unfired_scheduled_faults"], 1);
    assert_eq!(summary["violations"], 1);
    assert_eq!(
        summary["failures"]
            .as_array()
            .expect("failure ledger")
            .iter()
            .map(|failure| failure["seed"].as_u64().expect("failure seed"))
            .collect::<Vec<_>>(),
        vec![84, 85, 86, 87]
    );

    for seed in 84..=87 {
        let directory = artifacts
            .path()
            .join("failures")
            .join(format!("seed-{seed}-{}", profile_for_seed(seed).key()));
        for name in [
            "program.jsonl",
            "faults.jsonl",
            "violations.json",
            "coverage.json",
            "repro.txt",
            "failure.json",
        ] {
            assert!(
                directory.join(name).is_file(),
                "failed seed {seed} lost {name}: {transcript}"
            );
        }
    }
    assert!(
        artifacts.path().join("seed-88-none").is_dir(),
        "the campaign did not continue through the canonical matrix: {transcript}"
    );
    for seed in 84..=87 {
        assert!(transcript.contains(&format!(
            "ADV_CAMPAIGN_FAILURE seed={seed} profile={}",
            profile_for_seed(seed).key()
        )));
    }
    assert!(transcript.contains("ADV_CAMPAIGN_COMPLETE episodes=89 qualification=failed"));
}

#[test]
fn exploratory_feature_campaign_reports_missing_coverage_and_fails_qualification() {
    let artifacts = tempfile::tempdir().expect("exploratory campaign artifacts");
    let campaign = CampaignKind::StorageDurability;
    let clean_seed = (0..128)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("storage campaign clean slot");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", campaign.key())
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", clean_seed.to_string())
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run exploratory campaign probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{transcript}");
    assert!(
        transcript.contains("ADV_CAMPAIGN_COVERAGE_INCOMPLETE campaign=storage-durability"),
        "{transcript}"
    );
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("exploratory summary"),
        )
        .expect("valid exploratory summary");
    assert_eq!(summary["version"], 3);
    assert_eq!(summary["campaign"], campaign.key());
    assert_eq!(summary["qualification"], "exploratory");
    assert_eq!(summary["verdict"], "passed", "{transcript}");
    assert_eq!(summary["run_verdict"], "passed", "{transcript}");
    assert_eq!(summary["qualification_passed"], false);
    assert_eq!(summary["violations"], 0);
    assert!(summary["host"]["os"].is_string());
    assert!(summary["host"]["arch"].is_string());
    assert!(summary["required_operations"].is_array());
    assert!(summary["executed_operations"].is_array());
    assert!(summary["missing_operations"].is_array());
    assert!(summary["required_profiles"].is_array());
    assert!(summary["executed_profiles"].is_array());
    assert!(summary["missing_profiles"].is_array());
    assert!(summary["required_generic_faults"].is_array());
    assert!(summary["fired_generic_faults"].is_array());
    assert!(summary["missing_generic_faults"].is_array());
    assert!(summary["languages"]["required"].is_array());
    assert!(summary["backends"]["required"].is_array());
    assert_eq!(summary["attestation"]["oracle_contract_version"], 1);
    assert_eq!(
        summary["attestation"]["oracle_contract"],
        zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION
    );
    assert!(
        summary["attestation"]["harness_git_revision"]
            .as_str()
            .is_some_and(|revision| !revision.is_empty() && revision != "unknown")
    );
    assert!(summary["attestation"]["comparison_counts"].is_object());
    assert!(summary["attestation"]["same_seed_clean_controls"].is_u64());
    assert!(summary["attestation"]["integrated_feature_fault_receipts"].is_u64());
    assert!(summary["attestation"]["expected_feature_fault_receipts"].is_u64());
    assert_eq!(summary["attestation"]["merged_evidence"]["episodes"], 1);
    assert_eq!(summary["attestation"]["merged_evidence"]["complete"], true);
    for stream in ["oracle", "controls", "receipts", "mutations"] {
        assert!(
            summary["attestation"]["merged_evidence"]["streams"][stream]["records"].is_u64(),
            "missing merged {stream} record count"
        );
        assert!(
            summary["attestation"]["merged_evidence"]["streams"][stream]["digest"]
                .as_str()
                .is_some_and(|digest| digest.starts_with("fnv1a64:")),
            "missing merged {stream} digest"
        );
    }
    assert_eq!(summary["panics"], 0);
    assert_eq!(summary["counters"]["panics"], 0);
    assert!(!summary["missing_coverage"].as_array().unwrap().is_empty());
    assert!(
        !summary["required_invariants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!summary["checked_invariants"].as_array().unwrap().is_empty());
    assert!(summary["missing_invariants"].is_array());
    assert!(
        !summary["required_feature_faults"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !summary["missing_feature_faults"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn feature_rotation_preserves_complete_merged_evidence_first() {
    let artifacts = tempfile::tempdir().expect("merged feature artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "storage-durability")
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "2")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", "0")
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run merged feature campaign probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Exploratory runs exit green on a clean run and report the coverage
    // gap; two episodes cannot fully qualify storage durability.
    assert!(output.status.success(), "{transcript}");
    assert!(
        transcript.contains("ADV_CAMPAIGN_COVERAGE_INCOMPLETE campaign=storage-durability"),
        "{transcript}"
    );

    let index = std::fs::read_to_string(artifacts.path().join("merged-index.jsonl"))
        .unwrap_or_else(|error| {
            panic!("merged index missing before rotation: {error}: {transcript}")
        });
    assert_eq!(index.lines().count(), 2, "{transcript}");
    let oracle = std::fs::read_to_string(artifacts.path().join("merged-oracle.jsonl"))
        .expect("merged oracle");
    assert!(oracle.contains("\"seed\":0"), "{oracle}");
    assert!(oracle.contains("\"seed\":1"), "{oracle}");
    for name in [
        "merged-controls.jsonl",
        "merged-receipts.jsonl",
        "merged-mutations.jsonl",
    ] {
        assert!(artifacts.path().join(name).is_file(), "missing {name}");
    }
    let retained = std::fs::read_dir(artifacts.path().join("storage-durability"))
        .expect("retained storage episodes")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .count();
    assert_eq!(
        retained, 1,
        "rotation did not run after merge: {transcript}"
    );
}

#[test]
fn vector_rotation_preserves_merged_evidence() {
    let artifacts = tempfile::tempdir().expect("merged vector artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "vector-execution")
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "2")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", "0")
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run merged vector campaign probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Exploratory runs exit green on a clean run and report the coverage
    // gap; two episodes cannot fully qualify vector execution.
    assert!(output.status.success(), "{transcript}");
    assert!(
        transcript.contains("ADV_CAMPAIGN_COVERAGE_INCOMPLETE campaign=vector-execution"),
        "{transcript}"
    );

    let index = std::fs::read_to_string(artifacts.path().join("merged-index.jsonl"))
        .unwrap_or_else(|error| {
            panic!("vector merged index missing before rotation: {error}: {transcript}")
        });
    assert_eq!(index.lines().count(), 2, "{transcript}");
    let oracle = std::fs::read_to_string(artifacts.path().join("merged-oracle.jsonl"))
        .expect("merged vector oracle");
    assert!(oracle.contains("\"seed\":0"), "{oracle}");
    assert!(oracle.contains("\"seed\":1"), "{oracle}");
    for name in [
        "merged-controls.jsonl",
        "merged-receipts.jsonl",
        "merged-mutations.jsonl",
        "merged-family-fixture.json.jsonl",
        "merged-family-backend-inventory.json.jsonl",
        "merged-family-quantization.jsonl.jsonl",
        "merged-family-rescore.jsonl.jsonl",
        "merged-family-identity.jsonl.jsonl",
        "merged-family-episode-summary.json.jsonl",
    ] {
        assert!(artifacts.path().join(name).is_file(), "missing {name}");
    }
    let retained = std::fs::read_dir(artifacts.path().join("vector-execution"))
        .expect("retained vector episodes")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .count();
    assert_eq!(
        retained, 1,
        "vector rotation did not run after merge: {transcript}"
    );
}

#[test]
fn release_feature_campaign_refuses_incomplete_coverage() {
    let artifacts = tempfile::tempdir().expect("release campaign artifacts");
    let clean_seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(CampaignKind::Fts, *seed);
            FaultPlan::for_program(
                CampaignKind::Fts,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("FTS campaign clean slot");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "fts")
        .env("ZE_ADV_QUALIFICATION", "release")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", clean_seed.to_string())
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run release coverage probe");
    assert!(!output.status.success());
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("release summary"),
        )
        .expect("valid release summary");
    assert_eq!(summary["run_verdict"], "passed");
    assert_eq!(summary["qualification_passed"], false);
    assert_eq!(summary["violations"].as_u64(), Some(0));
    assert!(!summary["missing_coverage"].as_array().unwrap().is_empty());
}

#[test]
fn release_feature_campaign_rejects_real_violations() {
    let artifacts = tempfile::tempdir().expect("release campaign artifacts");
    let clean_seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(CampaignKind::Fts, *seed);
            FaultPlan::for_program(
                CampaignKind::Fts,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .is_empty()
        })
        .expect("FTS campaign clean slot");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "fts")
        .env("ZE_ADV_QUALIFICATION", "release")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", clean_seed.to_string())
        .env(
            "ZE_ADV_CAMPAIGN_TEST_FAILURES",
            format!("{clean_seed}:violation"),
        )
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run release coverage probe");
    assert!(!output.status.success());
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("release summary"),
        )
        .expect("valid release summary");
    assert_eq!(summary["run_verdict"], "failed");
    assert_eq!(summary["qualification_passed"], false);
    assert!(summary["violations"].as_u64().unwrap() > 0);
}

#[test]
#[ignore = "helper subprocess intentionally aborts after a durable acknowledgement"]
fn crash_child() {
    if let Err(error) = adversarial::runner::crash_child_from_env() {
        panic!("crash child setup failed: {error}");
    }
}

fn missing_campaign_coverage(campaign: CampaignKind, coverage: &CoverageRegistry) -> Vec<String> {
    if campaign == CampaignKind::Overall {
        coverage
            .missing_required_smoke()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        let mut required = CampaignSpec::for_kind(campaign)
            .all_required_coverage()
            .into_iter();
        let generic = adversarial::coverage::REQUIRED_SMOKE_COVERAGE
            .iter()
            .filter(|key| {
                key.starts_with("fault.profile.")
                    || key.starts_with("fault.site.")
                    || key.starts_with("fault.mode.")
            })
            .map(|key| (*key).to_owned());
        let mut missing = required
            .by_ref()
            .chain(generic)
            .filter(|key| coverage.count(key) == 0)
            .collect::<Vec<_>>();
        if campaign == CampaignKind::FfiBindings {
            // Only the Rust adapter exists. The summary still lists c, python,
            // and swift as required-and-missing so the gap stays visible, but
            // required coverage cannot include keys nothing can earn.
            missing.extend(
                ["rust"]
                    .into_iter()
                    .map(|language| format!("binding.language.{language}"))
                    .filter(|key| coverage.count(key) == 0),
            );
        }
        if campaign == CampaignKind::VectorExecution {
            missing.extend(
                zeppelin_embed::kernels::KernelVariant::available()
                    .map(|variant| format!("kernel.backend.{}", variant.backend_id().as_str()))
                    .filter(|key| coverage.count(key) == 0),
            );
        }
        missing.sort();
        missing.dedup();
        missing
    }
}

fn campaign_test_failures(test_mode: bool) -> BTreeMap<u64, CampaignInjectedFailure> {
    let Ok(specification) = std::env::var("ZE_ADV_CAMPAIGN_TEST_FAILURES") else {
        return BTreeMap::new();
    };
    assert!(
        test_mode,
        "ZE_ADV_CAMPAIGN_TEST_FAILURES is available only in explicit campaign test mode"
    );
    let mut failures = BTreeMap::new();
    for entry in specification.split(',').filter(|entry| !entry.is_empty()) {
        let (seed, kind) = entry
            .split_once(':')
            .unwrap_or_else(|| panic!("invalid campaign test failure: {entry}"));
        let seed = seed
            .parse::<u64>()
            .unwrap_or_else(|error| panic!("invalid campaign test seed {seed}: {error}"));
        let kind = match kind {
            "violation" => CampaignInjectedFailure::Violation,
            "error" => CampaignInjectedFailure::ExecutionError,
            "unfired" => CampaignInjectedFailure::UnfiredScheduledFault,
            "panic" => CampaignInjectedFailure::Panic,
            _ => panic!("invalid campaign test failure kind: {kind}"),
        };
        assert!(
            failures.insert(seed, kind).is_none(),
            "duplicate campaign test failure seed: {seed}"
        );
    }
    failures
}

fn panic_detail(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic payload".to_owned())
        },
        |message| (*message).to_owned(),
    )
}

fn episode_artifact_directory(
    root: &Path,
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
) -> PathBuf {
    let root = if campaign == CampaignKind::Overall {
        root.to_path_buf()
    } else {
        root.join(campaign.key())
    };
    root.join(format!("seed-{seed}-{}", profile.key()))
}

fn preserve_campaign_failure(
    root: &Path,
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    kind: CampaignFailureKind,
    detail: &str,
) -> PathBuf {
    let source = episode_artifact_directory(root, campaign, seed, profile);
    let failures_root = if campaign == CampaignKind::Overall {
        root.join("failures")
    } else {
        root.join("failures").join(campaign.key())
    };
    std::fs::create_dir_all(&failures_root).expect("create campaign failures directory");
    let destination = failures_root.join(format!("seed-{seed}-{}", profile.key()));
    assert!(
        !destination.exists(),
        "campaign failure artifact already exists: {}",
        destination.display()
    );
    if source.is_dir() {
        std::fs::rename(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "preserve campaign failure {} as {}: {error}",
                source.display(),
                destination.display()
            )
        });
    } else {
        std::fs::create_dir_all(&destination).expect("create failed episode artifact directory");
    }

    let mut partial_artifacts = false;
    partial_artifacts |= ensure_failure_artifact(
        &destination.join("program.jsonl"),
        &Program::generate_for(campaign, seed).jsonl(),
    );
    partial_artifacts |= ensure_failure_artifact(&destination.join("faults.jsonl"), b"");
    partial_artifacts |= ensure_failure_artifact(&destination.join("violations.json"), b"[]\n");
    partial_artifacts |= ensure_failure_artifact(&destination.join("coverage.json"), b"{}\n");
    partial_artifacts |= ensure_failure_artifact(
        &destination.join("repro.txt"),
        format!(
            "{}\n",
            adversarial::runner::reproduction_for(campaign, seed, profile)
        )
        .as_bytes(),
    );
    partial_artifacts |= ensure_failure_artifact(
        &destination.join("episode.json"),
        &zeppelin_embed_bench::harness_json::to_vec_pretty(
            &zeppelin_embed_bench::harness_json::json!({
                "schema": "zeppelin-embed-adversarial-episode",
                "version": 3,
                "campaign": campaign.key(),
                "seed": seed,
                "profile": profile.key(),
            }),
        )
        .expect("serialize fallback episode metadata"),
    );
    let failure = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-episode-failure",
        "version": 2,
        "campaign": campaign.key(),
        "seed": seed,
        "profile": profile.key(),
        "kind": kind.key(),
        "detail": detail,
        "partial_artifacts": partial_artifacts,
        "reproduce": adversarial::runner::reproduction_for(campaign, seed, profile),
    });
    write_file_synced(
        &destination.join("failure.json"),
        &zeppelin_embed_bench::harness_json::to_vec_pretty(&failure)
            .expect("serialize campaign failure"),
    );
    File::open(&failures_root)
        .and_then(|directory| directory.sync_all())
        .expect("sync campaign failures directory");
    destination
}

fn ensure_failure_artifact(path: &Path, fallback: &[u8]) -> bool {
    if path.is_file() {
        false
    } else {
        write_file_synced(path, fallback);
        true
    }
}

fn write_file_synced(path: &Path, bytes: &[u8]) {
    let mut file = File::create(path)
        .unwrap_or_else(|error| panic!("create campaign artifact {}: {error}", path.display()));
    file.write_all(bytes)
        .unwrap_or_else(|error| panic!("write campaign artifact {}: {error}", path.display()));
    file.sync_all()
        .unwrap_or_else(|error| panic!("sync campaign artifact {}: {error}", path.display()));
}

/// Coverage keys for every declared family evidence stream that has no
/// records yet. A short or unlucky run can leave a fault-only stream empty;
/// that is missing coverage, which release qualification requires and
/// exploratory qualification only reports.
fn empty_family_evidence_streams(
    campaign: CampaignKind,
    merged: Option<&adversarial::artifacts::MergedEvidenceStats>,
) -> Vec<String> {
    let Some(merged) = merged else {
        return Vec::new();
    };
    if campaign == CampaignKind::Overall {
        return Vec::new();
    }
    adversarial::artifacts::replay_artifacts_for(campaign)
        .into_iter()
        .filter(|name| !adversarial::artifacts::REPLAY_ARTIFACTS.contains(name))
        // The family violations alias mirrors the core violations stream
        // and is empty by design on a clean run.
        .filter(|name| *name != "violations.jsonl")
        .map(|name| format!("family/{name}"))
        .filter(|name| {
            merged
                .streams
                .get(name)
                .is_none_or(|stream| stream.records == 0)
        })
        .map(|name| format!("evidence.stream.{name}"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn feature_attestation_complete(
    config: &RunConfig,
    episodes: u64,
    counters: &CampaignAttestationCounters,
    merged: Option<adversarial::artifacts::MergedEvidenceStats>,
) -> bool {
    let Some(merged) = merged else {
        return false;
    };
    let profile_override = std::env::var_os("ZE_ADV_PROFILE").map(|_| config.profile);
    let expected_comparisons = expected_campaign_comparison_counts_with_profile(
        config.campaign,
        config.start_seed,
        episodes,
        profile_override,
    );
    let exact_comparisons = counters.comparison_counts == expected_comparisons;
    let stream_records = |name: &str| merged.streams.get(name).map_or(0, |stream| stream.records);
    let expected_core = [
        "program",
        "faults",
        "violations",
        "coverage",
        "oracle",
        "controls",
        "receipts",
        "mutations",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    let observed_core = merged
        .streams
        .keys()
        .filter(|name| !name.starts_with("family/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let expected_family = adversarial::artifacts::replay_artifacts_for(config.campaign)
        .into_iter()
        .filter(|name| !adversarial::artifacts::REPLAY_ARTIFACTS.contains(name))
        .map(|name| format!("family/{name}"))
        .collect::<BTreeSet<_>>();
    let observed_family = merged
        .streams
        .keys()
        .filter(|name| name.starts_with("family/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    // Empty declared family streams are a coverage gap (see
    // `empty_family_evidence_streams`), not an attestation defect.
    merged.episodes == episodes
        && exact_comparisons
        && counters.comparison_pass_counts == counters.comparison_counts
        && observed_core == expected_core
        && observed_family == expected_family
        && counters.same_seed_clean_controls == counters.selected_feature_fault_events
        && counters.integrated_feature_fault_receipts == counters.expected_feature_fault_receipts
        && stream_records("oracle") == counters.comparison_counts.values().copied().sum::<u64>()
        && stream_records("controls") >= counters.same_seed_clean_controls
        && stream_records("receipts") >= counters.integrated_feature_fault_receipts
}

fn expected_campaign_comparison_counts(
    campaign: CampaignKind,
    start_seed: u64,
    episodes: u64,
) -> BTreeMap<String, u64> {
    expected_campaign_comparison_counts_with_profile(campaign, start_seed, episodes, None)
}

fn expected_campaign_comparison_counts_with_profile(
    campaign: CampaignKind,
    start_seed: u64,
    episodes: u64,
    profile_override: Option<FaultProfile>,
) -> BTreeMap<String, u64> {
    if campaign == CampaignKind::StorageDurability {
        let mut counts = CampaignSpec::for_kind(campaign)
            .owned_invariants
            .iter()
            .map(|invariant| (invariant.key(), 0_u64))
            .collect::<BTreeMap<_, _>>();
        for episode in 0..episodes {
            let seed = start_seed
                .checked_add(episode)
                .expect("storage campaign seed fits u64");
            let profile = profile_override.unwrap_or_else(|| profile_for_seed(seed));
            let program = Program::generate_for(campaign, seed);
            let schedule = plan_schedule(seed, environment_for_profile(profile, seed), &program);
            let plan = FaultPlan::for_program(campaign, seed, profile, &program, schedule);
            for (operation, invariant) in [
                (adversarial::campaign::StorageOperation::WalPrefix, "I16"),
                (adversarial::campaign::StorageOperation::Publication, "I15"),
                (adversarial::campaign::StorageOperation::Retry, "I17"),
                (adversarial::campaign::StorageOperation::FormatCheck, "I18"),
                (
                    adversarial::campaign::StorageOperation::OrphanCleanup,
                    "I19",
                ),
            ] {
                let selected = plan
                    .feature
                    .iter()
                    .filter(|event| {
                        event.fault.operation()
                            == adversarial::campaign::FeatureOperation::Storage(operation)
                    })
                    .count();
                let comparisons = u64::try_from(selected.max(1))
                    .expect("storage operation comparison count fits u64");
                let count = counts
                    .get_mut(invariant)
                    .expect("storage count contract includes operation invariant");
                *count = (*count)
                    .checked_add(comparisons)
                    .expect("storage operation comparison count fits u64");
            }
            let wal_refusal_projections = plan
                .feature
                .iter()
                .filter(|event| {
                    matches!(
                        event.fault,
                        adversarial::campaign::FeatureFault::StorageTornWalHeader
                            | adversarial::campaign::FeatureFault::StorageTornWalBody
                            | adversarial::campaign::FeatureFault::StorageTornWalChecksum
                    )
                })
                .count();
            if wal_refusal_projections > 0 {
                let count = counts
                    .get_mut("I18")
                    .expect("storage count contract includes I18");
                *count = (*count)
                    .checked_add(
                        u64::try_from(wal_refusal_projections)
                            .expect("storage WAL refusal projection count fits u64"),
                    )
                    .expect("storage WAL I18 projection count fits u64");
            }
        }
        return counts;
    }
    if campaign == CampaignKind::IngestRetention {
        let mut counts = CampaignSpec::for_kind(campaign)
            .owned_invariants
            .iter()
            .map(|invariant| (invariant.key(), 0_u64))
            .collect::<BTreeMap<_, _>>();
        for episode in 0..episodes {
            let seed = start_seed
                .checked_add(episode)
                .expect("ingest campaign seed fits u64");
            let profile = profile_override.unwrap_or_else(|| profile_for_seed(seed));
            let program = Program::generate_for(campaign, seed);
            let schedule = plan_schedule(seed, environment_for_profile(profile, seed), &program);
            let plan = FaultPlan::for_program(campaign, seed, profile, &program, schedule);
            for (operation, invariant) in [
                (adversarial::campaign::IngestOperation::BatchCommit, "I20"),
                (adversarial::campaign::IngestOperation::Seal, "I21"),
                (adversarial::campaign::IngestOperation::Retention, "I22"),
                (adversarial::campaign::IngestOperation::Purge, "I23"),
            ] {
                let selected = plan
                    .feature
                    .iter()
                    .filter(|event| {
                        event.fault.operation()
                            == adversarial::campaign::FeatureOperation::Ingest(operation)
                    })
                    .count();
                let comparisons = u64::try_from(selected.max(1))
                    .expect("ingest operation comparison count fits u64");
                let count = counts
                    .get_mut(invariant)
                    .expect("ingest count contract includes operation invariant");
                *count = (*count)
                    .checked_add(comparisons)
                    .expect("ingest operation comparison count fits u64");
            }
        }
        return counts;
    }
    if campaign == CampaignKind::VectorExecution {
        let clean = adversarial::vector_execution::expected_comparison_counts()
            .expect("vector family comparison-count contract");
        let mut counts = clean
            .keys()
            .map(|invariant| ((*invariant).to_owned(), 0_u64))
            .collect::<BTreeMap<_, _>>();
        for episode in 0..episodes {
            let seed = start_seed
                .checked_add(episode)
                .expect("vector campaign seed fits u64");
            let profile = profile_override.unwrap_or_else(|| profile_for_seed(seed));
            let program = Program::generate_for(campaign, seed);
            let schedule = plan_schedule(seed, environment_for_profile(profile, seed), &program);
            let plan = FaultPlan::for_program(campaign, seed, profile, &program, schedule);
            for (operation, invariant) in [
                (adversarial::campaign::VectorOperation::KernelParity, "I24"),
                (adversarial::campaign::VectorOperation::Quantization, "I25"),
                (adversarial::campaign::VectorOperation::Rescore, "I26"),
                (adversarial::campaign::VectorOperation::RowIdentity, "I27"),
            ] {
                let selected = plan
                    .feature
                    .iter()
                    .filter(|event| {
                        event.fault.operation()
                            == adversarial::campaign::FeatureOperation::Vector(operation)
                    })
                    .count();
                let invocations = u64::try_from(selected.max(1))
                    .expect("vector operation invocation count fits u64");
                let comparisons = clean[invariant]
                    .checked_mul(invocations)
                    .expect("vector operation comparison count fits u64");
                let count = counts
                    .get_mut(invariant)
                    .expect("vector count contract includes operation invariant");
                *count = count
                    .checked_add(comparisons)
                    .expect("vector campaign comparison count fits u64");
            }
            let forced_dispatches = plan
                .feature
                .iter()
                .filter(|event| {
                    event.fault == adversarial::campaign::FeatureFault::VectorForcedDispatchBackend
                })
                .count();
            if forced_dispatches > 0 {
                let count = counts
                    .get_mut("I24")
                    .expect("vector count contract includes I24");
                *count = count
                    .checked_add(
                        u64::try_from(forced_dispatches).expect("forced-dispatch count fits u64"),
                    )
                    .expect("forced-dispatch comparison count fits u64");
            }
        }
        return counts;
    }
    if campaign == CampaignKind::MetadataFilterPlanner {
        let matrix = adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT
            .checked_mul(episodes)
            .expect("metadata I37 campaign comparison count fits u64");
        let mut counts = BTreeMap::from([
            ("I36".to_owned(), 0),
            ("I37".to_owned(), matrix),
            ("I38".to_owned(), 0),
            ("I39".to_owned(), 0),
        ]);
        for episode in 0..episodes {
            let seed = start_seed
                .checked_add(episode)
                .expect("metadata campaign seed fits u64");
            let profile = profile_override.unwrap_or_else(|| profile_for_seed(seed));
            let program = Program::generate_for(campaign, seed);
            let schedule = plan_schedule(seed, environment_for_profile(profile, seed), &program);
            let plan = FaultPlan::for_program(campaign, seed, profile, &program, schedule);
            // Each operation runs once per selected feature fault (at least
            // once). Execution owns two fault kinds, so a `full` profile can
            // select both and compare I39 twice in one episode.
            for (operation, invariant) in [
                (adversarial::campaign::MetadataOperation::Columns, "I36"),
                (adversarial::campaign::MetadataOperation::Planner, "I38"),
                (adversarial::campaign::MetadataOperation::Execution, "I39"),
            ] {
                let selected = plan
                    .feature
                    .iter()
                    .filter(|event| {
                        event.fault.operation()
                            == adversarial::campaign::FeatureOperation::Metadata(operation)
                    })
                    .count();
                let comparisons = u64::try_from(selected.max(1))
                    .expect("metadata operation comparison count fits u64");
                let count = counts
                    .get_mut(invariant)
                    .expect("metadata count contract includes operation invariant");
                *count = count
                    .checked_add(comparisons)
                    .expect("metadata operation comparison count fits u64");
            }
            if plan.feature.iter().any(|event| {
                event.fault == adversarial::campaign::FeatureFault::MetadataBitmapTruncation
            }) {
                let count = counts
                    .get_mut("I37")
                    .expect("metadata count contract includes I37");
                *count = count
                    .checked_add(1)
                    .expect("metadata faulted I37 comparison count fits u64");
            }
        }
        return counts;
    }
    let spec = CampaignSpec::for_kind(campaign);
    let mut counts = spec
        .owned_invariants
        .iter()
        .map(|invariant| (invariant.key(), 0_u64))
        .collect::<BTreeMap<_, _>>();
    for episode in 0..episodes {
        let seed = start_seed
            .checked_add(episode)
            .expect("feature campaign seed fits u64");
        let profile = profile_override.unwrap_or_else(|| profile_for_seed(seed));
        let program = Program::generate_for(campaign, seed);
        let schedule = plan_schedule(seed, environment_for_profile(profile, seed), &program);
        let plan = FaultPlan::for_program(campaign, seed, profile, &program, schedule);
        for invariant in spec.invariant_specs {
            let selected = plan
                .feature
                .iter()
                .filter(|event| event.fault.operation() == invariant.operation)
                .count();
            let comparisons = u64::try_from(selected.max(1))
                .expect("feature operation comparison count fits u64");
            let count = counts
                .get_mut(&invariant.invariant.key())
                .expect("feature count contract includes operation invariant");
            *count = count
                .checked_add(comparisons)
                .expect("feature campaign comparison count fits u64");
        }
    }
    counts
}

#[test]
fn feature_attestation_accepts_a_declared_multi_receipt_fault() {
    let config = RunConfig {
        campaign: CampaignKind::MetadataFilterPlanner,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            (
                "I37".to_owned(),
                adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
            ),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            (
                "I37".to_owned(),
                adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
            ),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        same_seed_clean_controls: 1,
        integrated_feature_fault_receipts: 2,
        expected_feature_fault_receipts: 2,
        selected_feature_fault_events: 1,
        replayed_seeds: 1,
    };
    let stream = |records| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{records:016x}"),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: BTreeMap::from([
            ("program".to_owned(), stream(1)),
            ("faults".to_owned(), stream(0)),
            ("violations".to_owned(), stream(1)),
            ("coverage".to_owned(), stream(1)),
            (
                "oracle".to_owned(),
                stream(
                    adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT
                        .saturating_add(3),
                ),
            ),
            ("controls".to_owned(), stream(1)),
            ("receipts".to_owned(), stream(2)),
            ("mutations".to_owned(), stream(0)),
            ("family/metadata-fixture.json".to_owned(), stream(1)),
            ("family/queries.jsonl".to_owned(), stream(1)),
            ("family/fixture-mutations.jsonl".to_owned(), stream(1)),
        ]),
    };

    assert!(feature_attestation_complete(
        &config,
        1,
        &counters,
        Some(merged),
    ));
}

#[test]
fn feature_attestation_rejects_a_failed_comparison() {
    let config = RunConfig {
        campaign: CampaignKind::MetadataFilterPlanner,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            (
                "I37".to_owned(),
                adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
            ),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I36".to_owned(), 0),
            (
                "I37".to_owned(),
                adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
            ),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        same_seed_clean_controls: 1,
        integrated_feature_fault_receipts: 2,
        expected_feature_fault_receipts: 2,
        selected_feature_fault_events: 1,
        replayed_seeds: 1,
    };
    let stream = |records| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{records:016x}"),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: BTreeMap::from([
            ("program".to_owned(), stream(1)),
            ("faults".to_owned(), stream(0)),
            ("violations".to_owned(), stream(1)),
            ("coverage".to_owned(), stream(1)),
            (
                "oracle".to_owned(),
                stream(
                    adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT
                        .saturating_add(3),
                ),
            ),
            ("controls".to_owned(), stream(1)),
            ("receipts".to_owned(), stream(2)),
            ("mutations".to_owned(), stream(0)),
            ("family/metadata-fixture.json".to_owned(), stream(1)),
            ("family/queries.jsonl".to_owned(), stream(1)),
            ("family/fixture-mutations.jsonl".to_owned(), stream(1)),
        ]),
    };

    // The verifier recomputes validity from the oracle rows, so a failed
    // comparison must make the producer's flag false as well.
    assert!(!feature_attestation_complete(
        &config,
        1,
        &counters,
        Some(merged),
    ));
}

#[test]
fn empty_family_violations_alias_is_not_a_coverage_gap() {
    let stream = |records| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{records:016x}"),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: adversarial::artifacts::replay_artifacts_for(CampaignKind::VectorExecution)
            .into_iter()
            .filter(|name| !adversarial::artifacts::REPLAY_ARTIFACTS.contains(name))
            .map(|name| {
                let records = u64::from(name != "violations.jsonl");
                (format!("family/{name}"), stream(records))
            })
            .collect(),
    };
    assert!(empty_family_evidence_streams(CampaignKind::VectorExecution, Some(&merged)).is_empty());
}

#[test]
fn feature_attestation_rejects_empty_declared_family_streams() {
    let config = RunConfig {
        campaign: CampaignKind::MetadataFilterPlanner,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let stream = |records| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{records:016x}"),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: BTreeMap::from([
            ("program".to_owned(), stream(1)),
            ("faults".to_owned(), stream(0)),
            ("violations".to_owned(), stream(1)),
            ("coverage".to_owned(), stream(1)),
            ("oracle".to_owned(), stream(4)),
            ("controls".to_owned(), stream(0)),
            ("receipts".to_owned(), stream(0)),
            ("mutations".to_owned(), stream(0)),
            ("family/metadata-fixture.json".to_owned(), stream(0)),
            ("family/queries.jsonl".to_owned(), stream(0)),
            ("family/fixture-mutations.jsonl".to_owned(), stream(0)),
        ]),
    };

    let missing = empty_family_evidence_streams(config.campaign, Some(&merged));
    assert_eq!(
        missing,
        vec![
            "evidence.stream.family/fixture-mutations.jsonl".to_owned(),
            "evidence.stream.family/metadata-fixture.json".to_owned(),
            "evidence.stream.family/queries.jsonl".to_owned(),
        ],
        "empty family streams must surface as missing coverage"
    );
    // Empty streams alone are not an attestation defect; the completeness
    // predicate is exercised by the positive fixture in the previous test.
    drop(merged);
}

#[test]
fn feature_attestation_rejects_excess_oracle_comparisons() {
    let config = RunConfig {
        campaign: CampaignKind::MetadataFilterPlanner,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I36".to_owned(), 2),
            ("I37".to_owned(), 1),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I36".to_owned(), 2),
            ("I37".to_owned(), 1),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        same_seed_clean_controls: 0,
        integrated_feature_fault_receipts: 0,
        expected_feature_fault_receipts: 0,
        selected_feature_fault_events: 0,
        replayed_seeds: 1,
    };
    let stream = |records| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{records:016x}"),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: BTreeMap::from([
            ("program".to_owned(), stream(1)),
            ("faults".to_owned(), stream(1)),
            ("violations".to_owned(), stream(1)),
            ("coverage".to_owned(), stream(1)),
            ("oracle".to_owned(), stream(5)),
            ("controls".to_owned(), stream(0)),
            ("receipts".to_owned(), stream(0)),
            ("mutations".to_owned(), stream(0)),
            ("family/metadata-fixture.json".to_owned(), stream(1)),
            ("family/queries.jsonl".to_owned(), stream(1)),
            ("family/fixture-mutations.jsonl".to_owned(), stream(1)),
        ]),
    };

    assert!(
        !feature_attestation_complete(&config, 1, &counters, Some(merged)),
        "an excess oracle comparison was accepted as exact attestation"
    );
}

#[test]
fn vector_campaign_comparison_counts_follow_the_family_contract() {
    let shared = expected_campaign_comparison_counts(CampaignKind::VectorExecution, 0, 1);
    let family = adversarial::vector_execution::expected_comparison_counts()
        .expect("vector family comparison-count contract");
    assert_eq!(shared["I25"], family["I25"]);
    assert_eq!(shared["I26"], family["I26"]);
    assert_eq!(shared["I27"], family["I27"]);
}

#[test]
fn vector_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::VectorExecution, 0, 1_000);
    assert_eq!(counts["I24"], 2_041_175);
    assert_eq!(counts["I25"], 69_000);
    assert_eq!(counts["I26"], 12_000);
    assert_eq!(counts["I27"], 17_425);
}

#[test]
fn graph_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::VamanaGraph, 0, 1_000);
    assert_eq!(counts["I28"], 1_000);
    assert_eq!(counts["I29"], 1_000);
    assert_eq!(counts["I30"], 1_006);
    assert_eq!(counts["I31"], 1_006);
    assert_eq!(counts["I32"], 1_000);
    assert_eq!(counts["I33"], 1_000);
    assert_eq!(counts["I34"], 1_000);
    assert_eq!(counts["I35"], 1_000);
}

#[test]
fn fts_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::Fts, 0, 1_000);
    assert_eq!(counts["I40"], 1_000);
    assert_eq!(counts["I41"], 1_035);
    assert_eq!(counts["I42"], 1_000);
    assert_eq!(counts["I43"], 1_000);
    assert_eq!(counts["I44"], 1_006);
}

#[test]
fn hybrid_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::HybridFusion, 0, 1_000);
    assert_eq!(counts["I45"], 1_000);
    assert_eq!(counts["I46"], 1_000);
    assert_eq!(counts["I47"], 1_000);
    assert_eq!(counts["I48"], 1_000);
    assert_eq!(counts["I49"], 1_059);
}

#[test]
fn tiering_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::TieringMaintenance, 0, 1_000);
    assert_eq!(counts["I50"], 1_000);
    assert_eq!(counts["I51"], 1_000);
    assert!(counts["I52"] > 1_000);
    assert!(counts["I53"] > 1_000);
}

#[test]
fn lifecycle_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::LifecycleAccounting, 0, 1_000);
    assert_eq!(counts["I54"], 1_000);
    assert_eq!(counts["I55"], 1_000);
    assert!(counts["I56"] > 1_000);
    assert_eq!(counts["I57"], 1_000);
    assert_eq!(counts["I58"], 1_000);
}

#[test]
fn ffi_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    let counts = expected_campaign_comparison_counts(CampaignKind::FfiBindings, 0, 1_000);
    assert_eq!(counts["I66"], 1_008);
    assert_eq!(counts["I67"], 1_008);
    assert_eq!(counts["I68"], 1_000);
    assert_eq!(counts["I69"], 1_000);
    assert_eq!(counts["I70"], 1_000);
}

#[test]
fn metadata_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    // Execution owns two fault kinds; 41 `full`-profile episodes in the
    // first 1,000 select both, so I39 compares twice in those episodes.
    let counts = expected_campaign_comparison_counts(CampaignKind::MetadataFilterPlanner, 0, 1_000);
    assert_eq!(counts["I36"], 1_000);
    assert_eq!(counts["I37"], 45_217);
    assert_eq!(counts["I38"], 1_000);
    assert_eq!(counts["I39"], 1_041);
}

#[test]
fn graph_campaign_oracle_rows_bind_all_eight_exact_checkers() {
    let empty = zeppelin_embed_bench::harness_json::json!({});
    let canonical =
        zeppelin_embed_bench::harness_json::to_vec(&empty).expect("canonical empty graph evidence");
    let digest = adversarial::artifacts::evidence_digest(&[&canonical]);
    for (invariant, checker_id, operation) in [
        (
            "I28",
            zeppelin_embed_adversarial_oracle::vamana_graph::I28_CHECKER_ID,
            "shape",
        ),
        (
            "I29",
            zeppelin_embed_adversarial_oracle::vamana_graph::I29_CHECKER_ID,
            "entry-points",
        ),
        (
            "I30",
            zeppelin_embed_adversarial_oracle::vamana_graph::I30_CHECKER_ID,
            "search",
        ),
        (
            "I31",
            zeppelin_embed_adversarial_oracle::vamana_graph::I31_CHECKER_ID,
            "search",
        ),
        (
            "I32",
            zeppelin_embed_adversarial_oracle::vamana_graph::I32_CHECKER_ID,
            "bounded-build",
        ),
        (
            "I33",
            zeppelin_embed_adversarial_oracle::vamana_graph::I33_CHECKER_ID,
            "publication",
        ),
        (
            "I34",
            zeppelin_embed_adversarial_oracle::vamana_graph::I34_CHECKER_ID,
            "checkpoint",
        ),
        (
            "I35",
            zeppelin_embed_adversarial_oracle::vamana_graph::I35_CHECKER_ID,
            "filtered-search",
        ),
    ] {
        let record = zeppelin_embed_bench::harness_json::json!({
            "invariant": invariant,
            "checker_id": checker_id,
            "operation": operation,
            "expected": {},
            "observed": {},
            "input_digest": digest,
            "observed_digest": digest,
            "canonical_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
            "oracle_input_digest": digest,
            "oracle_observed_digest": digest,
            "passed": true,
            "first_difference": null,
        });
        assert_eq!(
            validate_feature_oracle_record(CampaignKind::VamanaGraph, &record),
            Ok(invariant.to_owned())
        );
    }
}

#[test]
fn remaining_direct_family_oracle_rows_bind_their_exact_checkers() {
    let empty = zeppelin_embed_bench::harness_json::json!({});
    let canonical =
        zeppelin_embed_bench::harness_json::to_vec(&empty).expect("canonical empty evidence");
    let digest = adversarial::artifacts::evidence_digest(&[&canonical]);
    let bindings = [
        (
            CampaignKind::Fts,
            "I40",
            zeppelin_embed_adversarial_oracle::fts::I40_CHECKER_ID,
            "tokenizer",
        ),
        (
            CampaignKind::Fts,
            "I41",
            zeppelin_embed_adversarial_oracle::fts::I41_CHECKER_ID,
            "regions",
        ),
        (
            CampaignKind::Fts,
            "I42",
            zeppelin_embed_adversarial_oracle::fts::I42_CHECKER_ID,
            "bm25",
        ),
        (
            CampaignKind::Fts,
            "I43",
            zeppelin_embed_adversarial_oracle::fts::I43_CHECKER_ID,
            "pruning",
        ),
        (
            CampaignKind::Fts,
            "I44",
            zeppelin_embed_adversarial_oracle::fts::I44_CHECKER_ID,
            "extras",
        ),
        (
            CampaignKind::HybridFusion,
            "I45",
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I45_CHECKER_ID,
            "provenance",
        ),
        (
            CampaignKind::HybridFusion,
            "I46",
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I46_CHECKER_ID,
            "normalization",
        ),
        (
            CampaignKind::HybridFusion,
            "I47",
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I47_CHECKER_ID,
            "bounded-fusion",
        ),
        (
            CampaignKind::HybridFusion,
            "I48",
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I48_CHECKER_ID,
            "rrf-fallback",
        ),
        (
            CampaignKind::HybridFusion,
            "I49",
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I49_CHECKER_ID,
            "legs",
        ),
        (
            CampaignKind::TieringMaintenance,
            "I50",
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I50_CHECKER_ID,
            "policy",
        ),
        (
            CampaignKind::TieringMaintenance,
            "I51",
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I51_CHECKER_ID,
            "transition",
        ),
        (
            CampaignKind::TieringMaintenance,
            "I52",
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I52_CHECKER_ID,
            "budget",
        ),
        (
            CampaignKind::TieringMaintenance,
            "I53",
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I53_CHECKER_ID,
            "publication",
        ),
        (
            CampaignKind::LifecycleAccounting,
            "I54",
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I54_CHECKER_ID,
            "deadline",
        ),
        (
            CampaignKind::LifecycleAccounting,
            "I55",
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I55_CHECKER_ID,
            "cancellation",
        ),
        (
            CampaignKind::LifecycleAccounting,
            "I56",
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I56_CHECKER_ID,
            "close-drain",
        ),
        (
            CampaignKind::LifecycleAccounting,
            "I57",
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I57_CHECKER_ID,
            "locking",
        ),
        (
            CampaignKind::LifecycleAccounting,
            "I58",
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I58_CHECKER_ID,
            "accounting",
        ),
        (
            CampaignKind::DiagnosticsHealth,
            "I63",
            zeppelin_embed_adversarial_oracle::diagnostics_health::I63_CHECKER_ID,
            "health",
        ),
        (
            CampaignKind::DiagnosticsHealth,
            "I64",
            zeppelin_embed_adversarial_oracle::diagnostics_health::I64_CHECKER_ID,
            "self-check",
        ),
        (
            CampaignKind::DiagnosticsHealth,
            "I65",
            zeppelin_embed_adversarial_oracle::diagnostics_health::I65_CHECKER_ID,
            "recovery",
        ),
        (
            CampaignKind::FfiBindings,
            "I66",
            zeppelin_embed_adversarial_oracle::ffi_bindings::I66_CHECKER_ID,
            "validation",
        ),
        (
            CampaignKind::FfiBindings,
            "I67",
            zeppelin_embed_adversarial_oracle::ffi_bindings::I67_CHECKER_ID,
            "ownership",
        ),
        (
            CampaignKind::FfiBindings,
            "I68",
            zeppelin_embed_adversarial_oracle::ffi_bindings::I68_CHECKER_ID,
            "containment",
        ),
        (
            CampaignKind::FfiBindings,
            "I69",
            zeppelin_embed_adversarial_oracle::ffi_bindings::I69_CHECKER_ID,
            "deadline",
        ),
        (
            CampaignKind::FfiBindings,
            "I70",
            zeppelin_embed_adversarial_oracle::ffi_bindings::I70_CHECKER_ID,
            "parity",
        ),
    ];
    for (campaign, invariant, checker_id, operation) in bindings {
        let record = zeppelin_embed_bench::harness_json::json!({
            "invariant": invariant,
            "checker_id": checker_id,
            "operation": operation,
            "expected": {},
            "observed": {},
            "input_digest": digest,
            "observed_digest": digest,
            "canonical_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
            "oracle_input_digest": digest,
            "oracle_observed_digest": digest,
            "passed": true,
            "first_difference": null,
        });
        assert_eq!(
            validate_feature_oracle_record(campaign, &record),
            Ok(invariant.to_owned())
        );
    }
}

#[test]
fn storage_campaign_comparison_counts_include_damaged_wal_i18_projections() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..128)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault == adversarial::campaign::FeatureFault::StorageTornWalHeader)
        })
        .expect("storage campaign schedules torn WAL header");
    let counts = expected_campaign_comparison_counts(campaign, seed, 1);
    assert_eq!(counts.get("I16"), Some(&1));
    assert_eq!(counts.get("I18"), Some(&2));
}

#[test]
fn storage_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    assert_eq!(
        expected_campaign_comparison_counts(CampaignKind::StorageDurability, 0, 1_000),
        BTreeMap::from([
            ("I15".to_owned(), 1_003),
            ("I16".to_owned(), 1_005),
            ("I17".to_owned(), 1_000),
            ("I18".to_owned(), 1_308),
            ("I19".to_owned(), 1_000),
        ])
    );
}

#[test]
fn ingest_campaign_comparison_counts_include_same_operation_fault_multiplicity() {
    assert_eq!(
        expected_campaign_comparison_counts(CampaignKind::IngestRetention, 0, 1_000),
        BTreeMap::from([
            ("I20".to_owned(), 1_009),
            ("I21".to_owned(), 1_000),
            ("I22".to_owned(), 1_000),
            ("I23".to_owned(), 1_008),
        ])
    );
}

#[test]
fn storage_required_coverage_names_every_format_and_omission_case() {
    let required = CampaignSpec::for_kind(CampaignKind::StorageDurability)
        .all_required_coverage()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for key in [
        "storage.format-case.wal-header",
        "storage.format-case.wal-record-body",
        "storage.format-case.wal-record-checksum",
        "storage.format-case.segment-region",
        "storage.format-case.manifest-wrong-family",
        "storage.format-case.segment-wrong-family",
        "storage.format-case.segment-wrong-identity",
        "storage.omission.final-segment.list",
        "storage.omission.final-segment.delete",
        "storage.omission.segment-temporary.list",
        "storage.omission.segment-temporary.delete",
        "storage.omission.manifest-temporary.list",
        "storage.omission.manifest-temporary.delete",
    ] {
        assert!(required.contains(key), "missing storage coverage key {key}");
    }
}

#[test]
fn storage_fixture_grammar_reaches_required_phases() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| {
                event.fault == adversarial::campaign::FeatureFault::StorageListDeleteOmission
            })
        })
        .expect("canonical storage smoke schedules list/delete omission");
    let mut coverage = CoverageRegistry::default();
    for profile in FaultProfile::DEFAULTS {
        let artifacts = tempfile::tempdir().expect("storage omission matrix artifacts");
        let outcome =
            adversarial::runner::run_program_for(campaign, seed, profile, artifacts.path())
                .unwrap_or_else(|error| {
                    panic!("storage omission profile {}: {error}", profile.key())
                });
        assert!(
            outcome.violations.is_empty(),
            "storage omission profile {}: {:?}",
            profile.key(),
            outcome.violations,
        );
        assert!(outcome.feature_faults_scheduled > 0, "{}", profile.key());
        assert_eq!(
            outcome.feature_faults_fired,
            outcome.feature_faults_scheduled,
            "{}",
            profile.key()
        );
        coverage.merge(&outcome.coverage);
    }
    let missing = [
        "storage.omission.final-segment.list",
        "storage.omission.final-segment.delete",
        "storage.omission.segment-temporary.list",
        "storage.omission.segment-temporary.delete",
        "storage.omission.manifest-temporary.list",
        "storage.omission.manifest-temporary.delete",
    ]
    .into_iter()
    .filter(|key| coverage.count(key) == 0)
    .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "canonical seven-profile storage smoke omitted I19 cases: {missing:?}"
    );
}

#[test]
fn storage_clean_fault_pair_is_byte_identical_before_injection() {
    let evidence = adversarial::storage_durability::observe_retry(17, 3, None)
        .expect("observe storage retry clean/fault pair");
    assert_eq!(
        evidence.control.pre_clean_inventory, evidence.control.pre_fault_inventory,
        "storage clean/fault fixture inventories differ before injection"
    );
    assert_eq!(
        evidence.control.pre_clean_digest, evidence.control.pre_fault_digest,
        "storage clean/fault fixture bytes differ before injection"
    );
    assert_eq!(
        evidence
            .control
            .pre_clean_artifacts
            .iter()
            .map(|artifact| (&artifact.fact, &artifact.bytes))
            .collect::<Vec<_>>(),
        evidence
            .control
            .pre_fault_artifacts
            .iter()
            .map(|artifact| (&artifact.fact, &artifact.bytes))
            .collect::<Vec<_>>(),
        "storage clean/fault retained artifact bytes differ before injection"
    );
}

#[test]
fn storage_summary_reconciles_checkers_controls_and_receipts() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..128)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(
                campaign,
                *seed,
                FaultProfile::None,
                &program,
                FaultSchedule::default(),
            )
            .feature
            .iter()
            .any(|event| event.fault == adversarial::campaign::FeatureFault::StoragePostCommitError)
        })
        .expect("storage campaign schedules post-commit error");
    let artifacts = tempfile::tempdir().expect("storage reconciliation artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("storage reconciliation episode");
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert_eq!(
        outcome.comparison_counts,
        expected_campaign_comparison_counts(campaign, seed, 1),
        "storage checker ledger differs from its exact episode schedule"
    );
    assert_eq!(
        outcome.same_seed_clean_controls,
        outcome.feature_faults_scheduled as u64
    );
    assert_eq!(
        outcome.integrated_feature_fault_receipts,
        outcome.expected_feature_fault_receipts
    );
    assert_eq!(
        outcome.integrated_feature_fault_receipts,
        outcome.feature_faults_fired as u64
    );
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&outcome.episode_bytes)
            .expect("parse reconciled storage episode metadata");
    let storage = &episode["attestation"]["storage_oracle_attestation"];
    assert_eq!(
        storage["integrated_receipts"]["observed"].as_u64(),
        Some(outcome.integrated_feature_fault_receipts)
    );
    for (invariant, count) in &outcome.comparison_counts {
        assert_eq!(
            storage["per_invariant_comparisons"][invariant]["comparisons"].as_u64(),
            Some(*count),
            "storage episode attestation count differs for {invariant}"
        );
    }
}

#[test]
fn storage_campaign_executes_every_required_format_and_omission_case() {
    use zeppelin_embed_adversarial_oracle::storage_durability::{OmissionSubsite, OrphanKind};

    let campaign = CampaignKind::StorageDurability;
    let required = CampaignSpec::for_kind(campaign)
        .all_required_coverage()
        .into_iter()
        .filter(|key| {
            key.starts_with("storage.format-case.") || key.starts_with("storage.omission.")
        })
        .collect::<BTreeSet<_>>();
    let mut seed_by_key = BTreeMap::<String, u64>::new();
    for seed in 0..4096 {
        let program = Program::generate_for(campaign, seed);
        let plan = FaultPlan::for_program(
            campaign,
            seed,
            FaultProfile::None,
            &program,
            FaultSchedule::default(),
        );
        let selected = plan.feature.first().map(|event| event.fault);
        let format_key = match selected {
            Some(adversarial::campaign::FeatureFault::StorageCorruptSegmentRegion) => {
                "storage.format-case.segment-region"
            }
            Some(adversarial::campaign::FeatureFault::StorageWrongManifestObject) => {
                "storage.format-case.manifest-wrong-family"
            }
            Some(adversarial::campaign::FeatureFault::StorageWrongSegmentObject)
                if seed & 1 == 1 =>
            {
                "storage.format-case.segment-wrong-identity"
            }
            Some(adversarial::campaign::FeatureFault::StorageWrongSegmentObject) => {
                "storage.format-case.segment-wrong-family"
            }
            _ => match seed % 4 {
                0 => "storage.format-case.segment-region",
                1 => "storage.format-case.manifest-wrong-family",
                2 => "storage.format-case.segment-wrong-family",
                _ => "storage.format-case.segment-wrong-identity",
            },
        };
        seed_by_key.entry(format_key.to_owned()).or_insert(seed);
        let wal_key = match selected {
            Some(adversarial::campaign::FeatureFault::StorageTornWalHeader) => {
                Some("storage.format-case.wal-header")
            }
            Some(adversarial::campaign::FeatureFault::StorageTornWalBody) => {
                Some("storage.format-case.wal-record-body")
            }
            Some(adversarial::campaign::FeatureFault::StorageTornWalChecksum) => {
                Some("storage.format-case.wal-record-checksum")
            }
            _ => None,
        };
        if let Some(key) = wal_key {
            seed_by_key.entry(key.to_owned()).or_insert(seed);
        }
        if selected == Some(adversarial::campaign::FeatureFault::StorageListDeleteOmission) {
            let omission = adversarial::storage_durability::omission_case_for_schedule(seed, 0);
            let orphan = match omission.orphan {
                OrphanKind::FinalSegment => "final-segment",
                OrphanKind::SegmentTemporary => "segment-temporary",
                OrphanKind::ManifestTemporary => "manifest-temporary",
            };
            let subsite = match omission.subsite {
                OmissionSubsite::List => "list",
                OmissionSubsite::Delete => "delete",
            };
            seed_by_key
                .entry(format!("storage.omission.{orphan}.{subsite}"))
                .or_insert(seed);
        }
        if required.iter().all(|key| seed_by_key.contains_key(key)) {
            break;
        }
    }
    let missing_seeds = required
        .iter()
        .filter(|key| !seed_by_key.contains_key(*key))
        .collect::<Vec<_>>();
    assert!(missing_seeds.is_empty(), "no seed for {missing_seeds:?}");

    let mut coverage = CoverageRegistry::default();
    for seed in seed_by_key.values().copied().collect::<BTreeSet<_>>() {
        let artifacts = tempfile::tempdir().expect("storage catalog artifacts");
        let outcome = adversarial::runner::run_program_for(
            campaign,
            seed,
            FaultProfile::None,
            artifacts.path(),
        )
        .expect("storage catalog episode");
        assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
        coverage.merge(&outcome.coverage);
    }
    let missing = required
        .iter()
        .filter(|key| coverage.count(key) == 0)
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "missing executed storage cases: {missing:?}"
    );
}

#[test]
fn campaign_attestation_binds_every_merged_core_stream() {
    let config = RunConfig {
        campaign: CampaignKind::Fts,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters::default();
    let stream = |name: &str| adversarial::artifacts::MergedStreamStats {
        records: 1,
        bytes: 1,
        digest: format!("fnv1a64:{:016x}", name.len()),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: [
            "program",
            "faults",
            "violations",
            "coverage",
            "oracle",
            "controls",
            "receipts",
            "mutations",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), stream(name)))
        .collect(),
    };
    let attestation = campaign_attestation_json(&config, 1, &counters, Some(merged), None)
        .expect("feature attestation");

    for field in ["program", "faults", "violations", "coverage"] {
        assert!(
            attestation["evidence_digests"][field].as_str().is_some(),
            "campaign attestation omitted merged {field} digest"
        );
    }
}

#[test]
fn storage_campaign_attestation_embeds_the_strict_family_ledger() {
    let config = RunConfig {
        campaign: CampaignKind::StorageDurability,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I15".to_owned(), 1),
            ("I16".to_owned(), 1),
            ("I17".to_owned(), 1),
            ("I18".to_owned(), 1),
            ("I19".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I15".to_owned(), 1),
            ("I16".to_owned(), 1),
            ("I17".to_owned(), 1),
            ("I18".to_owned(), 1),
            ("I19".to_owned(), 1),
        ]),
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let stream = |name: &str, records: u64| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{:016x}", name.len()),
    };
    let mut streams = [
        ("program", 1),
        ("faults", 0),
        ("violations", 1),
        ("coverage", 1),
        ("oracle", 5),
        ("controls", 0),
        ("receipts", 0),
        ("mutations", 0),
    ]
    .into_iter()
    .map(|(name, records)| (name.to_owned(), stream(name, records)))
    .collect::<BTreeMap<_, _>>();
    for artifact in adversarial::artifacts::STORAGE_REPLAY_ARTIFACTS {
        streams.insert(format!("family/{artifact}"), stream(artifact, 1));
    }
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams,
    };
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::StorageDurability).all_required_coverage() {
        coverage.hit(key);
    }
    let attestation =
        campaign_attestation_json(&config, 1, &counters, Some(merged), Some(&coverage))
            .expect("storage feature attestation");
    assert_eq!(
        attestation["oracle_contract_versions"]["storage-durability"].as_str(),
        Some(zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION),
        "storage campaign attestation omitted its named oracle contract version"
    );
    validate_storage_oracle_attestation_shape(&attestation["storage_oracle_attestation"])
        .expect("strict nested storage attestation");
}

#[test]
fn storage_episode_writes_complete_attestation() {
    let artifacts = tempfile::tempdir().expect("storage episode attestation artifacts");
    let outcome = adversarial::runner::run_program_for(
        CampaignKind::StorageDurability,
        6,
        FaultProfile::None,
        artifacts.path(),
    )
    .expect("storage episode attestation run");
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    let episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&outcome.episode_bytes)
            .expect("parse storage episode.json");
    let storage = &episode["attestation"]["storage_oracle_attestation"];
    assert!(
        storage.is_object(),
        "storage episode omitted its family oracle attestation: {episode}"
    );
}

#[test]
fn storage_episode_serializes_the_shared_base_and_five_ordered_forks() {
    let campaign = CampaignKind::StorageDurability;
    let seed = 6;
    let root = tempfile::tempdir().expect("storage shared-base artifact root");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("storage shared-base episode");
    let fixture: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            outcome
                .family_artifact_bytes
                .get("storage-fixture.json")
                .expect("storage fixture artifact"),
        )
        .expect("parse storage fixture artifact");
    let base = fixture["episode_base"]
        .as_object()
        .expect("storage fixture omitted its closed episode base");
    assert!(
        base["inventory"]
            .as_array()
            .is_some_and(|files| !files.is_empty()),
        "storage episode base inventory is empty"
    );
    assert!(
        base["ack_ledger"]
            .as_array()
            .is_some_and(|acks| !acks.is_empty()),
        "storage episode base acknowledgement ledger is empty"
    );
    let forks = fixture["operation_forks"]
        .as_array()
        .expect("storage fixture omitted its operation forks");
    assert_eq!(forks.len(), 5);
    assert_eq!(
        forks
            .iter()
            .filter_map(|fork| fork["operation"].as_str())
            .collect::<Vec<_>>(),
        [
            "wal-prefix",
            "publication",
            "retry",
            "format-check",
            "orphan-cleanup",
        ]
    );
    assert!(forks.iter().all(|fork| {
        fork["source_digest"] == base["inventory_digest"]
            && fork["destination_digest"] == base["inventory_digest"]
            && fork["source_inventory"] == base["inventory"]
            && fork["destination_inventory"] == base["inventory"]
    }));
}

#[test]
fn storage_replay_metadata_rejects_a_stale_nested_checker_contract() {
    let campaign = CampaignKind::StorageDurability;
    let seed = 6_u64;
    let root = tempfile::tempdir().expect("storage nested attestation root");
    adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
        .expect("storage nested attestation episode");
    let directory = root
        .path()
        .join(format!("storage-durability/seed-{seed}-none"));
    let path = directory.join("episode.json");
    let mut episode: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(&path).expect("read storage episode metadata"),
        )
        .expect("parse storage episode metadata");
    episode["attestation"]["storage_oracle_attestation"]["per_invariant_comparisons"]["I15"]["checker_id"] =
        zeppelin_embed_bench::harness_json::json!("stale-checker");
    std::fs::write(
        &path,
        zeppelin_embed_bench::harness_json::to_vec_pretty(&episode)
            .expect("serialize planted storage episode metadata"),
    )
    .expect("write planted storage episode metadata");

    let error = adversarial::campaign::campaign_from_replay_metadata(&directory)
        .expect_err("stale nested storage checker contract must be rejected");
    assert!(error.contains("independent-oracle attestation"), "{error}");
}

#[test]
fn vector_campaign_attestation_embeds_the_exact_family_ledger() {
    let config = RunConfig {
        campaign: CampaignKind::VectorExecution,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counts = adversarial::vector_execution::expected_comparison_counts()
        .expect("vector family count contract");
    let counters = CampaignAttestationCounters {
        comparison_counts: counts
            .iter()
            .map(|(invariant, count)| ((*invariant).to_owned(), *count))
            .collect(),
        comparison_pass_counts: counts
            .into_iter()
            .map(|(invariant, count)| (invariant.to_owned(), count))
            .collect(),
        same_seed_clean_controls: 11,
        integrated_feature_fault_receipts: 11,
        expected_feature_fault_receipts: 11,
        selected_feature_fault_events: 11,
        replayed_seeds: 1,
    };
    let stream = |name: &str, records: u64| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{:016x}", name.len()),
    };
    let oracle_records = counters.comparison_counts.values().sum();
    let mut streams = [
        ("program", 1),
        ("faults", 0),
        ("violations", 1),
        ("coverage", 1),
        ("oracle", oracle_records),
        ("controls", 0),
        ("receipts", 0),
        ("mutations", 0),
    ]
    .into_iter()
    .map(|(name, records)| (name.to_owned(), stream(name, records)))
    .collect::<BTreeMap<_, _>>();
    for artifact in adversarial::artifacts::VECTOR_REPLAY_ARTIFACTS {
        streams.insert(format!("family/{artifact}"), stream(artifact, 1));
    }
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams,
    };
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::VectorExecution).all_required_coverage() {
        coverage.hit(key);
    }
    for fault in CampaignSpec::for_kind(CampaignKind::VectorExecution).feature_faults {
        let required_sites = match fault.key() {
            "corrupt-codes-factors" => 3,
            "missing-rescore-rows" => 2,
            "row-count-cancellation" => 4,
            _ => 1,
        };
        for _ in 1..required_sites {
            coverage.hit(fault.coverage_key());
        }
    }
    for backend in zeppelin_embed::kernels::KernelVariant::available() {
        coverage.hit(format!("kernel.backend.{}", backend.backend_id().as_str()));
    }
    let attestation =
        campaign_attestation_json(&config, 1, &counters, Some(merged), Some(&coverage))
            .expect("vector feature attestation");
    let vector = &attestation["vector_oracle_attestation"];
    validate_vector_oracle_attestation_shape(vector).expect("strict nested vector attestation");
    assert_eq!(vector["version"], 1);
    assert!(vector["fixture_digest"].is_string());
    assert_eq!(
        vector["per_invariant_comparisons"]
            .as_object()
            .map(|records| records.len()),
        Some(4)
    );
    assert!(
        vector["backend_inventory"]["available"]
            .as_array()
            .is_some_and(|backends| !backends.is_empty())
    );
}

#[test]
fn metadata_campaign_attestation_reports_the_exact_family_ledger() {
    let config = RunConfig {
        campaign: CampaignKind::MetadataFilterPlanner,
        seed: 0,
        start_seed: 0,
        profile: FaultProfile::None,
        qualification: Qualification::Exploratory,
        minimum_seconds: 0,
        minimum_episodes: 1,
        retain_successful: 1,
        artifacts: PathBuf::from("unused"),
        replay_directory: None,
    };
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            ("I37".to_owned(), 1),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            ("I37".to_owned(), 1),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        same_seed_clean_controls: 0,
        integrated_feature_fault_receipts: 0,
        expected_feature_fault_receipts: 0,
        selected_feature_fault_events: 0,
        replayed_seeds: 1,
    };
    let stream = |name: &str, records: u64| adversarial::artifacts::MergedStreamStats {
        records,
        bytes: records,
        digest: format!("fnv1a64:{:016x}", name.len()),
    };
    let merged = adversarial::artifacts::MergedEvidenceStats {
        episodes: 1,
        streams: BTreeMap::from([
            ("program".to_owned(), stream("program", 1)),
            ("faults".to_owned(), stream("faults", 0)),
            ("violations".to_owned(), stream("violations", 1)),
            ("coverage".to_owned(), stream("coverage", 1)),
            ("oracle".to_owned(), stream("oracle", 4)),
            ("controls".to_owned(), stream("controls", 0)),
            ("receipts".to_owned(), stream("receipts", 0)),
            ("mutations".to_owned(), stream("mutations", 0)),
            (
                "family/metadata-fixture.json".to_owned(),
                stream("metadata-fixture", 1),
            ),
            ("family/queries.jsonl".to_owned(), stream("queries", 1)),
            (
                "family/fixture-mutations.jsonl".to_owned(),
                stream("fixture-mutations", 1),
            ),
        ]),
    };
    let attestation = campaign_attestation_json(&config, 1, &counters, Some(merged), None)
        .expect("metadata feature attestation");
    let metadata = &attestation["metadata_oracle_attestation"];
    assert_eq!(metadata["version"], 1);
    assert_eq!(
        metadata["required_invariants"],
        zeppelin_embed_bench::harness_json::json!(["I36", "I37", "I38", "I39"])
    );
    assert_eq!(
        metadata["invariants"]["I36"]["checker_id"],
        zeppelin_embed_adversarial_oracle::metadata_filter_planner::I36_CHECKER_ID
    );
    assert!(metadata["oracle_source_digest"].is_string());
    assert!(metadata["repository"]["dirty_state"].is_string());
}

#[test]
fn metadata_selectivity_attestation_counts_two_receipts_per_control_pair() {
    let mut coverage = CoverageRegistry::default();
    let fault = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
        .feature_faults
        .iter()
        .find(|fault| fault.key() == "selectivity-boundary")
        .expect("metadata selectivity fault catalog entry");
    coverage.hit(fault.coverage_key());
    coverage.hit("metadata.receipt.selectivity-boundary.cardinality-two");
    let metadata = metadata_oracle_attestation_json(
        1,
        &CampaignAttestationCounters::default(),
        &BTreeMap::new(),
        Some(&coverage),
    );

    assert_eq!(
        metadata["faults"]["selectivity-boundary"]["same_seed_pairs"],
        1
    );
    assert_eq!(
        metadata["faults"]["selectivity-boundary"]["production_receipts"],
        2
    );
}

#[test]
fn metadata_attestation_uses_the_observed_replay_count() {
    let counters = CampaignAttestationCounters {
        replayed_seeds: 7,
        ..CampaignAttestationCounters::default()
    };
    let metadata = metadata_oracle_attestation_json(7, &counters, &BTreeMap::new(), None);
    assert_eq!(metadata["completed_seeds"], 7);
    assert_eq!(metadata["replayed_seeds"], 7);
}

#[test]
fn metadata_oracle_attestation_rejects_a_stale_checker_ledger() {
    let counters = CampaignAttestationCounters {
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let mut metadata = metadata_oracle_attestation_json(1, &counters, &BTreeMap::new(), None);
    metadata["invariants"]["I36"]["checker_id"] =
        zeppelin_embed_bench::harness_json::json!("generic-marker-v1");

    let error = validate_metadata_oracle_attestation_shape(&metadata)
        .expect_err("stale metadata checker ledger was accepted");
    assert!(error.contains("I36 checker_id"), "{error}");
}

#[test]
fn metadata_oracle_attestation_rejects_wrong_fault_receipt_cardinality() {
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner).all_required_coverage() {
        coverage.hit(key);
    }
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            (
                "I37".to_owned(),
                adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
            ),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I36".to_owned(), 1),
            (
                "I37".to_owned(),
                adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
            ),
            ("I38".to_owned(), 1),
            ("I39".to_owned(), 1),
        ]),
        ..CampaignAttestationCounters::default()
    };
    let mut metadata =
        metadata_oracle_attestation_json(1, &counters, &BTreeMap::new(), Some(&coverage));
    metadata["replayed_seeds"] = zeppelin_embed_bench::harness_json::json!(1);
    validate_metadata_oracle_attestation_shape(&metadata)
        .expect("complete metadata attestation fixture");

    metadata["faults"]["selectivity-boundary"]["production_receipts"] =
        zeppelin_embed_bench::harness_json::json!(1);
    let error = validate_metadata_oracle_attestation_shape(&metadata)
        .expect_err("wrong metadata receipt cardinality was accepted");
    assert!(error.contains("selectivity-boundary"), "{error}");

    metadata["faults"]["selectivity-boundary"]["production_receipts"] =
        zeppelin_embed_bench::harness_json::json!(2);
    metadata["replayed_seeds"] = zeppelin_embed_bench::harness_json::json!(0);
    let error = validate_metadata_oracle_attestation_shape(&metadata)
        .expect_err("incomplete metadata replay was accepted");
    assert!(error.contains("replayed_seeds"), "{error}");

    metadata["replayed_seeds"] = zeppelin_embed_bench::harness_json::json!(1);
    let comparisons = BTreeMap::from([
        ("I36".to_owned(), 1),
        (
            "I37".to_owned(),
            adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
        ),
        ("I38".to_owned(), 1),
        ("I39".to_owned(), 1),
    ]);
    let fault_pairs = BTreeMap::from([
        ("column-corruption".to_owned(), 1),
        ("bitmap-truncation".to_owned(), 1),
        ("selectivity-boundary".to_owned(), 1),
        ("visited-budget-fallback".to_owned(), 1),
    ]);
    let receipts = BTreeMap::from([
        ("column-corruption".to_owned(), 1),
        ("bitmap-truncation".to_owned(), 1),
        ("selectivity-boundary".to_owned(), 1),
        ("visited-budget-fallback".to_owned(), 1),
    ]);
    let branches = [
        "pruned",
        "exact-allow-list",
        "masked-scan",
        "filtered-graph",
        "graph-exact-fallback",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), 1))
    .collect();
    let fallbacks = ["none", "visited-budget", "candidate-shortfall"]
        .into_iter()
        .map(|key| (key.to_owned(), 1))
        .collect();
    let error = validate_metadata_observed_ledgers(
        &metadata,
        &comparisons,
        &comparisons,
        &fault_pairs,
        &receipts,
        &branches,
        &fallbacks,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        1,
    )
    .expect_err("summary receipt counts were not bound to merged receipts");
    assert!(error.contains("selectivity-boundary"), "{error}");
}

#[test]
fn metadata_attestation_rejects_duplicate_i37_oracle_case_identity() {
    let mut coverage = CoverageRegistry::default();
    for operation in [
        "metadata_columns_roundtrip",
        "metadata_bitmap_algebra",
        "metadata_pruning_soundness",
        "metadata_execution_truth",
    ] {
        coverage.hit(format!("campaign.op.metadata-filter-planner.{operation}"));
    }
    let mut coverage_counts = BTreeMap::new();
    let mut oracle_case_counts = BTreeMap::new();
    for index in 0..adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT {
        let case = adversarial::metadata_filter_planner::i37_predicate_case_key(index);
        let coverage_key = format!("metadata.i37.matrix.{case}");
        coverage.hit(coverage_key.clone());
        coverage_counts.insert(coverage_key, 1);
        oracle_case_counts.insert(case.to_owned(), 1);
    }
    let comparisons = BTreeMap::from([
        ("I36".to_owned(), 1),
        (
            "I37".to_owned(),
            adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT,
        ),
        ("I38".to_owned(), 1),
        ("I39".to_owned(), 1),
    ]);
    let counters = CampaignAttestationCounters {
        comparison_counts: comparisons.clone(),
        comparison_pass_counts: comparisons.clone(),
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let metadata =
        metadata_oracle_attestation_json(1, &counters, &BTreeMap::new(), Some(&coverage));
    let faults = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
        .feature_faults
        .iter()
        .map(|fault| (fault.key().to_owned(), 0))
        .collect::<BTreeMap<_, _>>();
    let branches = [
        "pruned",
        "exact-allow-list",
        "masked-scan",
        "filtered-graph",
        "graph-exact-fallback",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), 0))
    .collect::<BTreeMap<_, _>>();
    let fallbacks = ["none", "visited-budget", "candidate-shortfall"]
        .into_iter()
        .map(|key| (key.to_owned(), 0))
        .collect::<BTreeMap<_, _>>();
    let fault_cases = oracle_case_counts
        .keys()
        .map(|case| (case.clone(), 0))
        .collect::<BTreeMap<_, _>>();
    validate_metadata_observed_ledgers(
        &metadata,
        &comparisons,
        &comparisons,
        &faults,
        &faults,
        &branches,
        &fallbacks,
        &coverage_counts,
        &oracle_case_counts,
        &fault_cases,
        1,
    )
    .expect("complete one-episode I37 case ledger");

    *oracle_case_counts
        .get_mut("eq-u64")
        .expect("I37 eq-u64 case") = 2;
    *oracle_case_counts
        .get_mut("eq-i64")
        .expect("I37 eq-i64 case") = 0;
    let error = validate_metadata_observed_ledgers(
        &metadata,
        &comparisons,
        &comparisons,
        &faults,
        &faults,
        &branches,
        &fallbacks,
        &coverage_counts,
        &oracle_case_counts,
        &fault_cases,
        1,
    )
    .expect_err("duplicated I37 oracle case identity replaced a missing case");
    assert!(error.contains("eq-u64"), "{error}");
}

#[test]
fn retained_storage_summary_without_family_attestation_is_rejected() {
    let old_summary = zeppelin_embed_bench::harness_json::json!({});
    let error = validate_storage_oracle_attestation_shape(&old_summary)
        .expect_err("legacy generic storage summary was accepted");
    assert_eq!(
        error,
        "storage-durability attestation missing: oracle_contract_version"
    );
}

#[test]
fn storage_verifier_rejects_legacy_false_credit() {
    retained_storage_summary_without_family_attestation_is_rejected();
}

#[test]
fn retained_vector_summary_without_family_attestation_is_rejected() {
    let old_summary = zeppelin_embed_bench::harness_json::json!({});
    let error = validate_vector_oracle_attestation_shape(&old_summary)
        .expect_err("legacy generic vector summary was accepted");
    assert_eq!(
        error,
        "vector-execution attestation missing: oracle_contract"
    );
}

#[test]
fn feature_summary_verifier_requires_the_nested_vector_attestation() {
    let root = tempfile::tempdir().expect("vector verifier root");
    let summary = zeppelin_embed_bench::harness_json::json!({
        "attestation": {"vector_oracle_attestation": {}},
    });
    let error =
        verify_feature_summary_attestation(root.path(), CampaignKind::VectorExecution, 1, &summary)
            .expect_err("top-level verifier accepted a missing vector family ledger");
    assert_eq!(
        error,
        "vector-execution attestation missing: oracle_contract"
    );
}

#[test]
fn vector_attestation_rejects_an_empty_runtime_backend_inventory() {
    let selected_faults = 11;
    let counts = adversarial::vector_execution::expected_comparison_counts()
        .expect("vector family count contract");
    let counters = CampaignAttestationCounters {
        comparison_counts: counts
            .iter()
            .map(|(invariant, count)| ((*invariant).to_owned(), *count))
            .collect(),
        comparison_pass_counts: counts
            .into_iter()
            .map(|(invariant, count)| (invariant.to_owned(), count))
            .collect(),
        same_seed_clean_controls: selected_faults,
        integrated_feature_fault_receipts: selected_faults,
        expected_feature_fault_receipts: selected_faults,
        selected_feature_fault_events: selected_faults,
        replayed_seeds: 1,
    };
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::VectorExecution).all_required_coverage() {
        coverage.hit(key);
    }
    for fault in CampaignSpec::for_kind(CampaignKind::VectorExecution).feature_faults {
        let required_sites = match fault.key() {
            "corrupt-codes-factors" => 3,
            "missing-rescore-rows" => 2,
            "row-count-cancellation" => 4,
            _ => 1,
        };
        for _ in 1..required_sites {
            coverage.hit(fault.coverage_key());
        }
    }
    let digest = "fnv1a64:0123456789abcdef".to_owned();
    let evidence_digests = BTreeMap::from([
        ("program".to_owned(), digest.clone()),
        ("faults".to_owned(), digest.clone()),
        ("violations".to_owned(), digest.clone()),
        ("coverage".to_owned(), digest.clone()),
        ("checker".to_owned(), digest.clone()),
        ("control".to_owned(), digest.clone()),
        ("receipt".to_owned(), digest.clone()),
        ("mutation".to_owned(), digest.clone()),
        ("family/fixture.json".to_owned(), digest),
    ]);
    let mut vector =
        vector_oracle_attestation_json(1, &counters, &evidence_digests, Some(&coverage));
    let detected = zeppelin_embed::kernels::detected_features();
    let features = vector["backend_inventory"]["features"]
        .as_object()
        .expect("vector runtime features must be named booleans");
    for (name, expected) in [
        ("neon", detected.neon),
        ("dotprod", detected.dotprod),
        ("fp16", detected.fp16),
        ("i8mm", detected.i8mm),
        ("sme2", detected.sme2),
        ("avx2", detected.avx2),
        ("popcnt", detected.popcnt),
    ] {
        assert_eq!(features[name].as_bool(), Some(expected), "feature {name}");
    }
    vector["backend_inventory"]["available"] = zeppelin_embed_bench::harness_json::json!([]);

    let error = validate_vector_oracle_attestation_shape(&vector)
        .expect_err("empty vector runtime backend inventory was accepted");
    assert!(error.contains("available backend inventory"), "{error}");
}

#[test]
fn storage_oracle_attestation_has_exact_checker_case_and_receipt_catalogs() {
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I15".to_owned(), 1),
            ("I16".to_owned(), 1),
            ("I17".to_owned(), 1),
            ("I18".to_owned(), 1),
            ("I19".to_owned(), 1),
        ]),
        comparison_pass_counts: BTreeMap::from([
            ("I15".to_owned(), 1),
            ("I16".to_owned(), 1),
            ("I17".to_owned(), 1),
            ("I18".to_owned(), 1),
            ("I19".to_owned(), 1),
        ]),
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::StorageDurability).all_required_coverage() {
        coverage.hit(key);
    }
    let digest = "0123456789abcdef".to_owned();
    let evidence_digests = BTreeMap::from([
        ("program".to_owned(), digest.clone()),
        ("faults".to_owned(), digest.clone()),
        ("violations".to_owned(), digest.clone()),
        ("coverage".to_owned(), digest.clone()),
        ("checker".to_owned(), digest.clone()),
        ("control".to_owned(), digest.clone()),
        ("receipt".to_owned(), digest.clone()),
        ("mutation".to_owned(), digest),
    ]);
    let storage = storage_oracle_attestation_json(1, &counters, &evidence_digests, Some(&coverage));
    validate_storage_oracle_attestation_shape(&storage)
        .expect("complete storage oracle attestation");
    assert_eq!(
        storage["required_invariants"].as_array().map(Vec::len),
        Some(5)
    );
    assert_eq!(
        storage["format_cases"].as_object().map(|map| map.len()),
        Some(7)
    );
    assert_eq!(
        storage["omission_cases"].as_object().map(|map| map.len()),
        Some(6)
    );
    assert_eq!(
        storage["receipt_sites"].as_object().map(|map| map.len()),
        Some(12)
    );
}

#[test]
fn storage_attestation_rejects_a_case_count_not_present_in_merged_coverage() {
    let storage = zeppelin_embed_bench::harness_json::json!({
        "operations": {"publication": {"executions": 1}},
        "faults": {},
        "format_cases": {"wal-header": 2},
        "omission_cases": {},
        "receipt_sites": {},
    });
    let observed = BTreeMap::from([
        ("campaign.op.storage-durability.publication".to_owned(), 1),
        ("storage.format-case.wal-header".to_owned(), 1),
    ]);
    let error = validate_storage_attested_coverage(&storage, &observed)
        .expect_err("fabricated storage format-case count was accepted");
    assert!(error.contains("wal-header"), "{error}");
    assert!(error.contains("attested=2 observed=1"), "{error}");
}

#[test]
fn storage_attestation_rejects_a_receipt_count_not_present_in_merged_receipts() {
    let counters = CampaignAttestationCounters {
        comparison_counts: BTreeMap::from([
            ("I15".to_owned(), 1),
            ("I16".to_owned(), 1),
            ("I17".to_owned(), 1),
            ("I18".to_owned(), 1),
            ("I19".to_owned(), 1),
        ]),
        replayed_seeds: 1,
        ..CampaignAttestationCounters::default()
    };
    let mut coverage = CoverageRegistry::default();
    for key in CampaignSpec::for_kind(CampaignKind::StorageDurability).all_required_coverage() {
        coverage.hit(key);
    }
    let storage = storage_oracle_attestation_json(1, &counters, &BTreeMap::new(), Some(&coverage));
    let fault_pairs = CampaignSpec::for_kind(CampaignKind::StorageDurability)
        .feature_faults
        .iter()
        .map(|fault| (fault.key().to_owned(), 1_u64))
        .collect::<BTreeMap<_, _>>();
    let mut production_receipts = fault_pairs.clone();
    production_receipts.insert("torn-wal-header".to_owned(), 0);
    let receipt_sites = storage["receipt_sites"]
        .as_object()
        .expect("storage receipt-site attestation")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_u64().unwrap_or(0)))
        .collect::<BTreeMap<_, _>>();
    let ledgers = StorageMergedLedgers {
        fault_pairs: fault_pairs.clone(),
        same_seed_controls: fault_pairs,
        production_receipts,
        receipt_sites,
        retained_artifacts: 1,
    };

    let error =
        validate_storage_observed_ledgers(&storage, &counters.comparison_counts, &ledgers, 1)
            .expect_err("storage attestation accepted a fabricated production receipt count");
    assert!(
        error.contains("production receipt ledger mismatch"),
        "{error}"
    );
}

#[test]
fn vector_attestation_rejects_a_site_count_not_present_in_merged_coverage() {
    let vector = zeppelin_embed_bench::harness_json::json!({
        "same_seed_controls": {
            "operations": {"kernel-parity": 1},
            "faults": {"forced-dispatch-backend": 1},
        },
        "integrated_receipts": {
            "faults": {"forced-dispatch-backend": 1},
            "sites": {
                "fault.forced-backend.kernel-dispatch-selected-scoring-table": 2,
            },
        },
        "backend_inventory": {
            "selected": ["scalar"],
            "observed": ["scalar"],
        },
    });
    let observed = BTreeMap::from([
        ("campaign.op.vector-execution.kernel-parity".to_owned(), 1),
        (
            "feature_fault.vector-execution.forced-dispatch-backend".to_owned(),
            1,
        ),
        (
            "fault.forced-backend.kernel-dispatch-selected-scoring-table".to_owned(),
            1,
        ),
        ("I24.store-selected.scalar".to_owned(), 1),
        ("kernel.backend.scalar".to_owned(), 1),
    ]);
    let error = validate_vector_attested_coverage(&vector, &observed)
        .expect_err("fabricated vector receipt-site count was accepted");
    assert!(
        error.contains("kernel-dispatch-selected-scoring-table"),
        "{error}"
    );
    assert!(error.contains("attested=2 observed=1"), "{error}");
}

#[test]
fn vector_attestation_accepts_repeated_backend_coverage() {
    let vector = zeppelin_embed_bench::harness_json::json!({
        "same_seed_controls": {
            "operations": {},
            "faults": {},
        },
        "integrated_receipts": {
            "faults": {},
            "sites": {},
        },
        "generic_fault_pairs": {
            "scheduled": 0,
            "clean_fired": 0,
            "fault_fired": 0,
            "same_path": 0,
            "isolated_directories": 0,
            "isolated_runtimes": 0,
            "typed_feature_receipts": 0,
        },
        "backend_inventory": {
            "selected": ["neon-dotprod-u4"],
            "observed": ["neon-dotprod-u4"],
        },
    });
    let observed = BTreeMap::from([
        ("I24.store-selected.neon-dotprod-u4".to_owned(), 1_199),
        ("kernel.backend.neon-dotprod-u4".to_owned(), 1_497),
    ]);

    validate_vector_attested_coverage(&vector, &observed)
        .expect("repeated backend coverage should attest backend presence");
}

#[test]
fn vector_attestation_rejects_a_comparison_count_not_present_in_merged_oracle() {
    let vector = zeppelin_embed_bench::harness_json::json!({
        "per_invariant_comparisons": {
            "I24": {"comparisons": 2},
            "I25": {"comparisons": 1},
            "I26": {"comparisons": 1},
            "I27": {"comparisons": 1},
        },
    });
    let observed = BTreeMap::from([
        ("I24".to_owned(), 1),
        ("I25".to_owned(), 1),
        ("I26".to_owned(), 1),
        ("I27".to_owned(), 1),
    ]);
    let error = validate_vector_attested_comparisons(&vector, &observed)
        .expect_err("fabricated vector comparison count was accepted");
    assert!(error.contains("I24"), "{error}");
    assert!(error.contains("attested=2 observed=1"), "{error}");
}

#[test]
fn vector_attestation_rejects_a_generic_pair_count_not_present_in_merged_controls() {
    let vector = zeppelin_embed_bench::harness_json::json!({
        "generic_fault_pairs": {
            "scheduled": 2,
            "clean_fired": 2,
            "fault_fired": 2,
            "same_path": 2,
            "isolated_directories": 2,
            "isolated_runtimes": 2,
            "typed_feature_receipts": 2,
        },
    });
    let observed = VectorGenericPairLedger {
        scheduled: 1,
        clean_fired: 1,
        fault_fired: 1,
        same_path: 1,
        isolated_directories: 1,
        isolated_runtimes: 1,
        typed_feature_receipts: 1,
    };
    let error = validate_vector_attested_generic_pairs(&vector, &observed)
        .expect_err("fabricated vector generic fault-pair count was accepted");
    assert!(error.contains("scheduled"), "{error}");
    assert!(error.contains("attested=2 observed=1"), "{error}");
}

fn merged_stream_json(
    merged: &adversarial::artifacts::MergedEvidenceStats,
    name: &str,
) -> zeppelin_embed_bench::harness_json::Value {
    let stream = merged
        .streams
        .get(name)
        .unwrap_or_else(|| panic!("merged evidence omitted {name}"));
    zeppelin_embed_bench::harness_json::json!({
        "records": stream.records,
        "bytes": stream.bytes,
        "digest": stream.digest,
    })
}

fn validate_ingest_retention_oracle_attestation_shape(
    ingest: &zeppelin_embed_bench::harness_json::Value,
) -> Result<(), String> {
    if ingest["oracle_contract_version"].as_str()
        != Some(zeppelin_embed_adversarial_oracle::ingest_retention::ORACLE_CONTRACT_VERSION)
    {
        return Err("ingest-retention attestation missing: oracle_contract_version".to_owned());
    }
    if ingest["version"].as_u64() != Some(1) {
        return Err("ingest-retention attestation missing: version 1".to_owned());
    }
    let expected_source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/ingest_retention.rs"
    )]);
    if ingest["oracle_source_digest"].as_str() != Some(expected_source_digest.as_str()) {
        return Err("ingest-retention oracle source digest differs".to_owned());
    }
    if ingest["repository"]["revision"].as_str()
        != Some(adversarial::artifacts::harness_git_revision())
        || ingest["repository"]["dirty_state"].as_str()
            != Some(adversarial::artifacts::harness_git_dirty_state())
    {
        return Err("ingest-retention repository attestation differs".to_owned());
    }
    let exact_object = |label: &str,
                        value: &zeppelin_embed_bench::harness_json::Value,
                        expected: &[&str]|
     -> Result<(), String> {
        let observed = value
            .as_object()
            .ok_or_else(|| format!("ingest-retention {label} is not an object"))?
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        if observed != expected {
            return Err(format!(
                "ingest-retention {label} keys differ expected={expected:?} observed={observed:?}"
            ));
        }
        Ok(())
    };
    let expected_invariants = ["I20", "I21", "I22", "I23"];
    let required_invariants = ingest["required_invariants"]
        .as_array()
        .ok_or_else(|| "ingest-retention required_invariants is not an array".to_owned())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "ingest-retention invariant name is not a string".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if required_invariants != expected_invariants {
        return Err(format!(
            "ingest-retention required invariants differ: {required_invariants:?}"
        ));
    }
    exact_object("invariants", &ingest["invariants"], &expected_invariants)?;
    for (invariant, checker_id) in [
        (
            "I20",
            zeppelin_embed_adversarial_oracle::ingest_retention::I20_CHECKER_ID,
        ),
        (
            "I21",
            zeppelin_embed_adversarial_oracle::ingest_retention::I21_CHECKER_ID,
        ),
        (
            "I22",
            zeppelin_embed_adversarial_oracle::ingest_retention::I22_CHECKER_ID,
        ),
        (
            "I23",
            zeppelin_embed_adversarial_oracle::ingest_retention::I23_CHECKER_ID,
        ),
    ] {
        let record = &ingest["invariants"][invariant];
        if record["checker_id"].as_str() != Some(checker_id)
            || record["comparisons"].as_u64().is_none()
            || record["passes"].as_u64().is_none()
            || record["passes"].as_u64() > record["comparisons"].as_u64()
            || record["plants"].as_u64() != Some(0)
        {
            return Err(format!(
                "ingest-retention {invariant} checker/count ledger differs"
            ));
        }
    }
    exact_object(
        "operations",
        &ingest["operations"],
        &["batch-commit", "seal", "retention", "purge"],
    )?;
    for operation in ["batch-commit", "seal", "retention", "purge"] {
        if ingest["operations"][operation]["executions"]
            .as_u64()
            .is_none()
            || ingest["operations"][operation]["qualifying_checks"]
                .as_u64()
                .is_none()
        {
            return Err(format!(
                "ingest-retention {operation} operation ledger differs"
            ));
        }
    }
    let fault_keys = CampaignSpec::for_kind(CampaignKind::IngestRetention)
        .feature_faults
        .iter()
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    exact_object("faults", &ingest["faults"], &fault_keys)?;
    for fault in &fault_keys {
        if ingest["faults"][fault]["same_seed_pairs"]
            .as_u64()
            .is_none()
            || ingest["faults"][fault]["production_receipts"]
                .as_u64()
                .is_none()
        {
            return Err(format!("ingest-retention {fault} fault ledger differs"));
        }
    }
    exact_object(
        "receipt sites",
        &ingest["receipt_sites"],
        &[
            "ingest.replay.no-wal-append",
            "ingest.commit-many.append-error",
            "seal.after-segment-write.before-manifest-commit",
            "retention.policy-evaluated",
            "purge.old-segment-unlink.error",
            "purge.after-durable-intent.before-rewrite",
        ],
    )?;
    if ingest["receipt_sites"]
        .as_object()
        .expect("validated receipt-site object")
        .values()
        .any(|count| count.as_u64().is_none())
    {
        return Err("ingest-retention receipt-site count is not u64".to_owned());
    }
    if ingest["evidence_digests"].as_object().is_none() {
        return Err("ingest-retention evidence digests are absent".to_owned());
    }
    let completed = ingest["completed_seeds"]
        .as_u64()
        .ok_or_else(|| "ingest-retention completed seed count is absent".to_owned())?;
    if completed == 0 || ingest["replayed_seeds"].as_u64() != Some(completed) {
        return Err(
            "ingest-retention completed/replayed seed counts are zero or differ".to_owned(),
        );
    }
    Ok(())
}

fn validate_vector_oracle_attestation_shape(
    vector: &zeppelin_embed_bench::harness_json::Value,
) -> Result<(), String> {
    if vector["oracle_contract"].as_str()
        != Some(zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_ORACLE_CONTRACT)
    {
        return Err("vector-execution attestation missing: oracle_contract".to_owned());
    }
    if vector["version"].as_u64() != Some(1) {
        return Err("vector-execution attestation missing: version 1".to_owned());
    }
    let expected_source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/vector_execution.rs"
    )]);
    if vector["oracle_source_digest"].as_str() != Some(expected_source_digest.as_str()) {
        return Err("vector-execution oracle source digest differs".to_owned());
    }
    if vector["harness_git_revision"].as_str()
        != Some(adversarial::artifacts::harness_git_revision())
        || vector["dirty_state"].as_str() != Some(adversarial::artifacts::harness_git_dirty_state())
    {
        return Err("vector-execution repository attestation differs".to_owned());
    }
    let fixture_digest = vector["fixture_digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| "vector-execution fixture digest is absent".to_owned())?;
    if vector["evidence_digests"]["family/fixture.json"].as_str() != Some(fixture_digest) {
        return Err("vector-execution fixture digest differs from merged evidence".to_owned());
    }

    let exact_keys = |label: &str,
                      value: &zeppelin_embed_bench::harness_json::Value,
                      expected: BTreeSet<String>|
     -> Result<(), String> {
        let observed = value
            .as_object()
            .ok_or_else(|| format!("vector-execution {label} is not an object"))?
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if observed != expected {
            return Err(format!(
                "vector-execution {label} keys differ expected={expected:?} observed={observed:?}"
            ));
        }
        Ok(())
    };
    let expected_invariants = ["I24", "I25", "I26", "I27"]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    exact_keys(
        "per-invariant comparisons",
        &vector["per_invariant_comparisons"],
        expected_invariants,
    )?;
    for (invariant, checker_id) in [
        (
            "I24",
            zeppelin_embed_adversarial_oracle::vector_execution::I24_CHECKER_ID,
        ),
        (
            "I25",
            zeppelin_embed_adversarial_oracle::vector_execution::I25_CHECKER_ID,
        ),
        (
            "I26",
            zeppelin_embed_adversarial_oracle::vector_execution::I26_CHECKER_ID,
        ),
        (
            "I27",
            zeppelin_embed_adversarial_oracle::vector_execution::I27_CHECKER_ID,
        ),
    ] {
        let record = &vector["per_invariant_comparisons"][invariant];
        let comparisons = record["comparisons"]
            .as_u64()
            .ok_or_else(|| format!("vector-execution {invariant} comparisons are absent"))?;
        if comparisons == 0
            || record["checker_id"].as_str() != Some(checker_id)
            || record["passes"].as_u64() != Some(comparisons)
            || record["plants"].as_u64() != Some(0)
        {
            return Err(format!(
                "vector-execution {invariant} checker/pass/count attestation differs"
            ));
        }
    }

    let expected_operations = ["kernel-parity", "quantization", "rescore", "row-identity"]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    exact_keys(
        "same-seed operations",
        &vector["same_seed_controls"]["operations"],
        expected_operations,
    )?;
    for (operation, count) in vector["same_seed_controls"]["operations"]
        .as_object()
        .ok_or_else(|| "vector-execution same-seed operations are absent".to_owned())?
    {
        count
            .as_u64()
            .ok_or_else(|| format!("vector-execution operation {operation} count is not u64"))?;
    }
    let expected_faults = CampaignSpec::for_kind(CampaignKind::VectorExecution)
        .feature_faults
        .iter()
        .map(|fault| fault.key().to_owned())
        .collect::<BTreeSet<_>>();
    exact_keys(
        "same-seed faults",
        &vector["same_seed_controls"]["faults"],
        expected_faults.clone(),
    )?;
    exact_keys(
        "receipt faults",
        &vector["integrated_receipts"]["faults"],
        expected_faults,
    )?;
    let fault_sum = vector["same_seed_controls"]["faults"]
        .as_object()
        .ok_or_else(|| "vector-execution same-seed faults are absent".to_owned())?
        .iter()
        .try_fold(0_u64, |total, (fault, count)| {
            let count = count
                .as_u64()
                .ok_or_else(|| format!("vector-execution fault {fault} count is not u64"))?;
            total
                .checked_add(count)
                .ok_or_else(|| "vector-execution fault count overflowed".to_owned())
        })?;
    if vector["same_seed_controls"]["pairs"].as_u64() != Some(fault_sum)
        || vector["same_seed_controls"]["passed"].as_u64() != Some(fault_sum)
        || vector["integrated_receipts"]["expected"].as_u64() != Some(fault_sum)
        || vector["integrated_receipts"]["observed"].as_u64() != Some(fault_sum)
    {
        return Err("vector-execution control/receipt totals differ from fault ledger".to_owned());
    }
    let expected_sites = CampaignSpec::for_kind(CampaignKind::VectorExecution)
        .all_required_coverage()
        .into_iter()
        .filter(|key| key.starts_with("fault."))
        .collect::<BTreeSet<_>>();
    exact_keys(
        "receipt sites",
        &vector["integrated_receipts"]["sites"],
        expected_sites,
    )?;
    let site_sum = vector["integrated_receipts"]["sites"]
        .as_object()
        .ok_or_else(|| "vector-execution receipt sites are absent".to_owned())?
        .iter()
        .try_fold(0_u64, |total, (site, count)| {
            let count = count
                .as_u64()
                .ok_or_else(|| format!("vector-execution site {site} count is not u64"))?;
            total
                .checked_add(count)
                .ok_or_else(|| "vector-execution receipt site count overflowed".to_owned())
        })?;
    if site_sum != fault_sum {
        return Err(format!(
            "vector-execution receipt site total differs expected={fault_sum} observed={site_sum}"
        ));
    }
    let generic = &vector["generic_fault_pairs"];
    let generic_scheduled = generic["scheduled"]
        .as_u64()
        .ok_or_else(|| "vector-execution generic fault-pair count is absent".to_owned())?;
    for field in [
        "clean_fired",
        "fault_fired",
        "same_path",
        "isolated_directories",
        "isolated_runtimes",
        "typed_feature_receipts",
    ] {
        if generic[field].as_u64() != Some(generic_scheduled) {
            return Err(format!(
                "vector-execution generic fault-pair {field} differs from scheduled"
            ));
        }
    }
    validate_vector_attested_generic_pairs(
        vector,
        &VectorGenericPairLedger {
            scheduled: generic_scheduled,
            clean_fired: generic_scheduled,
            fault_fired: generic_scheduled,
            same_path: generic_scheduled,
            isolated_directories: generic_scheduled,
            isolated_runtimes: generic_scheduled,
            typed_feature_receipts: generic_scheduled,
        },
    )?;

    let string_set = |label: &str,
                      value: &zeppelin_embed_bench::harness_json::Value|
     -> Result<BTreeSet<String>, String> {
        value
            .as_array()
            .ok_or_else(|| format!("vector-execution {label} is not an array"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("vector-execution {label} entry is not a string"))
            })
            .collect()
    };
    let available = string_set(
        "available backend inventory",
        &vector["backend_inventory"]["available"],
    )?;
    let expected_available = zeppelin_embed::kernels::KernelVariant::available()
        .map(|variant| variant.backend_id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    if available.is_empty() || available != expected_available {
        return Err(format!(
            "vector-execution available backend inventory differs expected={expected_available:?} observed={available:?}"
        ));
    }
    let features = vector["backend_inventory"]["features"]
        .as_object()
        .ok_or_else(|| "vector-execution backend features are not an object".to_owned())?;
    let expected_feature_keys = ["neon", "dotprod", "fp16", "i8mm", "sme2", "avx2", "popcnt"]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if features.keys().cloned().collect::<BTreeSet<_>>() != expected_feature_keys {
        return Err("vector-execution backend feature keys differ".to_owned());
    }
    let detected = zeppelin_embed::kernels::detected_features();
    for (name, expected) in [
        ("neon", detected.neon),
        ("dotprod", detected.dotprod),
        ("fp16", detected.fp16),
        ("i8mm", detected.i8mm),
        ("sme2", detected.sme2),
        ("avx2", detected.avx2),
        ("popcnt", detected.popcnt),
    ] {
        if features[name].as_bool() != Some(expected) {
            return Err(format!(
                "vector-execution backend feature {name} differs from runtime detection"
            ));
        }
    }
    let all_backends = [
        "scalar",
        "neon-widen",
        "neon-dotprod-u4",
        "neon-i8mm",
        "neon-dotprod-u2",
        "neon-dotprod-u6",
        "neon-dotprod-u8",
        "neon-dotprod-u4-prefetch",
        "avx2",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    let unavailable = string_set(
        "unavailable backend inventory",
        &vector["backend_inventory"]["unavailable"],
    )?;
    if unavailable != all_backends.difference(&available).cloned().collect() {
        return Err("vector-execution unavailable backend inventory differs".to_owned());
    }
    let selected = string_set(
        "selected backend inventory",
        &vector["backend_inventory"]["selected"],
    )?;
    let observed = string_set(
        "observed backend inventory",
        &vector["backend_inventory"]["observed"],
    )?;
    if selected.is_empty() || !selected.is_subset(&available) {
        return Err(
            "vector-execution selected backend inventory is empty or unavailable".to_owned(),
        );
    }
    if observed != available {
        return Err(
            "vector-execution observed backend inventory differs from available".to_owned(),
        );
    }
    if vector["backend_inventory"]["host"]["os"].as_str() != Some(std::env::consts::OS)
        || vector["backend_inventory"]["host"]["arch"].as_str() != Some(std::env::consts::ARCH)
    {
        return Err("vector-execution host backend inventory differs".to_owned());
    }
    for (label, object) in [
        ("operation evidence", &vector["operation_evidence_digests"]),
        ("checker evidence", &vector["checker_evidence_digests"]),
        ("fault evidence", &vector["fault_evidence_digests"]),
    ] {
        for (field, digest) in object
            .as_object()
            .ok_or_else(|| format!("vector-execution {label} digests are absent"))?
        {
            if digest.as_str().is_none_or(str::is_empty) {
                return Err(format!("vector-execution {label} digest {field} is absent"));
            }
        }
    }
    let completed = vector["completed_seeds"]
        .as_u64()
        .ok_or_else(|| "vector-execution completed seed count is absent".to_owned())?;
    if completed == 0 || vector["replayed_seeds"].as_u64() != Some(completed) {
        return Err(
            "vector-execution completed/replayed seed counts are zero or differ".to_owned(),
        );
    }
    Ok(())
}

fn validate_storage_oracle_attestation_shape(
    storage: &zeppelin_embed_bench::harness_json::Value,
) -> Result<(), String> {
    if storage["oracle_contract_version"].as_str()
        != Some(zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION)
    {
        return Err("storage-durability attestation missing: oracle_contract_version".to_owned());
    }
    if storage["version"].as_u64() != Some(1) {
        return Err("storage-durability attestation missing: version 1".to_owned());
    }
    let expected_source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/storage_durability.rs"
    )]);
    if storage["oracle_source_digest"].as_str() != Some(expected_source_digest.as_str()) {
        return Err("storage-durability attestation oracle source digest differs".to_owned());
    }
    if storage["repository"]["revision"].as_str()
        != Some(adversarial::artifacts::harness_git_revision())
        || storage["repository"]["dirty_state"].as_str()
            != Some(adversarial::artifacts::harness_git_dirty_state())
    {
        return Err("storage-durability repository attestation differs".to_owned());
    }
    let expected_invariants = ["I15", "I16", "I17", "I18", "I19"];
    let required_invariants = storage["required_invariants"]
        .as_array()
        .ok_or_else(|| "storage-durability required_invariants is not an array".to_owned())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "storage-durability invariant name is not a string".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if required_invariants != expected_invariants {
        return Err(format!(
            "storage-durability required invariants differ: {required_invariants:?}"
        ));
    }
    let exact_object = |label: &str,
                        value: &zeppelin_embed_bench::harness_json::Value,
                        expected: &[&str]|
     -> Result<(), String> {
        let observed = value
            .as_object()
            .ok_or_else(|| format!("storage-durability {label} is not an object"))?
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        if observed != expected {
            return Err(format!(
                "storage-durability {label} keys differ expected={expected:?} observed={observed:?}"
            ));
        }
        Ok(())
    };
    exact_object("invariants", &storage["invariants"], &expected_invariants)?;
    let completed = storage["completed_seeds"]
        .as_u64()
        .ok_or_else(|| "storage-durability completed_seeds is absent".to_owned())?;
    if completed == 0 || storage["replayed_seeds"].as_u64() != Some(completed) {
        return Err(
            "storage-durability completed/replayed seed counts are zero or differ".to_owned(),
        );
    }
    for (invariant, checker_id) in [
        (
            "I15",
            zeppelin_embed_adversarial_oracle::storage_durability::I15_CHECKER_ID,
        ),
        (
            "I16",
            zeppelin_embed_adversarial_oracle::storage_durability::I16_CHECKER_ID,
        ),
        (
            "I17",
            zeppelin_embed_adversarial_oracle::storage_durability::I17_CHECKER_ID,
        ),
        (
            "I18",
            zeppelin_embed_adversarial_oracle::storage_durability::I18_CHECKER_ID,
        ),
        (
            "I19",
            zeppelin_embed_adversarial_oracle::storage_durability::I19_CHECKER_ID,
        ),
    ] {
        let record = &storage["invariants"][invariant];
        if record["checker_id"].as_str() != Some(checker_id) {
            return Err(format!("storage-durability {invariant} checker_id differs"));
        }
        let comparisons = record["comparisons"]
            .as_u64()
            .ok_or_else(|| format!("storage-durability {invariant} comparisons is absent"))?;
        if comparisons < completed
            || record["passes"].as_u64() != Some(comparisons)
            || record["plants"].as_u64() != Some(0)
        {
            return Err(format!(
                "storage-durability {invariant} comparison/pass/plant counts differ"
            ));
        }
    }
    let operations = [
        ("publication", "I15"),
        ("wal-prefix", "I16"),
        ("retry", "I17"),
        ("format-check", "I18"),
        ("orphan-cleanup", "I19"),
    ];
    exact_object(
        "operations",
        &storage["operations"],
        &operations.map(|(operation, _)| operation),
    )?;
    for (operation, invariant) in operations {
        let record = &storage["operations"][operation];
        if record["executions"].as_u64() != Some(completed)
            || record["qualifying_checks"].as_u64()
                != storage["invariants"][invariant]["comparisons"].as_u64()
        {
            return Err(format!(
                "storage-durability operation {operation} counts differ: completed={completed} record={record} invariant={}",
                storage["invariants"][invariant]
            ));
        }
    }
    let fault_keys = CampaignSpec::for_kind(CampaignKind::StorageDurability)
        .feature_faults
        .iter()
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    exact_object("faults", &storage["faults"], &fault_keys)?;
    for fault in fault_keys {
        let pairs = storage["faults"][fault]["same_seed_pairs"]
            .as_u64()
            .ok_or_else(|| format!("storage-durability fault {fault} pair count is absent"))?;
        if storage["faults"][fault]["production_receipts"].as_u64() != Some(pairs) {
            return Err(format!(
                "storage-durability fault {fault} receipt count differs"
            ));
        }
    }
    let format_cases = [
        "wal-header",
        "wal-record-body",
        "wal-record-checksum",
        "segment-region",
        "manifest-wrong-family",
        "segment-wrong-family",
        "segment-wrong-identity",
    ];
    let omission_cases = [
        "final-segment.list",
        "final-segment.delete",
        "segment-temporary.list",
        "segment-temporary.delete",
        "manifest-temporary.list",
        "manifest-temporary.delete",
    ];
    let receipt_sites = [
        "wal-open-header-validation",
        "wal-open-record-validation",
        "wal-open-record-checksum",
        "wal-commit-append-after-inner-success",
        "manifest-commit-before-rename",
        "manifest-commit-after-rename",
        "segment-read-region-checksum",
        "manifest-open-family-validation",
        "segment-open-family-validation",
        "segment-open-object-identity",
        "orphan-cleanup-list",
        "orphan-cleanup-delete",
    ];
    for (label, field, keys) in [
        ("format cases", "format_cases", format_cases.as_slice()),
        (
            "omission cases",
            "omission_cases",
            omission_cases.as_slice(),
        ),
        ("receipt sites", "receipt_sites", receipt_sites.as_slice()),
    ] {
        exact_object(label, &storage[field], keys)?;
        for key in keys {
            storage[field][*key]
                .as_u64()
                .ok_or_else(|| format!("storage-durability {label} {key} count is not u64"))?;
        }
    }
    let evidence = storage["evidence_digests"]
        .as_object()
        .ok_or_else(|| "storage-durability evidence_digests is not an object".to_owned())?;
    for field in [
        "program",
        "faults",
        "violations",
        "coverage",
        "checker",
        "control",
        "receipt",
        "mutation",
    ] {
        if evidence
            .get(field)
            .and_then(|value| value.as_str())
            .is_none()
        {
            return Err(format!(
                "storage-durability evidence digest missing: {field}"
            ));
        }
    }
    Ok(())
}

fn validate_metadata_control_evidence(
    control: &zeppelin_embed_bench::harness_json::Value,
) -> Result<(), String> {
    if control["normalized_schedule_digest"].as_u64().unwrap_or(0) == 0 {
        return Err("metadata control normalized schedule digest is zero or absent".to_owned());
    }
    let directory_relation = control["directory_relation"]
        .as_str()
        .ok_or_else(|| "metadata control directory relation is absent".to_owned())?;
    if !matches!(
        directory_relation,
        "unpaired-single-directory" | "distinct-byte-identical"
    ) {
        return Err(format!(
            "metadata control directory relation is unknown: {directory_relation}"
        ));
    }
    let outcome = control["outcome"]
        .as_str()
        .ok_or_else(|| "metadata control public outcome is absent".to_owned())?;
    if !matches!(
        outcome,
        "clean-only"
            | "clean-and-filtered-succeeded"
            | "clean-fault-and-retry-equivalent"
            | "clean-succeeded-fault-refused-retry-equivalent"
            | "clean-graph-fault-fallback-retry-equivalent"
    ) {
        return Err(format!(
            "metadata control public outcome is unknown: {outcome}"
        ));
    }
    let validate_directory = |label: &str,
                              directory: &zeppelin_embed_bench::harness_json::Value|
     -> Result<(), String> {
        if directory["digest"].as_u64().unwrap_or(0) == 0 {
            return Err(format!("metadata control {label} directory digest is zero"));
        }
        let files = directory["files"]
            .as_array()
            .ok_or_else(|| format!("metadata control {label} directory files are absent"))?;
        if files.is_empty() {
            return Err(format!(
                "metadata control {label} directory files are empty"
            ));
        }
        let mut previous = None::<&str>;
        for file in files {
            let path = file["relative_path"]
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| format!("metadata control {label} file path is absent"))?;
            if previous.is_some_and(|previous| previous >= path) {
                return Err(format!(
                    "metadata control {label} directory files are not strictly sorted"
                ));
            }
            previous = Some(path);
            if file["digest"].as_u64().unwrap_or(0) == 0 || file["byte_length"].as_u64().is_none() {
                return Err(format!(
                    "metadata control {label} file {path} facts are incomplete"
                ));
            }
        }
        Ok(())
    };
    validate_directory("clean", &control["clean_initial_directory"])?;
    if directory_relation == "distinct-byte-identical" {
        validate_directory("fault", &control["fault_initial_directory"])?;
        if control["clean_initial_directory"] != control["fault_initial_directory"] {
            return Err(
                "metadata control distinct clean/fault initial directories are not byte-identical"
                    .to_owned(),
            );
        }
    }
    let clean = &control["clean_results"];
    let fault = &control["fault_results"];
    let retry = &control["retry_results"];
    let expected = &control["independent_expected_results"];
    if !clean.is_array() || !fault.is_array() || !retry.is_array() || !expected.is_array() {
        return Err("metadata control public result ledgers are not arrays".to_owned());
    }
    if matches!(
        outcome,
        "clean-fault-and-retry-equivalent" | "clean-graph-fault-fallback-retry-equivalent"
    ) && !(clean == fault && clean == retry && clean == expected)
    {
        return Err(format!(
            "metadata control outcome {outcome} does not match clean/fault/retry results"
        ));
    }
    if outcome == "clean-succeeded-fault-refused-retry-equivalent"
        && (!(clean == retry && clean == expected)
            || fault.as_array().is_none_or(|results| !results.is_empty())
            || control["fault_error"].as_str().is_none_or(str::is_empty))
    {
        return Err(
            "metadata control fault-refused outcome does not match clean/retry/error facts"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_metadata_mutation_evidence(
    mutation: &zeppelin_embed_bench::harness_json::Value,
) -> Result<(), String> {
    if mutation["source"].as_str().is_none_or(str::is_empty)
        || mutation["region"]["code"].as_u64().is_none()
        || mutation["region"]["name"]
            .as_str()
            .is_none_or(str::is_empty)
        || mutation["absolute_offset"].as_u64().is_none()
        || mutation["before_hex"].as_str().is_none()
        || mutation["after_hex"].as_str().is_none()
    {
        return Err("metadata mutation primitive facts are incomplete".to_owned());
    }
    if mutation["post_mutation_artifact_digest"]
        .as_u64()
        .unwrap_or(0)
        == 0
    {
        return Err("metadata mutation post-mutation artifact digest is zero".to_owned());
    }
    let rewrites = mutation["checksum_rewrites"]
        .as_array()
        .ok_or_else(|| "metadata mutation checksum rewrite ledger is absent".to_owned())?;
    if rewrites.is_empty() {
        return Err("metadata mutation checksum rewrite ledger is empty".to_owned());
    }
    for (ordinal, rewrite) in rewrites.iter().enumerate() {
        if rewrite["field"]["kind"].as_str().is_none()
            || rewrite["absolute_offset"].as_u64().is_none()
            || rewrite["before"].as_u64().is_none()
            || rewrite["after"].as_u64().is_none()
        {
            return Err(format!(
                "metadata mutation checksum rewrite {ordinal} is incomplete"
            ));
        }
    }
    let mut cursor = usize::from(rewrites[0]["field"]["kind"] == "graph-internal");
    let target = rewrites.get(cursor).ok_or_else(|| {
        "metadata mutation checksum rewrite ledger omitted target region".to_owned()
    })?;
    if target["field"]["kind"] != "target-region" || target["field"]["region"] != mutation["region"]
    {
        return Err("metadata mutation checksum rewrite target region differs".to_owned());
    }
    cursor = cursor.saturating_add(1);
    let mut expected_chunk = 0_u64;
    while rewrites
        .get(cursor)
        .is_some_and(|rewrite| rewrite["field"]["kind"] == "target-region-chunk")
    {
        let rewrite = &rewrites[cursor];
        if rewrite["field"]["region"] != mutation["region"]
            || rewrite["field"]["chunk_index"].as_u64() != Some(expected_chunk)
        {
            return Err(format!(
                "metadata mutation target-region chunk ledger differs at chunk {expected_chunk}"
            ));
        }
        expected_chunk = expected_chunk
            .checked_add(1)
            .ok_or_else(|| "metadata mutation chunk index overflowed".to_owned())?;
        cursor = cursor.saturating_add(1);
    }
    if expected_chunk == 0 {
        return Err("metadata mutation checksum rewrite ledger omitted region chunks".to_owned());
    }
    for required in [
        "checksum-table-region",
        "segment-header",
        "segment-whole-file",
    ] {
        if rewrites
            .get(cursor)
            .is_none_or(|rewrite| rewrite["field"]["kind"] != required)
        {
            return Err(format!(
                "metadata mutation checksum rewrite ledger omitted or reordered {required}"
            ));
        }
        cursor = cursor.saturating_add(1);
    }
    if cursor != rewrites.len() {
        return Err("metadata mutation checksum rewrite ledger has extra entries".to_owned());
    }
    Ok(())
}

fn validate_metadata_oracle_attestation_shape(
    metadata: &zeppelin_embed_bench::harness_json::Value,
) -> Result<(), String> {
    let object_keys = |label: &str,
                       value: &zeppelin_embed_bench::harness_json::Value|
     -> Result<BTreeSet<String>, String> {
        value
            .as_object()
            .ok_or_else(|| format!("metadata oracle attestation {label} is not an object"))
            .map(|object| object.keys().cloned().collect())
    };
    let exact_keys = |label: &str,
                      value: &zeppelin_embed_bench::harness_json::Value,
                      required: &[&str]|
     -> Result<(), String> {
        let observed = object_keys(label, value)?;
        let expected = required
            .iter()
            .map(|key| (*key).to_owned())
            .collect::<BTreeSet<_>>();
        if observed != expected {
            return Err(format!(
                "metadata oracle attestation {label} keys mismatch: expected={expected:?} observed={observed:?}"
            ));
        }
        Ok(())
    };

    exact_keys(
        "root",
        metadata,
        &[
            "version",
            "oracle_contract",
            "oracle_source_digest",
            "repository",
            "required_invariants",
            "invariants",
            "operations",
            "i37_predicate_cases",
            "faults",
            "branches",
            "fallbacks",
            "evidence_digests",
            "completed_seeds",
            "replayed_seeds",
        ],
    )?;
    if metadata["version"].as_u64() != Some(1) {
        return Err("metadata oracle attestation version is not 1".to_owned());
    }
    if metadata["oracle_contract"].as_str()
        != Some(adversarial::artifacts::oracle_contract(
            CampaignKind::MetadataFilterPlanner,
        ))
    {
        return Err("metadata oracle attestation oracle_contract is stale".to_owned());
    }
    let expected_source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/metadata_filter_planner.rs"
    )]);
    if metadata["oracle_source_digest"].as_str() != Some(expected_source_digest.as_str()) {
        return Err("metadata oracle attestation oracle_source_digest is stale".to_owned());
    }
    exact_keys(
        "repository",
        &metadata["repository"],
        &["revision", "dirty_state"],
    )?;
    if metadata["repository"]["revision"].as_str()
        != Some(adversarial::artifacts::harness_git_revision())
    {
        return Err("metadata oracle attestation repository revision is stale".to_owned());
    }
    if metadata["repository"]["dirty_state"].as_str().is_none() {
        return Err("metadata oracle attestation omitted repository dirty_state".to_owned());
    }
    if metadata["required_invariants"]
        != zeppelin_embed_bench::harness_json::json!(["I36", "I37", "I38", "I39"])
    {
        return Err(
            "metadata oracle attestation required invariants are not exactly I36-I39".to_owned(),
        );
    }

    let checker_ids = [
        (
            "I36",
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I36_CHECKER_ID,
        ),
        (
            "I37",
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I37_CHECKER_ID,
        ),
        (
            "I38",
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I38_CHECKER_ID,
        ),
        (
            "I39",
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I39_CHECKER_ID,
        ),
    ];
    exact_keys(
        "invariants",
        &metadata["invariants"],
        &["I36", "I37", "I38", "I39"],
    )?;
    for (invariant, checker_id) in checker_ids {
        let ledger = &metadata["invariants"][invariant];
        exact_keys(
            &format!("invariants.{invariant}"),
            ledger,
            &["checker_id", "comparisons", "passes", "plants"],
        )?;
        if ledger["checker_id"].as_str() != Some(checker_id) {
            return Err(format!(
                "metadata oracle attestation {invariant} checker_id is stale"
            ));
        }
        for field in ["comparisons", "passes", "plants"] {
            if ledger[field].as_u64().is_none() {
                return Err(format!(
                    "metadata oracle attestation {invariant} {field} is not u64"
                ));
            }
        }
    }

    exact_keys(
        "operations",
        &metadata["operations"],
        &[
            "metadata_columns_roundtrip",
            "metadata_bitmap_algebra",
            "metadata_pruning_soundness",
            "metadata_execution_truth",
        ],
    )?;
    for operation in [
        "metadata_columns_roundtrip",
        "metadata_bitmap_algebra",
        "metadata_pruning_soundness",
        "metadata_execution_truth",
    ] {
        let ledger = &metadata["operations"][operation];
        exact_keys(
            &format!("operations.{operation}"),
            ledger,
            &["executions", "qualifying_checks"],
        )?;
        if ledger["executions"].as_u64().is_none() || ledger["qualifying_checks"].as_u64().is_none()
        {
            return Err(format!(
                "metadata oracle attestation operation {operation} has a non-u64 count"
            ));
        }
    }

    let expected_i37_cases = (0..adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT)
        .map(|case| adversarial::metadata_filter_planner::i37_predicate_case_key(case).to_owned())
        .collect::<BTreeSet<_>>();
    let observed_i37_cases = object_keys("i37_predicate_cases", &metadata["i37_predicate_cases"])?;
    if observed_i37_cases != expected_i37_cases {
        return Err(format!(
            "metadata oracle attestation I37 predicate-case keys mismatch: expected={expected_i37_cases:?} observed={observed_i37_cases:?}"
        ));
    }
    for case in expected_i37_cases {
        if metadata["i37_predicate_cases"][&case].as_u64().is_none() {
            return Err(format!(
                "metadata oracle attestation I37 predicate case {case} is not u64"
            ));
        }
    }

    let fault_keys = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
        .feature_faults
        .iter()
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    exact_keys("faults", &metadata["faults"], &fault_keys)?;
    for fault in fault_keys {
        let ledger = &metadata["faults"][fault];
        exact_keys(
            &format!("faults.{fault}"),
            ledger,
            &["same_seed_pairs", "production_receipts"],
        )?;
        if ledger["same_seed_pairs"].as_u64().is_none()
            || ledger["production_receipts"].as_u64().is_none()
        {
            return Err(format!(
                "metadata oracle attestation fault {fault} has a non-u64 count"
            ));
        }
        let same_seed_pairs = ledger["same_seed_pairs"]
            .as_u64()
            .expect("validated metadata same-seed count");
        let production_receipts = ledger["production_receipts"]
            .as_u64()
            .expect("validated metadata receipt count");
        let required_cardinality = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
            .feature_faults
            .iter()
            .find(|candidate| candidate.key() == fault)
            .map(|candidate| candidate.required_receipt_cardinality() as u64)
            .expect("metadata attestation fault came from the campaign catalog");
        let expected_receipts = same_seed_pairs
            .checked_mul(required_cardinality)
            .ok_or_else(|| {
                format!("metadata oracle attestation fault {fault} receipt count overflow")
            })?;
        if production_receipts != expected_receipts {
            return Err(format!(
                "metadata oracle attestation fault {fault} requires {required_cardinality} production receipts per same-seed pair: pairs={same_seed_pairs} receipts={production_receipts}"
            ));
        }
    }
    exact_keys(
        "branches",
        &metadata["branches"],
        &[
            "pruned",
            "exact-allow-list",
            "masked-scan",
            "filtered-graph",
            "graph-exact-fallback",
        ],
    )?;
    exact_keys(
        "fallbacks",
        &metadata["fallbacks"],
        &["none", "visited-budget", "candidate-shortfall"],
    )?;
    for (label, keys) in [
        (
            "branches",
            &[
                "pruned",
                "exact-allow-list",
                "masked-scan",
                "filtered-graph",
                "graph-exact-fallback",
            ][..],
        ),
        (
            "fallbacks",
            &["none", "visited-budget", "candidate-shortfall"][..],
        ),
    ] {
        for key in keys {
            if metadata[label][key].as_u64().is_none() {
                return Err(format!(
                    "metadata oracle attestation {label}.{key} is not u64"
                ));
            }
        }
    }
    if metadata["evidence_digests"].as_object().is_none() {
        return Err("metadata oracle attestation evidence_digests is not an object".to_owned());
    }
    let completed_seeds = metadata["completed_seeds"].as_u64();
    let replayed_seeds = metadata["replayed_seeds"].as_u64();
    if completed_seeds.is_none() || replayed_seeds.is_none() {
        return Err("metadata oracle attestation seed counts are not u64".to_owned());
    }
    let completed_seeds = completed_seeds.expect("validated metadata completed seed count");
    let replayed_seeds = replayed_seeds.expect("validated metadata replayed seed count");
    if completed_seeds == 0 || replayed_seeds != completed_seeds {
        return Err(format!(
            "metadata oracle attestation replay is incomplete: completed_seeds={completed_seeds} replayed_seeds={replayed_seeds}"
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "metadata qualification binds each independently merged evidence ledger"
)]
fn validate_metadata_observed_ledgers(
    metadata: &zeppelin_embed_bench::harness_json::Value,
    comparisons: &BTreeMap<String, u64>,
    passes: &BTreeMap<String, u64>,
    fault_pairs: &BTreeMap<String, u64>,
    production_receipts: &BTreeMap<String, u64>,
    branches: &BTreeMap<String, u64>,
    fallbacks: &BTreeMap<String, u64>,
    coverage_counts: &BTreeMap<String, u64>,
    oracle_i37_case_counts: &BTreeMap<String, u64>,
    i37_fault_case_counts: &BTreeMap<String, u64>,
    episodes: u64,
) -> Result<(), String> {
    for (invariant, operation) in [
        ("I36", "metadata_columns_roundtrip"),
        ("I37", "metadata_bitmap_algebra"),
        ("I38", "metadata_pruning_soundness"),
        ("I39", "metadata_execution_truth"),
    ] {
        let observed = comparisons.get(invariant).copied().unwrap_or(0);
        let observed_passes = passes.get(invariant).copied().unwrap_or(0);
        let ledger = &metadata["invariants"][invariant];
        if ledger["comparisons"].as_u64() != Some(observed)
            || ledger["passes"].as_u64() != Some(observed_passes)
            || ledger["plants"].as_u64() != Some(0)
        {
            return Err(format!(
                "metadata oracle attestation {invariant} ledger disagrees with merged oracle comparisons"
            ));
        }
        let operation_ledger = &metadata["operations"][operation];
        if operation_ledger["executions"].as_u64() != Some(episodes)
            || operation_ledger["qualifying_checks"].as_u64() != Some(observed_passes)
        {
            return Err(format!(
                "metadata oracle attestation operation {operation} disagrees with merged comparisons"
            ));
        }
    }

    let mut reported_pairs = BTreeMap::new();
    let mut reported_receipts = BTreeMap::new();
    for fault in CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner).feature_faults {
        let key = fault.key().to_owned();
        reported_pairs.insert(
            key.clone(),
            metadata["faults"][fault.key()]["same_seed_pairs"]
                .as_u64()
                .ok_or_else(|| format!("metadata fault {} pair count is not u64", fault.key()))?,
        );
        reported_receipts.insert(
            key,
            metadata["faults"][fault.key()]["production_receipts"]
                .as_u64()
                .ok_or_else(|| {
                    format!("metadata fault {} receipt count is not u64", fault.key())
                })?,
        );
    }
    if &reported_pairs != fault_pairs {
        return Err(format!(
            "metadata same-seed fault ledger mismatch reported={reported_pairs:?} observed={fault_pairs:?}"
        ));
    }
    if &reported_receipts != production_receipts {
        let differing_fault = reported_receipts
            .keys()
            .chain(production_receipts.keys())
            .find(|key| reported_receipts.get(*key) != production_receipts.get(*key))
            .map(String::as_str)
            .unwrap_or("unknown");
        return Err(format!(
            "metadata production receipt ledger mismatch fault={differing_fault} reported={reported_receipts:?} observed={production_receipts:?}"
        ));
    }

    let mut attested_i37_total = 0_u64;
    for case_index in 0..adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT {
        let case = adversarial::metadata_filter_planner::i37_predicate_case_key(case_index);
        let expected = episodes
            .checked_add(i37_fault_case_counts.get(case).copied().unwrap_or(0))
            .ok_or_else(|| format!("metadata I37 predicate case {case} count overflowed"))?;
        let attested = metadata["i37_predicate_cases"][case]
            .as_u64()
            .ok_or_else(|| format!("metadata I37 predicate case {case} is not u64"))?;
        let coverage_key = format!("metadata.i37.matrix.{case}");
        let observed = coverage_counts.get(&coverage_key).copied().unwrap_or(0);
        let oracle_rows = oracle_i37_case_counts.get(case).copied().unwrap_or(0);
        if attested != expected || observed != expected || oracle_rows != expected {
            return Err(format!(
                "metadata I37 predicate case {case} count mismatch: expected={expected} attested={attested} coverage={observed} oracle_rows={oracle_rows}"
            ));
        }
        attested_i37_total = attested_i37_total
            .checked_add(attested)
            .ok_or_else(|| "metadata I37 predicate-case total overflowed".to_owned())?;
    }
    let observed_i37 = comparisons.get("I37").copied().unwrap_or(0);
    if attested_i37_total != observed_i37 {
        return Err(format!(
            "metadata I37 predicate-case total disagrees with merged comparisons: cases={attested_i37_total} comparisons={observed_i37}"
        ));
    }
    let reported_branches = metadata["branches"]
        .as_object()
        .expect("metadata shape validated branches")
        .iter()
        .map(|(key, value)| {
            value
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("metadata branch {key} count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if &reported_branches != branches {
        return Err(format!(
            "metadata branch ledger mismatch reported={reported_branches:?} observed={branches:?}"
        ));
    }
    let reported_fallbacks = metadata["fallbacks"]
        .as_object()
        .expect("metadata shape validated fallbacks")
        .iter()
        .map(|(key, value)| {
            value
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("metadata fallback {key} count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if &reported_fallbacks != fallbacks {
        return Err(format!(
            "metadata fallback ledger mismatch reported={reported_fallbacks:?} observed={fallbacks:?}"
        ));
    }
    if metadata["completed_seeds"].as_u64() != Some(episodes) {
        return Err(format!(
            "metadata completed seed ledger mismatch reported={} observed={episodes}",
            metadata["completed_seeds"]
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct StorageMergedLedgers {
    fault_pairs: BTreeMap<String, u64>,
    same_seed_controls: BTreeMap<String, u64>,
    production_receipts: BTreeMap<String, u64>,
    receipt_sites: BTreeMap<String, u64>,
    retained_artifacts: u64,
}

fn storage_receipt_site_ledger_key(site: &str) -> Option<&'static str> {
    match site {
        "WalOpen.HeaderValidation" => Some("wal-open-header-validation"),
        "WalOpen.RecordValidation" => Some("wal-open-record-validation"),
        "WalOpen.RecordChecksum" => Some("wal-open-record-checksum"),
        "WalCommit.AppendAfterInnerSuccess" => Some("wal-commit-append-after-inner-success"),
        "ManifestCommit.BeforeRename" => Some("manifest-commit-before-rename"),
        "ManifestCommit.AfterRename" => Some("manifest-commit-after-rename"),
        "SegmentRead.RegionChecksum" => Some("segment-read-region-checksum"),
        "ManifestOpen.FamilyValidation" => Some("manifest-open-family-validation"),
        "SegmentOpen.FamilyValidation" => Some("segment-open-family-validation"),
        "SegmentOpen.ObjectIdentity" => Some("segment-open-object-identity"),
        "OrphanCleanup.List" => Some("orphan-cleanup-list"),
        "OrphanCleanup.Delete" => Some("orphan-cleanup-delete"),
        _ => None,
    }
}

fn storage_fault_allows_receipt_site(fault: &str, site: &str) -> bool {
    match fault {
        "torn-wal-header" => site == "WalOpen.HeaderValidation",
        "torn-wal-body" => site == "WalOpen.RecordValidation",
        "torn-wal-checksum" => site == "WalOpen.RecordChecksum",
        "post-commit-error" => site == "WalCommit.AppendAfterInnerSuccess",
        "manifest-pre-rename-crash" => site == "ManifestCommit.BeforeRename",
        "manifest-post-rename-crash" => site == "ManifestCommit.AfterRename",
        "corrupt-segment-region" => site == "SegmentRead.RegionChecksum",
        "wrong-manifest-object" => site == "ManifestOpen.FamilyValidation",
        "wrong-segment-object" => matches!(
            site,
            "SegmentOpen.FamilyValidation" | "SegmentOpen.ObjectIdentity"
        ),
        "list-delete-omission" => {
            matches!(site, "OrphanCleanup.List" | "OrphanCleanup.Delete")
        }
        _ => false,
    }
}

fn decode_storage_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("storage retained hex has odd length".to_owned());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            let high = digit(pair[0])
                .ok_or_else(|| "storage retained hex contains a non-hex digit".to_owned())?;
            let low = digit(pair[1])
                .ok_or_else(|| "storage retained hex contains a non-hex digit".to_owned())?;
            Ok((high << 4) | low)
        })
        .collect()
}

/// Retained artifact rows minus their `role` label, so a clean-pre-operation
/// capture and its fault-pre-operation twin compare on bytes and facts alone.
fn artifact_payloads(
    artifacts: &zeppelin_embed_bench::harness_json::Value,
) -> Option<
    Vec<(
        zeppelin_embed_bench::harness_json::Value,
        zeppelin_embed_bench::harness_json::Value,
    )>,
> {
    artifacts
        .as_array()?
        .iter()
        .map(|artifact| (artifact["bytes_hex"].clone(), artifact["fact"].clone()))
        .collect::<Vec<_>>()
        .into()
}

fn read_storage_merged_ledgers(root: &Path) -> Result<StorageMergedLedgers, String> {
    let spec = CampaignSpec::for_kind(CampaignKind::StorageDurability);
    let mut fault_pairs = spec
        .feature_faults
        .iter()
        .map(|fault| (fault.key().to_owned(), 0_u64))
        .collect::<BTreeMap<_, _>>();
    let mut same_seed_controls = fault_pairs.clone();
    let mut production_receipts = fault_pairs.clone();
    let mut receipt_sites = [
        "wal-open-header-validation",
        "wal-open-record-validation",
        "wal-open-record-checksum",
        "wal-commit-append-after-inner-success",
        "manifest-commit-before-rename",
        "manifest-commit-after-rename",
        "segment-read-region-checksum",
        "manifest-open-family-validation",
        "segment-open-family-validation",
        "segment-open-object-identity",
        "orphan-cleanup-list",
        "orphan-cleanup-delete",
    ]
    .into_iter()
    .map(|site| (site.to_owned(), 0_u64))
    .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeMap::<(u64, String, String), Vec<String>>::new();

    let fault_bytes = std::fs::read(root.join("merged-faults.jsonl"))
        .map_err(|error| format!("read merged storage faults: {error}"))?;
    for line in fault_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged storage fault row: {error}"))?;
        let record = &envelope["record"];
        if record["type"] != "feature"
            || record["campaign"] != CampaignKind::StorageDurability.key()
        {
            continue;
        }
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged storage fault omitted envelope seed".to_owned())?;
        let profile = envelope["profile"]
            .as_str()
            .ok_or_else(|| "merged storage fault omitted profile".to_owned())?
            .to_owned();
        let key = record["key"]
            .as_str()
            .ok_or_else(|| "merged storage feature fault omitted key".to_owned())?;
        let fault = spec
            .feature_faults
            .iter()
            .find(|fault| fault.key() == key)
            .ok_or_else(|| format!("merged storage fault key is unknown: {key}"))?;
        if record["op"].as_u64().is_none()
            || record["fired"].as_bool() != Some(true)
            || record["fire_count"].as_u64() != Some(fault.required_receipt_cardinality() as u64)
        {
            return Err(format!(
                "merged storage fault {key} did not fire with exact receipt cardinality"
            ));
        }
        let operation = fault.operation().key().to_owned();
        selected
            .entry((seed, profile, operation))
            .or_default()
            .push(key.to_owned());
        *fault_pairs
            .get_mut(key)
            .expect("storage fault map came from the same catalog") += 1;
    }

    let mut control_offsets = BTreeMap::<(u64, String, String), usize>::new();
    let control_bytes = std::fs::read(root.join("merged-controls.jsonl"))
        .map_err(|error| format!("read merged storage controls: {error}"))?;
    for line in control_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged storage control row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::StorageDurability.key() {
            continue;
        }
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged storage control omitted envelope seed".to_owned())?;
        let profile = envelope["profile"]
            .as_str()
            .ok_or_else(|| "merged storage control omitted profile".to_owned())?
            .to_owned();
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "merged storage control omitted operation".to_owned())?
            .to_owned();
        let control = &record["control"];
        if record["seed"].as_u64() != Some(seed)
            || control["seed"].as_u64() != Some(seed)
            || control["namespace"].as_str().is_none_or(str::is_empty)
            || control["operation_fixture_id"]
                .as_str()
                .is_none_or(str::is_empty)
            || control["clean_fault_pair_id"]
                .as_str()
                .is_none_or(str::is_empty)
            || !control["pre_clean_inventory"].is_array()
            || control["pre_clean_inventory"] != control["pre_fault_inventory"]
            || control["pre_clean_digest"].as_str().is_none()
            || control["pre_clean_digest"] != control["pre_fault_digest"]
            || artifact_payloads(&control["pre_clean_artifacts"])
                != artifact_payloads(&control["pre_fault_artifacts"])
        {
            return Err(format!(
                "merged storage control is not a byte-identical same-seed pair: seed={seed} operation={operation}"
            ));
        }
        let selected_key = (seed, profile, operation);
        if let Some(faults) = selected.get(&selected_key) {
            let offset = control_offsets.entry(selected_key).or_default();
            let fault = faults.get(*offset).ok_or_else(|| {
                "merged storage control has no matching selected fault".to_owned()
            })?;
            *offset = offset.saturating_add(1);
            *same_seed_controls
                .get_mut(fault)
                .expect("selected storage fault came from the catalog") += 1;
        }
    }

    let mut receipt_offsets = BTreeMap::<(u64, String, String), usize>::new();
    let receipt_bytes = std::fs::read(root.join("merged-receipts.jsonl"))
        .map_err(|error| format!("read merged storage receipts: {error}"))?;
    for line in receipt_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged storage receipt row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::StorageDurability.key() {
            continue;
        }
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged storage receipt omitted envelope seed".to_owned())?;
        let profile = envelope["profile"]
            .as_str()
            .ok_or_else(|| "merged storage receipt omitted profile".to_owned())?
            .to_owned();
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "merged storage receipt omitted operation".to_owned())?
            .to_owned();
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "merged storage receipt omitted fault".to_owned())?;
        let selected_key = (seed, profile, operation);
        let offset = receipt_offsets.entry(selected_key.clone()).or_default();
        let selected_fault = selected
            .get(&selected_key)
            .and_then(|faults| faults.get(*offset))
            .map(String::as_str);
        *offset = offset.saturating_add(1);
        if selected_fault != Some(fault) {
            return Err(format!(
                "merged storage receipt does not match its selected same-seed fault: seed={seed} fault={fault}"
            ));
        }
        let site = record["site"]
            .as_str()
            .ok_or_else(|| "merged storage receipt omitted production site".to_owned())?;
        let site_key = storage_receipt_site_ledger_key(site)
            .ok_or_else(|| format!("merged storage receipt used unknown site {site}"))?;
        let digest = record["receipt_digest"]
            .as_str()
            .ok_or_else(|| "merged storage receipt omitted receipt digest".to_owned())?;
        if record["cardinality"].as_u64() != Some(1)
            || !record["plan"].is_object()
            || !record["observed"].is_object()
            || !storage_fault_allows_receipt_site(fault, site)
            || digest.len() != "fnv1a64:".len() + 16
            || !digest.starts_with("fnv1a64:")
            || !digest["fnv1a64:".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "merged storage receipt failed typed plan/effect/site/cardinality attestation: fault={fault} site={site}"
            ));
        }
        let expected_digest = adversarial::runner::storage_receipt_evidence_digest(record)?;
        if digest != expected_digest {
            return Err(format!(
                "merged storage receipt digest mismatch: fault={fault} site={site} expected={expected_digest} observed={digest}"
            ));
        }
        *production_receipts
            .get_mut(fault)
            .ok_or_else(|| format!("merged storage receipt has unknown fault {fault}"))? += 1;
        *receipt_sites
            .get_mut(site_key)
            .expect("storage receipt site came from the closed map") += 1;
    }

    if fault_pairs != same_seed_controls {
        return Err(format!(
            "storage same-seed control ledger mismatch selected={fault_pairs:?} controls={same_seed_controls:?}"
        ));
    }
    if fault_pairs != production_receipts {
        return Err(format!(
            "storage production receipt ledger mismatch selected={fault_pairs:?} receipts={production_receipts:?}"
        ));
    }

    let artifact_bytes = std::fs::read(root.join("merged-family-artifact-index.jsonl.jsonl"))
        .map_err(|error| format!("read merged storage artifact index: {error}"))?;
    let mut retained_artifacts = 0_u64;
    for line in artifact_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged storage artifact row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::StorageDurability.key() {
            continue;
        }
        let role = record["role"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "merged storage artifact omitted role".to_owned())?;
        let path = record["path"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "merged storage artifact omitted path".to_owned())?;
        let length = record["length"]
            .as_u64()
            .ok_or_else(|| "merged storage artifact omitted length".to_owned())?;
        let retained = decode_storage_hex(
            record["bytes_hex"]
                .as_str()
                .ok_or_else(|| "merged storage artifact omitted retained bytes".to_owned())?,
        )?;
        let digest = decode_storage_hex(
            record["digest"]
                .as_str()
                .ok_or_else(|| "merged storage artifact omitted digest".to_owned())?,
        )?;
        let expected = adversarial::storage_durability::digest32(0x4649_4c45_4641_4354, &retained);
        if record["mutation"].is_object()
            || retained.len() as u64 != length
            || digest.as_slice() != expected
        {
            return Err(format!(
                "merged storage artifact does not match its retained bytes: role={role} path={path}"
            ));
        }
        retained_artifacts = retained_artifacts.saturating_add(1);
    }
    if retained_artifacts == 0 {
        return Err("merged storage artifact index retained no artifacts".to_owned());
    }

    Ok(StorageMergedLedgers {
        fault_pairs,
        same_seed_controls,
        production_receipts,
        receipt_sites,
        retained_artifacts,
    })
}

fn validate_storage_observed_ledgers(
    storage: &zeppelin_embed_bench::harness_json::Value,
    comparisons: &BTreeMap<String, u64>,
    ledgers: &StorageMergedLedgers,
    episodes: u64,
) -> Result<(), String> {
    let reported_comparisons = storage["invariants"]
        .as_object()
        .ok_or_else(|| "storage invariant attestation is not an object".to_owned())?
        .iter()
        .map(|(key, value)| {
            value["comparisons"]
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("storage invariant {key} comparison count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if &reported_comparisons != comparisons {
        return Err(format!(
            "storage comparison ledger mismatch reported={reported_comparisons:?} observed={comparisons:?}"
        ));
    }

    let reported_pairs = storage["faults"]
        .as_object()
        .ok_or_else(|| "storage fault attestation is not an object".to_owned())?
        .iter()
        .map(|(key, value)| {
            value["same_seed_pairs"]
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("storage fault {key} same-seed count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if reported_pairs != ledgers.fault_pairs || reported_pairs != ledgers.same_seed_controls {
        return Err(format!(
            "storage same-seed pair ledger mismatch reported={reported_pairs:?} selected={:?} controls={:?}",
            ledgers.fault_pairs, ledgers.same_seed_controls
        ));
    }

    let reported_receipts = storage["faults"]
        .as_object()
        .expect("storage fault object was validated above")
        .iter()
        .map(|(key, value)| {
            value["production_receipts"]
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("storage fault {key} receipt count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if reported_receipts != ledgers.production_receipts {
        return Err(format!(
            "storage production receipt ledger mismatch reported={reported_receipts:?} observed={:?}",
            ledgers.production_receipts
        ));
    }

    let reported_sites = storage["receipt_sites"]
        .as_object()
        .ok_or_else(|| "storage receipt-site attestation is not an object".to_owned())?
        .iter()
        .map(|(key, value)| {
            value
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("storage receipt site {key} count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if reported_sites != ledgers.receipt_sites {
        return Err(format!(
            "storage receipt-site ledger mismatch reported={reported_sites:?} observed={:?}",
            ledgers.receipt_sites
        ));
    }
    if ledgers.retained_artifacts == 0 {
        return Err("storage merged evidence retained no raw artifacts".to_owned());
    }
    if storage["completed_seeds"].as_u64() != Some(episodes) {
        return Err(format!(
            "storage completed seed ledger mismatch reported={} observed={episodes}",
            storage["completed_seeds"]
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct IngestRetentionMergedLedgers {
    operations: BTreeMap<String, u64>,
    fault_pairs: BTreeMap<String, u64>,
    same_seed_controls: BTreeMap<String, u64>,
    production_receipts: BTreeMap<String, u64>,
    receipt_sites: BTreeMap<String, u64>,
    retained_fixtures: u64,
}

fn ingest_retention_fault_contract(fault: &str) -> Option<(&'static str, &'static str)> {
    match fault {
        "post-ack-retry" => Some(("batch-commit", "ingest.replay.no-wal-append")),
        "partial-batch-append" => Some(("batch-commit", "ingest.commit-many.append-error")),
        "seal-cancellation" => Some(("seal", "seal.after-segment-write.before-manifest-commit")),
        "retention-clock-boundary" => Some(("retention", "retention.policy-evaluated")),
        "purge-unlink-error" => Some(("purge", "purge.old-segment-unlink.error")),
        "purge-crash-boundary" => Some(("purge", "purge.after-durable-intent.before-rewrite")),
        _ => None,
    }
}

fn ingest_retention_fixture_seed(
    fixture: &adversarial::ingest_retention::RetainedIngestFixtureV1,
) -> u64 {
    match fixture {
        adversarial::ingest_retention::RetainedIngestFixtureV1::I20(value) => value.seed,
        adversarial::ingest_retention::RetainedIngestFixtureV1::I21(value) => value.seed,
        adversarial::ingest_retention::RetainedIngestFixtureV1::I22(value) => value.seed,
        adversarial::ingest_retention::RetainedIngestFixtureV1::I23(value) => value.seed,
    }
}

fn ingest_retention_fnv1a64(bytes: &[u8]) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state
}

fn read_ingest_retention_merged_ledgers(
    root: &Path,
) -> Result<IngestRetentionMergedLedgers, String> {
    let spec = CampaignSpec::for_kind(CampaignKind::IngestRetention);
    let mut fault_pairs = spec
        .feature_faults
        .iter()
        .map(|fault| (fault.key().to_owned(), 0_u64))
        .collect::<BTreeMap<_, _>>();
    let mut same_seed_controls = fault_pairs.clone();
    let mut production_receipts = fault_pairs.clone();
    let mut receipt_sites = [
        "ingest.replay.no-wal-append",
        "ingest.commit-many.append-error",
        "seal.after-segment-write.before-manifest-commit",
        "retention.policy-evaluated",
        "purge.old-segment-unlink.error",
        "purge.after-durable-intent.before-rewrite",
    ]
    .into_iter()
    .map(|site| (site.to_owned(), 0_u64))
    .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeMap::<(u64, String, String), (String, u64)>::new();

    let fault_bytes = std::fs::read(root.join("merged-faults.jsonl"))
        .map_err(|error| format!("read merged ingest-retention faults: {error}"))?;
    for line in fault_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged ingest-retention fault row: {error}"))?;
        let record = &envelope["record"];
        if record["type"] != "feature" || record["campaign"] != CampaignKind::IngestRetention.key()
        {
            continue;
        }
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged ingest-retention fault omitted envelope seed".to_owned())?;
        let profile = envelope["profile"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention fault omitted profile".to_owned())?
            .to_owned();
        let fault = record["key"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention fault omitted key".to_owned())?;
        let (operation, _) = ingest_retention_fault_contract(fault)
            .ok_or_else(|| format!("merged ingest-retention fault key is unknown: {fault}"))?;
        let op_index = record["op"]
            .as_u64()
            .ok_or_else(|| format!("merged ingest-retention fault {fault} omitted op index"))?;
        if record["fired"].as_bool() != Some(true) || record["fire_count"].as_u64() != Some(1) {
            return Err(format!(
                "merged ingest-retention fault {fault} did not fire exactly once"
            ));
        }
        let key = (seed, profile, fault.to_owned());
        if selected
            .insert(key, (operation.to_owned(), op_index))
            .is_some()
        {
            return Err(format!(
                "merged ingest-retention fault {fault} duplicated its same-seed pair"
            ));
        }
        *fault_pairs
            .get_mut(fault)
            .expect("ingest-retention fault map came from the closed contract") += 1;
    }

    let control_bytes = std::fs::read(root.join("merged-controls.jsonl"))
        .map_err(|error| format!("read merged ingest-retention controls: {error}"))?;
    let mut seen_controls = BTreeSet::new();
    for line in control_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged ingest-retention control row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::IngestRetention.key() {
            continue;
        }
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged ingest-retention control omitted envelope seed".to_owned())?;
        let profile = envelope["profile"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention control omitted profile".to_owned())?
            .to_owned();
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention control omitted fault".to_owned())?;
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention control omitted operation".to_owned())?;
        let key = (seed, profile, fault.to_owned());
        let Some((selected_operation, _)) = selected.get(&key) else {
            return Err(format!(
                "merged ingest-retention control has no selected fault pair: seed={seed} fault={fault}"
            ));
        };
        let clean_digest = record["clean_initial_digest"].as_str();
        let fault_digest = record["fault_initial_digest"].as_str();
        if record["seed"].as_u64() != Some(seed)
            || operation != selected_operation
            || clean_digest.is_none_or(str::is_empty)
            || clean_digest != fault_digest
            || record["isolated_directories"].as_bool() != Some(true)
            || record["passed"].as_bool() != Some(true)
            || record["clean_final_count"].as_u64().is_none()
            || record["clean_final_count"] != record["fault_final_count"]
        {
            return Err(format!(
                "merged ingest-retention control is not an exact byte-identical same-seed pair: seed={seed} fault={fault}"
            ));
        }
        if !seen_controls.insert(key) {
            return Err(format!(
                "merged ingest-retention control duplicated seed={seed} fault={fault}"
            ));
        }
        *same_seed_controls.get_mut(fault).ok_or_else(|| {
            format!("merged ingest-retention control has unknown fault {fault}")
        })? += 1;
    }

    let receipt_bytes = std::fs::read(root.join("merged-receipts.jsonl"))
        .map_err(|error| format!("read merged ingest-retention receipts: {error}"))?;
    let mut seen_receipts = BTreeSet::new();
    for line in receipt_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged ingest-retention receipt row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::IngestRetention.key() {
            continue;
        }
        let claimed_checksum = record["receipt_checksum"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention receipt checksum is absent".to_owned())?;
        let observed_checksum = adversarial::runner::ingest_retention_receipt_checksum(record)?;
        if claimed_checksum != observed_checksum {
            return Err(format!(
                "merged ingest-retention receipt checksum differs expected={claimed_checksum} observed={observed_checksum}"
            ));
        }
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged ingest-retention receipt omitted envelope seed".to_owned())?;
        let profile = envelope["profile"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention receipt omitted profile".to_owned())?
            .to_owned();
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "merged ingest-retention receipt omitted fault".to_owned())?;
        let (expected_operation, expected_site) = ingest_retention_fault_contract(fault)
            .ok_or_else(|| format!("merged ingest-retention receipt has unknown fault {fault}"))?;
        let key = (seed, profile, fault.to_owned());
        let Some((selected_operation, op_index)) = selected.get(&key) else {
            return Err(format!(
                "merged ingest-retention receipt has no selected fault pair: seed={seed} fault={fault}"
            ));
        };
        if selected_operation != expected_operation
            || record["operation"].as_str() != Some(expected_operation)
            || record["site"].as_str() != Some(expected_site)
            || record["cardinality"].as_u64() != Some(1)
            || record["invocation_id"].as_u64() != Some(*op_index)
            || record["effect"]["kind"].as_str() != Some(fault)
        {
            return Err(format!(
                "merged ingest-retention receipt failed its typed operation/site/effect/cardinality contract: seed={seed} fault={fault}"
            ));
        }
        if !seen_receipts.insert(key) {
            return Err(format!(
                "merged ingest-retention receipt duplicated seed={seed} fault={fault}"
            ));
        }
        *production_receipts
            .get_mut(fault)
            .expect("ingest-retention receipt fault came from the closed contract") += 1;
        *receipt_sites
            .get_mut(expected_site)
            .expect("ingest-retention receipt site came from the closed contract") += 1;
    }

    if fault_pairs != same_seed_controls {
        return Err(format!(
            "ingest-retention same-seed control ledger mismatch selected={fault_pairs:?} controls={same_seed_controls:?}"
        ));
    }
    if fault_pairs != production_receipts {
        return Err(format!(
            "ingest-retention production receipt ledger mismatch selected={fault_pairs:?} receipts={production_receipts:?}"
        ));
    }

    let fixture_path = root.join("merged-family-fixture.json.jsonl");
    let fixture_bytes = std::fs::read(&fixture_path).map_err(|error| {
        format!(
            "read merged ingest-retention fixture {}: {error}",
            fixture_path.display()
        )
    })?;
    let mut retained_fixtures = 0_u64;
    for line in fixture_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line).map_err(|error| {
                format!("parse merged ingest-retention fixture envelope: {error}")
            })?;
        let seed = envelope["seed"]
            .as_u64()
            .ok_or_else(|| "merged ingest-retention fixture omitted envelope seed".to_owned())?;
        let document = &envelope["record"];
        if document["campaign"] != CampaignKind::IngestRetention.key() {
            return Err("merged ingest-retention fixture used the wrong campaign".to_owned());
        }
        let operations = document["operations"]
            .as_array()
            .ok_or_else(|| "merged ingest-retention fixture omitted operations".to_owned())?;
        for record in operations {
            if record["campaign"] != CampaignKind::IngestRetention.key()
                || record["schema"] != adversarial::ingest_retention::INGEST_RETAINED_FIXTURE_SCHEMA
            {
                return Err(
                    "merged ingest-retention fixture used the wrong schema/campaign".to_owned(),
                );
            }
            let retained_bytes =
                decode_storage_hex(record["retained_fixture_hex"].as_str().ok_or_else(|| {
                    "merged ingest-retention fixture omitted retained bytes".to_owned()
                })?)?;
            let expected_digest = format!("{:016x}", ingest_retention_fnv1a64(&retained_bytes));
            if record["retained_fixture_bytes"].as_u64() != u64::try_from(retained_bytes.len()).ok()
                || record["retained_fixture_digest"].as_str() != Some(expected_digest.as_str())
            {
                return Err(
                    "merged ingest-retention fixture length/digest differs from retained bytes"
                        .to_owned(),
                );
            }
            let retained = adversarial::ingest_retention::decode_ingest_fixture(&retained_bytes)
                .map_err(|error| format!("decode merged ingest-retention fixture: {error}"))?;
            let fault = retained.fault.map(|fault| fault.key());
            if ingest_retention_fixture_seed(&retained.fixture) != seed
                || record["seed"].as_u64() != Some(seed)
                || record["operation"].as_str() != Some(retained.operation.key())
                || record["fault"].as_str() != fault
                    && !(fault.is_none() && record["fault"].is_null())
                || record["invocation_id"].as_u64() != Some(retained.invocation_id)
            {
                return Err(
                    "merged ingest-retention fixture identity differs from decoded bytes"
                        .to_owned(),
                );
            }
            retained_fixtures = retained_fixtures.saturating_add(1);
        }
    }

    let coverage = read_merged_coverage_counts(root)?;
    let operations = ["batch-commit", "seal", "retention", "purge"]
        .into_iter()
        .map(|operation| {
            (
                operation.to_owned(),
                coverage
                    .get(&format!("campaign.op.ingest-retention.{operation}"))
                    .copied()
                    .unwrap_or(0),
            )
        })
        .collect();

    Ok(IngestRetentionMergedLedgers {
        operations,
        fault_pairs,
        same_seed_controls,
        production_receipts,
        receipt_sites,
        retained_fixtures,
    })
}

fn validate_ingest_retention_observed_ledgers(
    ingest: &zeppelin_embed_bench::harness_json::Value,
    comparisons: &BTreeMap<String, u64>,
    passes: &BTreeMap<String, u64>,
    ledgers: &IngestRetentionMergedLedgers,
    episodes: u64,
) -> Result<(), String> {
    let reported = |field: &str| {
        ingest[field]
            .as_object()
            .ok_or_else(|| format!("ingest-retention {field} attestation is not an object"))
    };
    let reported_counts = |field: &str, count_field: &str| {
        reported(field)?
            .iter()
            .map(|(key, value)| {
                value[count_field]
                    .as_u64()
                    .map(|count| (key.clone(), count))
                    .ok_or_else(|| {
                        format!("ingest-retention {field} {key} {count_field} is not u64")
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
    };

    let reported_comparisons = reported_counts("invariants", "comparisons")?;
    if &reported_comparisons != comparisons {
        return Err(format!(
            "ingest-retention comparison ledger mismatch reported={reported_comparisons:?} observed={comparisons:?}"
        ));
    }
    let reported_passes = reported_counts("invariants", "passes")?;
    if &reported_passes != passes {
        return Err(format!(
            "ingest-retention pass ledger mismatch reported={reported_passes:?} observed={passes:?}"
        ));
    }

    for (operation, invariant) in [
        ("batch-commit", "I20"),
        ("seal", "I21"),
        ("retention", "I22"),
        ("purge", "I23"),
    ] {
        let expected_executions = ledgers.operations.get(operation).copied().unwrap_or(0);
        let expected_checks = passes.get(invariant).copied().unwrap_or(0);
        if ingest["operations"][operation]["executions"].as_u64() != Some(expected_executions)
            || ingest["operations"][operation]["qualifying_checks"].as_u64()
                != Some(expected_checks)
        {
            return Err(format!(
                "ingest-retention {operation} operation ledger mismatch executions={expected_executions} qualifying_checks={expected_checks}"
            ));
        }
    }

    let reported_pairs = reported_counts("faults", "same_seed_pairs")?;
    if reported_pairs != ledgers.fault_pairs || reported_pairs != ledgers.same_seed_controls {
        return Err(format!(
            "ingest-retention same-seed pair ledger mismatch reported={reported_pairs:?} selected={:?} controls={:?}",
            ledgers.fault_pairs, ledgers.same_seed_controls
        ));
    }
    let reported_receipts = reported_counts("faults", "production_receipts")?;
    if reported_receipts != ledgers.production_receipts {
        return Err(format!(
            "ingest-retention production receipt ledger mismatch reported={reported_receipts:?} observed={:?}",
            ledgers.production_receipts
        ));
    }
    let reported_sites = ingest["receipt_sites"]
        .as_object()
        .ok_or_else(|| "ingest-retention receipt-site attestation is not an object".to_owned())?
        .iter()
        .map(|(key, value)| {
            value
                .as_u64()
                .map(|count| (key.clone(), count))
                .ok_or_else(|| format!("ingest-retention receipt site {key} count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if reported_sites != ledgers.receipt_sites {
        return Err(format!(
            "ingest-retention receipt-site ledger mismatch reported={reported_sites:?} observed={:?}",
            ledgers.receipt_sites
        ));
    }
    let expected_fixtures = comparisons.values().try_fold(0_u64, |total, count| {
        total
            .checked_add(*count)
            .ok_or_else(|| "ingest-retention retained fixture count overflowed".to_owned())
    })?;
    if ledgers.retained_fixtures != expected_fixtures {
        return Err(format!(
            "ingest-retention retained fixture ledger mismatch expected={expected_fixtures} observed={}",
            ledgers.retained_fixtures
        ));
    }
    if ingest["completed_seeds"].as_u64() != Some(episodes) {
        return Err(format!(
            "ingest-retention completed seed ledger mismatch reported={} observed={episodes}",
            ingest["completed_seeds"]
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct MetadataMergedLedgers {
    fault_pairs: BTreeMap<String, u64>,
    production_receipts: BTreeMap<String, u64>,
    branches: BTreeMap<String, u64>,
    fallbacks: BTreeMap<String, u64>,
    i37_fault_case_counts: BTreeMap<String, u64>,
}

fn read_metadata_merged_ledgers(root: &Path) -> Result<MetadataMergedLedgers, String> {
    let spec = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner);
    let mut fault_pairs = spec
        .feature_faults
        .iter()
        .map(|fault| (fault.key().to_owned(), 0_u64))
        .collect::<BTreeMap<_, _>>();
    let mut production_receipts = fault_pairs.clone();
    let mut branches = [
        "pruned",
        "exact-allow-list",
        "masked-scan",
        "filtered-graph",
        "graph-exact-fallback",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), 0_u64))
    .collect::<BTreeMap<_, _>>();
    let mut fallbacks = ["none", "visited-budget", "candidate-shortfall"]
        .into_iter()
        .map(|key| (key.to_owned(), 0_u64))
        .collect::<BTreeMap<_, _>>();
    let mut i37_fault_case_counts = (0
        ..adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT)
        .map(|case| {
            (
                adversarial::metadata_filter_planner::i37_predicate_case_key(case).to_owned(),
                0_u64,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let fault_bytes = std::fs::read(root.join("merged-faults.jsonl"))
        .map_err(|error| format!("read merged metadata faults: {error}"))?;
    for line in fault_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged metadata fault row: {error}"))?;
        let record = &envelope["record"];
        if record["type"] != "feature"
            || record["campaign"] != CampaignKind::MetadataFilterPlanner.key()
        {
            continue;
        }
        let key = record["key"]
            .as_str()
            .ok_or_else(|| "merged metadata feature fault omitted key".to_owned())?;
        let fault = spec
            .feature_faults
            .iter()
            .find(|fault| fault.key() == key)
            .ok_or_else(|| format!("merged metadata fault key is unknown: {key}"))?;
        if record["fired"].as_bool() != Some(true)
            || record["fire_count"].as_u64() != Some(fault.required_receipt_cardinality() as u64)
        {
            return Err(format!(
                "merged metadata fault {key} did not fire with exact receipt cardinality"
            ));
        }
        let count = fault_pairs
            .get_mut(key)
            .expect("metadata fault map came from the same catalog");
        *count = (*count).saturating_add(1);
        if key == "bitmap-truncation" {
            let seed = envelope["seed"]
                .as_u64()
                .ok_or_else(|| "merged metadata bitmap fault omitted episode seed".to_owned())?;
            let case = adversarial::metadata_filter_planner::i37_predicate_case_key(seed);
            let case_count = i37_fault_case_counts
                .get_mut(case)
                .expect("metadata I37 fault case came from the complete family catalog");
            *case_count = case_count
                .checked_add(1)
                .ok_or_else(|| format!("metadata I37 fault case {case} count overflowed"))?;
        }
    }

    let receipt_bytes = std::fs::read(root.join("merged-receipts.jsonl"))
        .map_err(|error| format!("read merged metadata receipts: {error}"))?;
    for line in receipt_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged metadata receipt row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::MetadataFilterPlanner.key() {
            continue;
        }
        if let Some(fault) = record["fault"].as_str() {
            let count = production_receipts.get_mut(fault).ok_or_else(|| {
                format!("merged metadata production receipt has unknown fault {fault}")
            })?;
            *count = (*count).saturating_add(1);
            continue;
        }
        if record["site"] != "planner.exec.execution-receipt" {
            return Err(format!(
                "merged metadata receipt has neither a feature fault nor an execution site: {record}"
            ));
        }
        if record["operation"] != "metadata_execution_truth" {
            continue;
        }
        let branch = record["receipt"]["branch"]
            .as_str()
            .ok_or_else(|| "merged metadata execution receipt omitted branch".to_owned())?;
        if let Some(count) = branches.get_mut(branch) {
            *count = (*count).saturating_add(1);
        } else if branch != "graph" {
            return Err(format!(
                "merged metadata execution receipt has unknown branch {branch}"
            ));
        }
        let fallback = record["receipt"]["fallback"]
            .as_str()
            .ok_or_else(|| "merged metadata execution receipt omitted fallback".to_owned())?;
        if let Some(count) = fallbacks.get_mut(fallback) {
            *count = (*count).saturating_add(1);
        } else if fallback != "ef-widened" {
            return Err(format!(
                "merged metadata execution receipt has unknown fallback {fallback}"
            ));
        }
    }

    let control_bytes = std::fs::read(root.join("merged-controls.jsonl"))
        .map_err(|error| format!("read merged metadata controls: {error}"))?;
    let mut fault_source_digests = BTreeMap::<(u64, String), u64>::new();
    for line in control_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged metadata control row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::MetadataFilterPlanner.key() {
            continue;
        }
        validate_metadata_control_evidence(&record["control"])?;
        let seed = record["seed"]
            .as_u64()
            .ok_or_else(|| "merged metadata control omitted seed".to_owned())?;
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "merged metadata control omitted operation".to_owned())?
            .to_owned();
        let digest = record["control"]["fault_source_digest"]
            .as_u64()
            .unwrap_or(0);
        if digest != 0 {
            fault_source_digests.insert((seed, operation), digest);
        }
    }

    let mut fixture_post_digests = BTreeMap::<(u64, String, String), u64>::new();
    for path in [
        root.join("merged-mutations.jsonl"),
        root.join("merged-family-fixture-mutations.jsonl.jsonl"),
    ] {
        let bytes = std::fs::read(&path).map_err(|error| {
            format!("read merged metadata mutations {}: {error}", path.display())
        })?;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let envelope: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line).map_err(|error| {
                    format!("parse merged metadata mutation {}: {error}", path.display())
                })?;
            let record = &envelope["record"];
            if record["campaign"] != CampaignKind::MetadataFilterPlanner.key() {
                continue;
            }
            if record["mutation"].is_null() {
                continue;
            }
            let role = record["role"]
                .as_str()
                .ok_or_else(|| "merged metadata mutation omitted role".to_owned())?;
            if !matches!(role, "selected-fault" | "fixture-preparation") {
                return Err(format!("merged metadata mutation has unknown role {role}"));
            }
            validate_metadata_mutation_evidence(&record["mutation"])?;
            let seed = record["seed"]
                .as_u64()
                .ok_or_else(|| "merged metadata mutation omitted seed".to_owned())?;
            let operation = record["operation"]
                .as_str()
                .ok_or_else(|| "merged metadata mutation omitted operation".to_owned())?;
            let observed = record["mutation"]["post_mutation_artifact_digest"]
                .as_u64()
                .unwrap_or(0);
            if role == "selected-fault" {
                if let Some(expected) = fault_source_digests.get(&(seed, operation.to_owned()))
                    && observed != *expected
                {
                    return Err(format!(
                        "metadata mutation post digest differs from same-seed fault artifact: operation={operation} seed={seed} expected={expected} observed={observed}"
                    ));
                }
            } else {
                let source = record["mutation"]["source"]
                    .as_str()
                    .ok_or_else(|| "metadata fixture mutation omitted source".to_owned())?
                    .to_owned();
                let key = (seed, operation.to_owned(), source);
                let expected = fixture_post_digests.entry(key).or_insert(observed);
                if observed != *expected {
                    return Err(format!(
                        "metadata fixture mutations disagree on their post-mutation artifact digest: operation={operation} seed={seed} expected={expected} observed={observed}"
                    ));
                }
            }
        }
    }

    Ok(MetadataMergedLedgers {
        fault_pairs,
        production_receipts,
        branches,
        fallbacks,
        i37_fault_case_counts,
    })
}

fn read_merged_coverage_counts(root: &Path) -> Result<BTreeMap<String, u64>, String> {
    let bytes = std::fs::read(root.join("merged-coverage.jsonl"))
        .map_err(|error| format!("read merged-coverage.jsonl: {error}"))?;
    let mut counts = BTreeMap::<String, u64>::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged coverage row: {error}"))?;
        let record = envelope["record"]
            .as_object()
            .ok_or_else(|| "merged coverage row record is not an object".to_owned())?;
        for (key, value) in record {
            let value = value
                .as_u64()
                .ok_or_else(|| format!("merged coverage {key} count is not u64"))?;
            let total = counts.entry(key.clone()).or_default();
            *total = total
                .checked_add(value)
                .ok_or_else(|| format!("merged coverage {key} count overflowed"))?;
        }
    }
    Ok(counts)
}

fn validate_storage_attested_coverage(
    storage: &zeppelin_embed_bench::harness_json::Value,
    observed: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let compare = |label: &str, attested: u64, coverage_key: String| -> Result<(), String> {
        let observed = observed.get(&coverage_key).copied().unwrap_or(0);
        if attested != observed {
            return Err(format!(
                "storage-durability {label} coverage differs: attested={attested} observed={observed} key={coverage_key}"
            ));
        }
        Ok(())
    };
    for (operation, record) in storage["operations"]
        .as_object()
        .ok_or_else(|| "storage-durability operations is not an object".to_owned())?
    {
        compare(
            &format!("operation {operation}"),
            record["executions"].as_u64().unwrap_or(0),
            format!("campaign.op.storage-durability.{operation}"),
        )?;
    }
    for (fault, record) in storage["faults"]
        .as_object()
        .ok_or_else(|| "storage-durability faults is not an object".to_owned())?
    {
        let key = format!("feature_fault.storage-durability.{fault}");
        compare(
            &format!("fault {fault} pairs"),
            record["same_seed_pairs"].as_u64().unwrap_or(0),
            key.clone(),
        )?;
        compare(
            &format!("fault {fault} receipts"),
            record["production_receipts"].as_u64().unwrap_or(0),
            key,
        )?;
    }
    for (field, prefix) in [
        ("format_cases", "storage.format-case."),
        ("omission_cases", "storage.omission."),
        ("receipt_sites", "storage.receipt-site."),
    ] {
        for (case, count) in storage[field]
            .as_object()
            .ok_or_else(|| format!("storage-durability {field} is not an object"))?
        {
            compare(
                &format!("{field} {case}"),
                count.as_u64().unwrap_or(0),
                format!("{prefix}{case}"),
            )?;
        }
    }
    Ok(())
}

fn validate_vector_attested_coverage(
    vector: &zeppelin_embed_bench::harness_json::Value,
    observed: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let compare = |label: &str, attested: u64, coverage_key: String| -> Result<(), String> {
        let observed = observed.get(&coverage_key).copied().unwrap_or(0);
        if attested != observed {
            return Err(format!(
                "vector-execution {label} coverage differs: attested={attested} observed={observed} key={coverage_key}"
            ));
        }
        Ok(())
    };
    for (operation, count) in vector["same_seed_controls"]["operations"]
        .as_object()
        .ok_or_else(|| "vector-execution operations are not an object".to_owned())?
    {
        compare(
            &format!("operation {operation}"),
            count.as_u64().unwrap_or(0),
            format!("campaign.op.vector-execution.{operation}"),
        )?;
    }
    for field in ["same_seed_controls", "integrated_receipts"] {
        for (fault, count) in vector[field]["faults"]
            .as_object()
            .ok_or_else(|| format!("vector-execution {field} faults are not an object"))?
        {
            compare(
                &format!("{field} fault {fault}"),
                count.as_u64().unwrap_or(0),
                format!("feature_fault.vector-execution.{fault}"),
            )?;
        }
    }
    for (site, count) in vector["integrated_receipts"]["sites"]
        .as_object()
        .ok_or_else(|| "vector-execution receipt sites are not an object".to_owned())?
    {
        compare(
            &format!("receipt site {site}"),
            count.as_u64().unwrap_or(0),
            site.clone(),
        )?;
    }
    validate_vector_attested_generic_pairs(
        vector,
        &VectorGenericPairLedger {
            scheduled: observed
                .get("vector.generic-pair.scheduled")
                .copied()
                .unwrap_or(0),
            clean_fired: observed
                .get("vector.generic-pair.clean-fired")
                .copied()
                .unwrap_or(0),
            fault_fired: observed
                .get("vector.generic-pair.fault-fired")
                .copied()
                .unwrap_or(0),
            same_path: observed
                .get("vector.generic-pair.same-path")
                .copied()
                .unwrap_or(0),
            isolated_directories: observed
                .get("vector.generic-pair.isolated-directories")
                .copied()
                .unwrap_or(0),
            isolated_runtimes: observed
                .get("vector.generic-pair.isolated-runtimes")
                .copied()
                .unwrap_or(0),
            typed_feature_receipts: observed
                .get("vector.generic-pair.typed-feature-receipt")
                .copied()
                .unwrap_or(0),
        },
    )?;
    for (field, prefix) in [
        ("selected", "I24.store-selected."),
        ("observed", "kernel.backend."),
    ] {
        for backend in vector["backend_inventory"][field]
            .as_array()
            .ok_or_else(|| format!("vector-execution backend {field} is not an array"))?
        {
            let backend = backend
                .as_str()
                .ok_or_else(|| format!("vector-execution backend {field} is not a string"))?;
            let coverage_key = format!("{prefix}{backend}");
            if observed.get(&coverage_key).copied().unwrap_or(0) == 0 {
                return Err(format!(
                    "vector-execution backend {field} {backend} is absent from merged coverage key={coverage_key}"
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VectorGenericPairLedger {
    scheduled: u64,
    clean_fired: u64,
    fault_fired: u64,
    same_path: u64,
    isolated_directories: u64,
    isolated_runtimes: u64,
    typed_feature_receipts: u64,
}

fn validate_vector_attested_generic_pairs(
    vector: &zeppelin_embed_bench::harness_json::Value,
    observed: &VectorGenericPairLedger,
) -> Result<(), String> {
    let pairs = vector["generic_fault_pairs"]
        .as_object()
        .ok_or_else(|| "vector-execution generic fault-pair ledger is not an object".to_owned())?;
    let expected_keys = [
        "scheduled",
        "clean_fired",
        "fault_fired",
        "same_path",
        "isolated_directories",
        "isolated_runtimes",
        "typed_feature_receipts",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    if pairs.keys().cloned().collect::<BTreeSet<_>>() != expected_keys {
        return Err("vector-execution generic fault-pair ledger keys differ".to_owned());
    }
    for (name, observed) in [
        ("scheduled", observed.scheduled),
        ("clean_fired", observed.clean_fired),
        ("fault_fired", observed.fault_fired),
        ("same_path", observed.same_path),
        ("isolated_directories", observed.isolated_directories),
        ("isolated_runtimes", observed.isolated_runtimes),
        ("typed_feature_receipts", observed.typed_feature_receipts),
    ] {
        let attested = pairs[name]
            .as_u64()
            .ok_or_else(|| format!("vector-execution generic pair {name} is not u64"))?;
        if attested != observed {
            return Err(format!(
                "vector-execution generic pair {name} differs: attested={attested} observed={observed}"
            ));
        }
    }
    Ok(())
}

fn read_vector_merged_generic_pairs(root: &Path) -> Result<VectorGenericPairLedger, String> {
    let bytes = std::fs::read(root.join("merged-controls.jsonl"))
        .map_err(|error| format!("read merged vector controls: {error}"))?;
    let mut ledger = VectorGenericPairLedger::default();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let envelope: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse merged vector control row: {error}"))?;
        let record = &envelope["record"];
        if record["campaign"] != CampaignKind::VectorExecution.key()
            || record["generic_fault"].is_null()
        {
            continue;
        }
        let generic = record["generic_fault"]
            .as_object()
            .ok_or_else(|| "merged vector generic fault is not an object".to_owned())?;
        let required = [
            "operation",
            "feature_fault",
            "feature_mutation",
            "program_op_index",
            "schedule",
            "clean",
            "fault",
            "clean_initial_directory",
            "fault_initial_directory",
            "isolated_directories",
            "isolated_runtimes",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        if generic.keys().cloned().collect::<BTreeSet<_>>() != required
            || generic["operation"].as_str().is_none()
            || generic["feature_fault"].as_str().is_none()
            || !generic["feature_mutation"].is_object()
            || generic["program_op_index"].as_u64().is_none()
            || !generic["schedule"].is_object()
            || !generic["clean"].is_object()
            || !generic["fault"].is_object()
        {
            return Err("merged vector generic fault-pair schema differs".to_owned());
        }
        ledger.scheduled = ledger.scheduled.saturating_add(1);
        let clean_event = &generic["clean"]["event"];
        let fault_event = &generic["fault"]["event"];
        if clean_event["fired"].as_bool() == Some(true) {
            ledger.clean_fired = ledger.clean_fired.saturating_add(1);
        }
        if fault_event["fired"].as_bool() == Some(true) {
            ledger.fault_fired = ledger.fault_fired.saturating_add(1);
        }
        if clean_event["path"].as_str().is_some() && clean_event["path"] == fault_event["path"] {
            ledger.same_path = ledger.same_path.saturating_add(1);
        }
        if generic["isolated_directories"].as_bool() == Some(true)
            && generic["clean_initial_directory"] == generic["fault_initial_directory"]
        {
            ledger.isolated_directories = ledger.isolated_directories.saturating_add(1);
        }
        if generic["isolated_runtimes"].as_bool() == Some(true) {
            ledger.isolated_runtimes = ledger.isolated_runtimes.saturating_add(1);
        }
        let receipts = generic["fault"]["feature_receipts"]
            .as_array()
            .ok_or_else(|| "merged vector generic fault receipts are not an array".to_owned())?;
        ledger.typed_feature_receipts = ledger
            .typed_feature_receipts
            .saturating_add(receipts.len() as u64);
    }
    Ok(ledger)
}

fn validate_vector_attested_comparisons(
    vector: &zeppelin_embed_bench::harness_json::Value,
    observed: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let attested = vector["per_invariant_comparisons"]
        .as_object()
        .ok_or_else(|| "vector-execution per-invariant comparisons are not an object".to_owned())?
        .iter()
        .map(|(invariant, record)| {
            record["comparisons"]
                .as_u64()
                .map(|count| (invariant.clone(), count))
                .ok_or_else(|| format!("vector-execution {invariant} comparison count is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let all = attested
        .keys()
        .chain(observed.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for invariant in all {
        let attested = attested.get(&invariant).copied().unwrap_or(0);
        let observed = observed.get(&invariant).copied().unwrap_or(0);
        if attested != observed {
            return Err(format!(
                "vector-execution {invariant} comparison count differs: attested={attested} observed={observed}"
            ));
        }
    }
    Ok(())
}

fn vector_oracle_attestation_json(
    episodes: u64,
    counters: &CampaignAttestationCounters,
    evidence_digests: &BTreeMap<String, String>,
    coverage: Option<&CoverageRegistry>,
) -> zeppelin_embed_bench::harness_json::Value {
    let invariant = |key: &str, checker_id: &'static str| {
        let comparisons = counters.comparison_counts.get(key).copied().unwrap_or(0);
        let passes = counters
            .comparison_pass_counts
            .get(key)
            .copied()
            .unwrap_or(0);
        zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "comparisons": comparisons,
            "passes": passes,
            "plants": 0,
        })
    };
    let per_invariant_comparisons = BTreeMap::from([
        (
            "I24".to_owned(),
            invariant(
                "I24",
                zeppelin_embed_adversarial_oracle::vector_execution::I24_CHECKER_ID,
            ),
        ),
        (
            "I25".to_owned(),
            invariant(
                "I25",
                zeppelin_embed_adversarial_oracle::vector_execution::I25_CHECKER_ID,
            ),
        ),
        (
            "I26".to_owned(),
            invariant(
                "I26",
                zeppelin_embed_adversarial_oracle::vector_execution::I26_CHECKER_ID,
            ),
        ),
        (
            "I27".to_owned(),
            invariant(
                "I27",
                zeppelin_embed_adversarial_oracle::vector_execution::I27_CHECKER_ID,
            ),
        ),
    ]);
    let coverage_count = |key: &str| coverage.map_or(0, |coverage| coverage.count(key));
    let faults = CampaignSpec::for_kind(CampaignKind::VectorExecution)
        .feature_faults
        .iter()
        .map(|fault| {
            (
                fault.key().to_owned(),
                coverage_count(&fault.coverage_key()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let operations = ["kernel-parity", "quantization", "rescore", "row-identity"]
        .into_iter()
        .map(|operation| {
            (
                operation.to_owned(),
                coverage_count(&format!("campaign.op.vector-execution.{operation}")),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let all_backends = [
        "scalar",
        "neon-widen",
        "neon-dotprod-u4",
        "neon-i8mm",
        "neon-dotprod-u2",
        "neon-dotprod-u6",
        "neon-dotprod-u8",
        "neon-dotprod-u4-prefetch",
        "avx2",
    ];
    let available = zeppelin_embed::kernels::KernelVariant::available()
        .map(|variant| variant.backend_id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let unavailable = all_backends
        .iter()
        .copied()
        .filter(|backend| !available.contains(*backend))
        .collect::<Vec<_>>();
    let selected = all_backends
        .iter()
        .copied()
        .filter(|backend| coverage_count(&format!("I24.store-selected.{backend}")) > 0)
        .collect::<Vec<_>>();
    let observed = available
        .iter()
        .filter(|backend| coverage_count(&format!("kernel.backend.{backend}")) > 0)
        .cloned()
        .collect::<Vec<_>>();
    let features = zeppelin_embed::kernels::detected_features();
    let fault_sites = CampaignSpec::for_kind(CampaignKind::VectorExecution)
        .all_required_coverage()
        .into_iter()
        .filter(|key| key.starts_with("fault."))
        .map(|key| {
            let count = coverage_count(&key);
            (key, count)
        })
        .collect::<BTreeMap<_, _>>();
    let source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/vector_execution.rs"
    )]);
    zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract":
            zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_ORACLE_CONTRACT,
        "oracle_source_digest": source_digest,
        "harness_git_revision": adversarial::artifacts::harness_git_revision(),
        "dirty_state": adversarial::artifacts::harness_git_dirty_state(),
        "fixture_digest": evidence_digests.get("family/fixture.json"),
        "per_invariant_comparisons": per_invariant_comparisons,
        "same_seed_controls": {
            "operations": operations,
            "faults": faults,
            "pairs": counters.same_seed_clean_controls,
            "passed": counters.same_seed_clean_controls,
        },
        "integrated_receipts": {
            "expected": counters.expected_feature_fault_receipts,
            "observed": counters.integrated_feature_fault_receipts,
            "faults": faults,
            "sites": fault_sites,
        },
        "generic_fault_pairs": {
            "scheduled": coverage_count("vector.generic-pair.scheduled"),
            "clean_fired": coverage_count("vector.generic-pair.clean-fired"),
            "fault_fired": coverage_count("vector.generic-pair.fault-fired"),
            "same_path": coverage_count("vector.generic-pair.same-path"),
            "isolated_directories": coverage_count("vector.generic-pair.isolated-directories"),
            "isolated_runtimes": coverage_count("vector.generic-pair.isolated-runtimes"),
            "typed_feature_receipts": coverage_count("vector.generic-pair.typed-feature-receipt"),
        },
        "operation_evidence_digests": {
            "program": evidence_digests.get("program"),
            "fixture": evidence_digests.get("family/fixture.json"),
            "control": evidence_digests.get("control"),
        },
        "checker_evidence_digests": {
            "oracle": evidence_digests.get("checker"),
        },
        "fault_evidence_digests": {
            "faults": evidence_digests.get("faults"),
            "receipts": evidence_digests.get("receipt"),
            "mutations": evidence_digests.get("mutation"),
        },
        "backend_inventory": {
            "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
            "features": {
                "neon": features.neon,
                "dotprod": features.dotprod,
                "fp16": features.fp16,
                "i8mm": features.i8mm,
                "sme2": features.sme2,
                "avx2": features.avx2,
                "popcnt": features.popcnt,
            },
            "available": available,
            "unavailable": unavailable,
            "selected": selected,
            "observed": observed,
        },
        "evidence_digests": evidence_digests,
        "completed_seeds": episodes,
        "replayed_seeds": counters.replayed_seeds,
    })
}

fn storage_oracle_attestation_json(
    episodes: u64,
    counters: &CampaignAttestationCounters,
    evidence_digests: &BTreeMap<String, String>,
    coverage: Option<&CoverageRegistry>,
) -> zeppelin_embed_bench::harness_json::Value {
    let invariant = |key: &str, checker_id: &'static str| {
        let comparisons = counters.comparison_counts.get(key).copied().unwrap_or(0);
        let passes = counters
            .comparison_pass_counts
            .get(key)
            .copied()
            .unwrap_or(0);
        zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "comparisons": comparisons,
            "passes": passes,
            "plants": 0,
        })
    };
    let invariants = BTreeMap::from([
        (
            "I15".to_owned(),
            invariant(
                "I15",
                zeppelin_embed_adversarial_oracle::storage_durability::I15_CHECKER_ID,
            ),
        ),
        (
            "I16".to_owned(),
            invariant(
                "I16",
                zeppelin_embed_adversarial_oracle::storage_durability::I16_CHECKER_ID,
            ),
        ),
        (
            "I17".to_owned(),
            invariant(
                "I17",
                zeppelin_embed_adversarial_oracle::storage_durability::I17_CHECKER_ID,
            ),
        ),
        (
            "I18".to_owned(),
            invariant(
                "I18",
                zeppelin_embed_adversarial_oracle::storage_durability::I18_CHECKER_ID,
            ),
        ),
        (
            "I19".to_owned(),
            invariant(
                "I19",
                zeppelin_embed_adversarial_oracle::storage_durability::I19_CHECKER_ID,
            ),
        ),
    ]);
    let coverage_count = |key: &str| coverage.map_or(0, |coverage| coverage.count(key));
    let operations = [
        ("publication", "I15"),
        ("wal-prefix", "I16"),
        ("retry", "I17"),
        ("format-check", "I18"),
        ("orphan-cleanup", "I19"),
    ]
    .into_iter()
    .map(|(operation, invariant)| {
        (
            operation.to_owned(),
            zeppelin_embed_bench::harness_json::json!({
                "executions": coverage_count(&format!(
                    "campaign.op.storage-durability.{operation}"
                )),
                "qualifying_checks": counters
                    .comparison_pass_counts
                    .get(invariant)
                    .copied()
                    .unwrap_or(0),
            }),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let faults = CampaignSpec::for_kind(CampaignKind::StorageDurability)
        .feature_faults
        .iter()
        .map(|fault| {
            let pairs = coverage_count(&fault.coverage_key());
            (
                fault.key().to_owned(),
                zeppelin_embed_bench::harness_json::json!({
                    "same_seed_pairs": pairs,
                    "production_receipts": pairs,
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let format_cases = [
        "wal-header",
        "wal-record-body",
        "wal-record-checksum",
        "segment-region",
        "manifest-wrong-family",
        "segment-wrong-family",
        "segment-wrong-identity",
    ]
    .into_iter()
    .map(|case| {
        (
            case.to_owned(),
            coverage_count(&format!("storage.format-case.{case}")),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let omission_cases = [
        "final-segment.list",
        "final-segment.delete",
        "segment-temporary.list",
        "segment-temporary.delete",
        "manifest-temporary.list",
        "manifest-temporary.delete",
    ]
    .into_iter()
    .map(|case| {
        (
            case.to_owned(),
            coverage_count(&format!("storage.omission.{case}")),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let receipt_sites = [
        "wal-open-header-validation",
        "wal-open-record-validation",
        "wal-open-record-checksum",
        "wal-commit-append-after-inner-success",
        "manifest-commit-before-rename",
        "manifest-commit-after-rename",
        "segment-read-region-checksum",
        "manifest-open-family-validation",
        "segment-open-family-validation",
        "segment-open-object-identity",
        "orphan-cleanup-list",
        "orphan-cleanup-delete",
    ]
    .into_iter()
    .map(|site| {
        (
            site.to_owned(),
            coverage_count(&format!("storage.receipt-site.{site}")),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/storage_durability.rs"
    )]);
    zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract_version":
            zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION,
        "oracle_source_digest": source_digest,
        "repository": {
            "revision": adversarial::artifacts::harness_git_revision(),
            "dirty_state": adversarial::artifacts::harness_git_dirty_state(),
        },
        "required_invariants": ["I15", "I16", "I17", "I18", "I19"],
        "invariants": invariants,
        "operations": operations,
        "faults": faults,
        "format_cases": format_cases,
        "omission_cases": omission_cases,
        "receipt_sites": receipt_sites,
        "evidence_digests": evidence_digests,
        "completed_seeds": episodes,
        "replayed_seeds": counters.replayed_seeds,
    })
}

fn ingest_retention_oracle_attestation_json(
    episodes: u64,
    counters: &CampaignAttestationCounters,
    evidence_digests: &BTreeMap<String, String>,
    coverage: Option<&CoverageRegistry>,
) -> zeppelin_embed_bench::harness_json::Value {
    let invariant = |key: &str, checker_id: &'static str| {
        let comparisons = counters.comparison_counts.get(key).copied().unwrap_or(0);
        let passes = counters
            .comparison_pass_counts
            .get(key)
            .copied()
            .unwrap_or(0);
        zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "comparisons": comparisons,
            "passes": passes,
            "plants": 0,
        })
    };
    let invariants = BTreeMap::from([
        (
            "I20".to_owned(),
            invariant(
                "I20",
                zeppelin_embed_adversarial_oracle::ingest_retention::I20_CHECKER_ID,
            ),
        ),
        (
            "I21".to_owned(),
            invariant(
                "I21",
                zeppelin_embed_adversarial_oracle::ingest_retention::I21_CHECKER_ID,
            ),
        ),
        (
            "I22".to_owned(),
            invariant(
                "I22",
                zeppelin_embed_adversarial_oracle::ingest_retention::I22_CHECKER_ID,
            ),
        ),
        (
            "I23".to_owned(),
            invariant(
                "I23",
                zeppelin_embed_adversarial_oracle::ingest_retention::I23_CHECKER_ID,
            ),
        ),
    ]);
    let coverage_count = |key: &str| coverage.map_or(0, |coverage| coverage.count(key));
    let operations = [
        ("batch-commit", "I20"),
        ("seal", "I21"),
        ("retention", "I22"),
        ("purge", "I23"),
    ]
    .into_iter()
    .map(|(operation, invariant)| {
        (
            operation.to_owned(),
            zeppelin_embed_bench::harness_json::json!({
                "executions": coverage_count(&format!(
                    "campaign.op.ingest-retention.{operation}"
                )),
                "qualifying_checks": counters
                    .comparison_pass_counts
                    .get(invariant)
                    .copied()
                    .unwrap_or(0),
            }),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let faults = CampaignSpec::for_kind(CampaignKind::IngestRetention)
        .feature_faults
        .iter()
        .map(|fault| {
            let pairs = coverage_count(&fault.coverage_key());
            (
                fault.key().to_owned(),
                zeppelin_embed_bench::harness_json::json!({
                    "same_seed_pairs": pairs,
                    "production_receipts": pairs,
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let receipt_sites = [
        "ingest.replay.no-wal-append",
        "ingest.commit-many.append-error",
        "seal.after-segment-write.before-manifest-commit",
        "retention.policy-evaluated",
        "purge.old-segment-unlink.error",
        "purge.after-durable-intent.before-rewrite",
    ]
    .into_iter()
    .map(|site| {
        (
            site.to_owned(),
            coverage_count(&format!("ingest.receipt-site.{site}")),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/ingest_retention.rs"
    )]);
    zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract_version":
            zeppelin_embed_adversarial_oracle::ingest_retention::ORACLE_CONTRACT_VERSION,
        "oracle_source_digest": source_digest,
        "repository": {
            "revision": adversarial::artifacts::harness_git_revision(),
            "dirty_state": adversarial::artifacts::harness_git_dirty_state(),
        },
        "required_invariants": ["I20", "I21", "I22", "I23"],
        "invariants": invariants,
        "operations": operations,
        "faults": faults,
        "receipt_sites": receipt_sites,
        "evidence_digests": evidence_digests,
        "completed_seeds": episodes,
        "replayed_seeds": counters.replayed_seeds,
    })
}

fn metadata_oracle_attestation_json(
    episodes: u64,
    counters: &CampaignAttestationCounters,
    evidence_digests: &BTreeMap<String, String>,
    coverage: Option<&CoverageRegistry>,
) -> zeppelin_embed_bench::harness_json::Value {
    let invariant = |key: &str, checker_id: &'static str| {
        let comparisons = counters.comparison_counts.get(key).copied().unwrap_or(0);
        let passes = counters
            .comparison_pass_counts
            .get(key)
            .copied()
            .unwrap_or(0);
        zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "comparisons": comparisons,
            "passes": passes,
            "plants": 0,
        })
    };
    let invariants = BTreeMap::from([
        (
            "I36".to_owned(),
            invariant(
                "I36",
                zeppelin_embed_adversarial_oracle::metadata_filter_planner::I36_CHECKER_ID,
            ),
        ),
        (
            "I37".to_owned(),
            invariant(
                "I37",
                zeppelin_embed_adversarial_oracle::metadata_filter_planner::I37_CHECKER_ID,
            ),
        ),
        (
            "I38".to_owned(),
            invariant(
                "I38",
                zeppelin_embed_adversarial_oracle::metadata_filter_planner::I38_CHECKER_ID,
            ),
        ),
        (
            "I39".to_owned(),
            invariant(
                "I39",
                zeppelin_embed_adversarial_oracle::metadata_filter_planner::I39_CHECKER_ID,
            ),
        ),
    ]);
    let coverage_count = |key: &str| coverage.map_or(0, |coverage| coverage.count(key));
    let operations = [
        ("metadata_columns_roundtrip", "I36"),
        ("metadata_bitmap_algebra", "I37"),
        ("metadata_pruning_soundness", "I38"),
        ("metadata_execution_truth", "I39"),
    ]
    .into_iter()
    .map(|(operation, invariant)| {
        (
            operation.to_owned(),
            zeppelin_embed_bench::harness_json::json!({
                "executions": coverage_count(&format!(
                    "campaign.op.metadata-filter-planner.{operation}"
                )),
                "qualifying_checks": counters
                    .comparison_pass_counts
                    .get(invariant)
                    .copied()
                    .unwrap_or(0),
            }),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let i37_predicate_cases = (0..adversarial::metadata_filter_planner::I37_PREDICATE_CASE_COUNT)
        .map(|case| {
            let key = adversarial::metadata_filter_planner::i37_predicate_case_key(case);
            (
                key.to_owned(),
                coverage_count(&format!("metadata.i37.matrix.{key}")),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let faults = CampaignSpec::for_kind(CampaignKind::MetadataFilterPlanner)
        .feature_faults
        .iter()
        .map(|fault| {
            let same_seed_pairs = coverage_count(&fault.coverage_key());
            let production_receipts = match fault.key() {
                "column-corruption" => {
                    coverage_count("metadata.receipt.column-corruption.cardinality-one")
                }
                "bitmap-truncation" => {
                    coverage_count("metadata.receipt.bitmap-truncation.cardinality-one")
                }
                "selectivity-boundary" => {
                    coverage_count("metadata.receipt.selectivity-boundary.cardinality-two")
                        .saturating_mul(2)
                }
                "visited-budget-fallback" => {
                    coverage_count("metadata.receipt.visited-budget.cardinality-one")
                }
                other => panic!("unexpected metadata fault in attestation: {other}"),
            };
            (
                fault.key().to_owned(),
                zeppelin_embed_bench::harness_json::json!({
                    "same_seed_pairs": same_seed_pairs,
                    "production_receipts": production_receipts,
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let branches = [
        "pruned",
        "exact-allow-list",
        "masked-scan",
        "filtered-graph",
        "graph-exact-fallback",
    ]
    .into_iter()
    .map(|branch| {
        (
            branch.to_owned(),
            coverage_count(&format!("metadata.i39.branch.{branch}")),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let fallbacks = ["none", "visited-budget", "candidate-shortfall"]
        .into_iter()
        .map(|fallback| {
            (
                fallback.to_owned(),
                coverage_count(&format!("metadata.i39.fallback.{fallback}")),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let source_digest = adversarial::artifacts::evidence_digest(&[include_bytes!(
        "adversarial-oracle/src/metadata_filter_planner.rs"
    )]);
    zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract": "metadata-filter-planner-oracle-v2",
        "oracle_source_digest": source_digest,
        "repository": {
            "revision": adversarial::artifacts::harness_git_revision(),
            "dirty_state": adversarial::artifacts::harness_git_dirty_state(),
        },
        "required_invariants": ["I36", "I37", "I38", "I39"],
        "invariants": invariants,
        "operations": operations,
        "i37_predicate_cases": i37_predicate_cases,
        "faults": faults,
        "branches": branches,
        "fallbacks": fallbacks,
        "evidence_digests": evidence_digests,
        "completed_seeds": episodes,
        "replayed_seeds": counters.replayed_seeds,
    })
}

fn campaign_attestation_json(
    config: &RunConfig,
    episodes: u64,
    counters: &CampaignAttestationCounters,
    merged: Option<adversarial::artifacts::MergedEvidenceStats>,
    coverage: Option<&CoverageRegistry>,
) -> Option<zeppelin_embed_bench::harness_json::Value> {
    if config.campaign == CampaignKind::Overall {
        return None;
    }
    let merged = merged.expect("feature campaign owns merged evidence");
    let valid = feature_attestation_complete(config, episodes, counters, Some(merged.clone()));
    let merged_streams = merged
        .streams
        .keys()
        .map(|name| (name.clone(), merged_stream_json(&merged, name)))
        .collect::<BTreeMap<_, _>>();
    let mut evidence_digests = BTreeMap::from([
        (
            "program".to_owned(),
            merged.streams["program"].digest.clone(),
        ),
        ("faults".to_owned(), merged.streams["faults"].digest.clone()),
        (
            "violations".to_owned(),
            merged.streams["violations"].digest.clone(),
        ),
        (
            "coverage".to_owned(),
            merged.streams["coverage"].digest.clone(),
        ),
        (
            "checker".to_owned(),
            merged.streams["oracle"].digest.clone(),
        ),
        (
            "control".to_owned(),
            merged.streams["controls"].digest.clone(),
        ),
        (
            "receipt".to_owned(),
            merged.streams["receipts"].digest.clone(),
        ),
        (
            "mutation".to_owned(),
            merged.streams["mutations"].digest.clone(),
        ),
    ]);
    evidence_digests.extend(merged.streams.iter().filter_map(|(name, stream)| {
        name.strip_prefix("family/")
            .map(|artifact| (format!("family/{artifact}"), stream.digest.clone()))
    }));
    let storage_oracle_attestation = (config.campaign == CampaignKind::StorageDurability)
        .then(|| storage_oracle_attestation_json(episodes, counters, &evidence_digests, coverage));
    let vector_oracle_attestation = (config.campaign == CampaignKind::VectorExecution)
        .then(|| vector_oracle_attestation_json(episodes, counters, &evidence_digests, coverage));
    let metadata_oracle_attestation = (config.campaign == CampaignKind::MetadataFilterPlanner)
        .then(|| metadata_oracle_attestation_json(episodes, counters, &evidence_digests, coverage));
    let ingest_retention_oracle_attestation = (config.campaign == CampaignKind::IngestRetention)
        .then(|| {
            ingest_retention_oracle_attestation_json(
                episodes,
                counters,
                &evidence_digests,
                coverage,
            )
        });
    let family_oracle_contract = match config.campaign {
        CampaignKind::StorageDurability => {
            zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION
        }
        CampaignKind::VectorExecution => {
            zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_ORACLE_CONTRACT
        }
        CampaignKind::MetadataFilterPlanner => "metadata-filter-planner-oracle-v2",
        CampaignKind::IngestRetention => {
            zeppelin_embed_adversarial_oracle::ingest_retention::ORACLE_CONTRACT_VERSION
        }
        _ => adversarial::artifacts::oracle_contract(config.campaign),
    };
    let oracle_contract_versions = BTreeMap::from([(
        config.campaign.key().to_owned(),
        family_oracle_contract.to_owned(),
    )]);
    Some(zeppelin_embed_bench::harness_json::json!({
        "oracle_contract_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
        "oracle_contract": adversarial::artifacts::oracle_contract(config.campaign),
        "oracle_contract_versions": oracle_contract_versions,
        "harness_git_revision": adversarial::artifacts::harness_git_revision(),
        "comparison_counts": counters.comparison_counts,
        "same_seed_clean_controls": counters.same_seed_clean_controls,
        "integrated_feature_fault_receipts": counters.integrated_feature_fault_receipts,
        "expected_feature_fault_receipts": counters.expected_feature_fault_receipts,
        "selected_feature_fault_events": counters.selected_feature_fault_events,
        "valid": valid,
        "evidence_digests": evidence_digests,
        "storage_oracle_attestation": storage_oracle_attestation,
        "vector_oracle_attestation": vector_oracle_attestation,
        "metadata_oracle_attestation": metadata_oracle_attestation,
        "ingest_retention_oracle_attestation": ingest_retention_oracle_attestation,
        "merged_evidence": {
            "episodes": merged.episodes,
            "complete": merged.episodes == episodes,
            "streams": merged_streams,
        },
    }))
}

#[allow(clippy::too_many_arguments)]
fn write_campaign_summary(
    root: &Path,
    config: &RunConfig,
    complete: bool,
    qualification_passed: bool,
    started_unix: u64,
    elapsed: Duration,
    episodes: u64,
    operations: u64,
    faults_fired: u64,
    last_seed: u64,
    coverage: &CoverageRegistry,
    violations: u64,
    execution_errors: u64,
    panics: u64,
    unfired_scheduled_faults: u64,
    failures: &[CampaignFailure],
    attestation_counters: &CampaignAttestationCounters,
    merged_evidence: Option<adversarial::artifacts::MergedEvidenceStats>,
) {
    let spec = CampaignSpec::for_kind(config.campaign);
    let required_invariants = spec
        .required_invariants()
        .into_iter()
        .map(|invariant| invariant.key())
        .collect::<Vec<_>>();
    let checked_invariants = spec
        .required_invariants()
        .into_iter()
        .filter(|invariant| campaign_invariant_checked(config.campaign, *invariant, coverage))
        .map(|invariant| invariant.key())
        .collect::<Vec<_>>();
    let missing_invariants = required_invariants
        .iter()
        .filter(|invariant| !checked_invariants.contains(invariant))
        .cloned()
        .collect::<Vec<_>>();
    let required_feature_faults = spec
        .feature_faults
        .iter()
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    let fired_feature_faults = spec
        .feature_faults
        .iter()
        .filter(|fault| coverage.count(&fault.coverage_key()) > 0)
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    let missing_feature_faults = required_feature_faults
        .iter()
        .filter(|fault| !fired_feature_faults.contains(fault))
        .copied()
        .collect::<Vec<_>>();
    let required_operations = spec.required_operations.to_vec();
    let executed_operations = spec
        .required_operations
        .iter()
        .copied()
        .filter(|operation| {
            coverage.count(&format!(
                "campaign.op.{}.{}",
                config.campaign.key(),
                operation
            )) > 0
        })
        .collect::<Vec<_>>();
    let missing_operations = required_operations
        .iter()
        .copied()
        .filter(|operation| !executed_operations.contains(operation))
        .collect::<Vec<_>>();
    let required_profiles = spec
        .fault_profiles
        .iter()
        .map(|profile| profile.key())
        .collect::<Vec<_>>();
    let executed_profiles = spec
        .fault_profiles
        .iter()
        .filter(|profile| coverage.count(&format!("fault.profile.{}", profile.key())) > 0)
        .map(|profile| profile.key())
        .collect::<Vec<_>>();
    let missing_profiles = required_profiles
        .iter()
        .copied()
        .filter(|profile| !executed_profiles.contains(profile))
        .collect::<Vec<_>>();
    let required_generic_faults = adversarial::coverage::REQUIRED_SMOKE_COVERAGE
        .iter()
        .copied()
        .filter(|key| key.starts_with("fault.site.") || key.starts_with("fault.mode."))
        .collect::<Vec<_>>();
    let fired_generic_faults = required_generic_faults
        .iter()
        .copied()
        .filter(|key| coverage.count(key) > 0)
        .collect::<Vec<_>>();
    let missing_generic_faults = required_generic_faults
        .iter()
        .copied()
        .filter(|key| !fired_generic_faults.contains(key))
        .collect::<Vec<_>>();
    // Only the Rust binding adapter exists; see `missing_campaign_coverage`.
    let required_languages = if config.campaign == CampaignKind::FfiBindings {
        vec!["rust"]
    } else {
        Vec::new()
    };
    let observed_languages = required_languages
        .iter()
        .copied()
        .filter(|language| coverage.count(&format!("binding.language.{language}")) > 0)
        .collect::<Vec<_>>();
    let missing_languages = required_languages
        .iter()
        .copied()
        .filter(|language| !observed_languages.contains(language))
        .collect::<Vec<_>>();
    let required_backends = if config.campaign == CampaignKind::VectorExecution {
        let mut backends = zeppelin_embed::kernels::KernelVariant::available()
            .map(|variant| variant.backend_id().as_str().to_owned())
            .collect::<Vec<_>>();
        backends.sort();
        backends.dedup();
        backends
    } else {
        Vec::new()
    };
    let observed_backends = required_backends
        .iter()
        .filter(|backend| coverage.count(&format!("kernel.backend.{backend}")) > 0)
        .cloned()
        .collect::<Vec<_>>();
    let missing_backends = required_backends
        .iter()
        .filter(|backend| !observed_backends.contains(backend))
        .cloned()
        .collect::<Vec<_>>();
    let mut missing_coverage = missing_campaign_coverage(config.campaign, coverage);
    missing_coverage.extend(empty_family_evidence_streams(
        config.campaign,
        merged_evidence.as_ref(),
    ));
    let coverage_json: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_str(coverage.json().trim())
            .expect("coverage registry JSON");
    let run_verdict = if complete {
        if failures.is_empty() {
            "passed"
        } else {
            "failed"
        }
    } else {
        "running"
    };
    let attestation = campaign_attestation_json(
        config,
        episodes,
        attestation_counters,
        merged_evidence,
        Some(coverage),
    );
    let summary = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-campaign",
        "version": 3,
        "campaign": config.campaign.key(),
        "qualification": config.qualification.key(),
        "verdict": run_verdict,
        "run_verdict": run_verdict,
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "name": std::env::var("HOSTNAME").ok(),
        },
        "complete": complete,
        "qualification_passed": qualification_passed,
        "started_unix": started_unix,
        "elapsed_seconds": elapsed.as_secs_f64(),
        "required_seconds": config.minimum_seconds,
        "required_episodes": config.minimum_episodes,
        "episodes": episodes,
        "operations": operations,
        "faults_fired": faults_fired,
        "failed_episodes": failures.len(),
        "violations": violations,
        "execution_errors": execution_errors,
        "panics": panics,
        "unfired_scheduled_faults": unfired_scheduled_faults,
        "last_seed": last_seed,
        "start_seed": config.start_seed,
        "required_invariants": required_invariants,
        "checked_invariants": checked_invariants,
        "missing_invariants": missing_invariants,
        "required_feature_faults": required_feature_faults,
        "fired_feature_faults": fired_feature_faults,
        "missing_feature_faults": missing_feature_faults,
        "required_operations": required_operations,
        "executed_operations": executed_operations,
        "missing_operations": missing_operations,
        "required_profiles": required_profiles,
        "executed_profiles": executed_profiles,
        "missing_profiles": missing_profiles,
        "required_generic_faults": required_generic_faults,
        "fired_generic_faults": fired_generic_faults,
        "missing_generic_faults": missing_generic_faults,
        "languages": {
            "required": required_languages,
            "observed": observed_languages,
            "missing": missing_languages,
        },
        "backends": {
            "required": required_backends,
            "observed": observed_backends,
            "missing": missing_backends,
        },
        "counters": {
            "episodes": episodes,
            "operations": operations,
            "faults_fired": faults_fired,
            "violations": violations,
            "execution_errors": execution_errors,
            "panics": panics,
            "unfired_scheduled_faults": unfired_scheduled_faults,
        },
        "missing_coverage": missing_coverage,
        "failures": failures.iter().map(CampaignFailure::json).collect::<Vec<_>>(),
        "coverage": coverage_json,
        "attestation": attestation,
    });
    let mut summary = zeppelin_embed_bench::harness_json::to_vec(&summary)
        .expect("serialize campaign summary v3");
    summary.push(b'\n');
    let final_path = root.join("campaign-summary.json");
    let temporary_path = root.join(".campaign-summary.json.tmp");
    let mut temporary = File::create(&temporary_path).expect("create campaign checkpoint");
    temporary
        .write_all(&summary)
        .expect("write campaign checkpoint");
    temporary.sync_all().expect("sync campaign checkpoint");
    std::fs::rename(&temporary_path, &final_path).expect("publish campaign checkpoint");
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .expect("sync campaign checkpoint directory");
}

fn validate_completed_campaign_summary(root: &Path, config: &RunConfig) {
    let bytes =
        std::fs::read(root.join("campaign-summary.json")).expect("read final campaign summary");
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&bytes)
            .expect("parse final campaign summary");
    assert_eq!(summary["version"], 3, "campaign summary schema version");
    assert_eq!(summary["campaign"], config.campaign.key());
    assert_eq!(summary["qualification"], config.qualification.key());
    assert_eq!(summary["complete"], true, "campaign summary is incomplete");
    let failures = summary["failures"]
        .as_array()
        .expect("campaign summary failure ledger");
    assert_eq!(
        summary["failed_episodes"].as_u64(),
        Some(failures.len() as u64),
        "campaign failure count drifted from its ledger"
    );
    let run_passed = failures.is_empty()
        && summary["violations"].as_u64() == Some(0)
        && summary["execution_errors"].as_u64() == Some(0)
        && summary["unfired_scheduled_faults"].as_u64() == Some(0);
    let coverage_complete = summary["missing_coverage"]
        == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_invariants"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_feature_faults"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_operations"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_profiles"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_generic_faults"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["languages"]["missing"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["backends"]["missing"] == zeppelin_embed_bench::harness_json::json!([]);
    let episodes = summary["episodes"]
        .as_u64()
        .expect("campaign summary episode count");
    let attestation_valid =
        verify_feature_summary_attestation(root, config.campaign, episodes, &summary)
            .unwrap_or_else(|error| panic!("campaign summary attestation rejected: {error}"));
    let expected_qualification = run_passed
        && attestation_valid
        && (config.campaign == CampaignKind::Overall
            && config.qualification == Qualification::Exploratory
            || coverage_complete);
    assert_eq!(
        summary["qualification_passed"].as_bool(),
        Some(expected_qualification),
        "campaign qualification disagrees with its evidence"
    );
    assert!(
        summary["elapsed_seconds"].as_f64().unwrap_or_default() >= config.minimum_seconds as f64,
        "campaign summary did not meet its duration"
    );
    assert!(
        episodes >= config.minimum_episodes,
        "campaign summary did not meet its episode count"
    );
}

fn validate_feature_oracle_record(
    campaign: CampaignKind,
    record: &zeppelin_embed_bench::harness_json::Value,
) -> Result<String, String> {
    let invariant = record["invariant"]
        .as_str()
        .ok_or_else(|| "merged oracle row omitted invariant".to_owned())?;
    let expected_binding = match (campaign, invariant) {
        (CampaignKind::StorageDurability, "I15") => Some((
            zeppelin_embed_adversarial_oracle::storage_durability::I15_CHECKER_ID,
            "publication",
        )),
        (CampaignKind::StorageDurability, "I16") => Some((
            zeppelin_embed_adversarial_oracle::storage_durability::I16_CHECKER_ID,
            "wal-prefix",
        )),
        (CampaignKind::StorageDurability, "I17") => Some((
            zeppelin_embed_adversarial_oracle::storage_durability::I17_CHECKER_ID,
            "retry",
        )),
        (CampaignKind::StorageDurability, "I18") => Some((
            zeppelin_embed_adversarial_oracle::storage_durability::I18_CHECKER_ID,
            "format-check",
        )),
        (CampaignKind::StorageDurability, "I19") => Some((
            zeppelin_embed_adversarial_oracle::storage_durability::I19_CHECKER_ID,
            "orphan-cleanup",
        )),
        (CampaignKind::IngestRetention, "I20") => Some((
            zeppelin_embed_adversarial_oracle::ingest_retention::I20_CHECKER_ID,
            "batch-commit",
        )),
        (CampaignKind::IngestRetention, "I21") => Some((
            zeppelin_embed_adversarial_oracle::ingest_retention::I21_CHECKER_ID,
            "seal",
        )),
        (CampaignKind::IngestRetention, "I22") => Some((
            zeppelin_embed_adversarial_oracle::ingest_retention::I22_CHECKER_ID,
            "retention",
        )),
        (CampaignKind::IngestRetention, "I23") => Some((
            zeppelin_embed_adversarial_oracle::ingest_retention::I23_CHECKER_ID,
            "purge",
        )),
        (CampaignKind::VectorExecution, "I24") => Some((
            zeppelin_embed_adversarial_oracle::vector_execution::I24_CHECKER_ID,
            "kernel-parity",
        )),
        (CampaignKind::VectorExecution, "I25") => Some((
            zeppelin_embed_adversarial_oracle::vector_execution::I25_CHECKER_ID,
            "quantization",
        )),
        (CampaignKind::VectorExecution, "I26") => Some((
            zeppelin_embed_adversarial_oracle::vector_execution::I26_CHECKER_ID,
            "rescore",
        )),
        (CampaignKind::VectorExecution, "I27") => Some((
            zeppelin_embed_adversarial_oracle::vector_execution::I27_CHECKER_ID,
            "row-identity",
        )),
        (CampaignKind::VamanaGraph, "I28") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I28_CHECKER_ID,
            "shape",
        )),
        (CampaignKind::VamanaGraph, "I29") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I29_CHECKER_ID,
            "entry-points",
        )),
        (CampaignKind::VamanaGraph, "I30") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I30_CHECKER_ID,
            "search",
        )),
        (CampaignKind::VamanaGraph, "I31") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I31_CHECKER_ID,
            "search",
        )),
        (CampaignKind::VamanaGraph, "I32") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I32_CHECKER_ID,
            "bounded-build",
        )),
        (CampaignKind::VamanaGraph, "I33") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I33_CHECKER_ID,
            "publication",
        )),
        (CampaignKind::VamanaGraph, "I34") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I34_CHECKER_ID,
            "checkpoint",
        )),
        (CampaignKind::VamanaGraph, "I35") => Some((
            zeppelin_embed_adversarial_oracle::vamana_graph::I35_CHECKER_ID,
            "filtered-search",
        )),
        (CampaignKind::Fts, "I40") => Some((
            zeppelin_embed_adversarial_oracle::fts::I40_CHECKER_ID,
            "tokenizer",
        )),
        (CampaignKind::Fts, "I41") => Some((
            zeppelin_embed_adversarial_oracle::fts::I41_CHECKER_ID,
            "regions",
        )),
        (CampaignKind::Fts, "I42") => Some((
            zeppelin_embed_adversarial_oracle::fts::I42_CHECKER_ID,
            "bm25",
        )),
        (CampaignKind::Fts, "I43") => Some((
            zeppelin_embed_adversarial_oracle::fts::I43_CHECKER_ID,
            "pruning",
        )),
        (CampaignKind::Fts, "I44") => Some((
            zeppelin_embed_adversarial_oracle::fts::I44_CHECKER_ID,
            "extras",
        )),
        (CampaignKind::HybridFusion, "I45") => Some((
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I45_CHECKER_ID,
            "provenance",
        )),
        (CampaignKind::HybridFusion, "I46") => Some((
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I46_CHECKER_ID,
            "normalization",
        )),
        (CampaignKind::HybridFusion, "I47") => Some((
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I47_CHECKER_ID,
            "bounded-fusion",
        )),
        (CampaignKind::HybridFusion, "I48") => Some((
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I48_CHECKER_ID,
            "rrf-fallback",
        )),
        (CampaignKind::HybridFusion, "I49") => Some((
            zeppelin_embed_adversarial_oracle::hybrid_fusion::I49_CHECKER_ID,
            "legs",
        )),
        (CampaignKind::TieringMaintenance, "I50") => Some((
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I50_CHECKER_ID,
            "policy",
        )),
        (CampaignKind::TieringMaintenance, "I51") => Some((
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I51_CHECKER_ID,
            "transition",
        )),
        (CampaignKind::TieringMaintenance, "I52") => Some((
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I52_CHECKER_ID,
            "budget",
        )),
        (CampaignKind::TieringMaintenance, "I53") => Some((
            zeppelin_embed_adversarial_oracle::tiering_maintenance::I53_CHECKER_ID,
            "publication",
        )),
        (CampaignKind::LifecycleAccounting, "I54") => Some((
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I54_CHECKER_ID,
            "deadline",
        )),
        (CampaignKind::LifecycleAccounting, "I55") => Some((
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I55_CHECKER_ID,
            "cancellation",
        )),
        (CampaignKind::LifecycleAccounting, "I56") => Some((
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I56_CHECKER_ID,
            "close-drain",
        )),
        (CampaignKind::LifecycleAccounting, "I57") => Some((
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I57_CHECKER_ID,
            "locking",
        )),
        (CampaignKind::LifecycleAccounting, "I58") => Some((
            zeppelin_embed_adversarial_oracle::lifecycle_accounting::I58_CHECKER_ID,
            "accounting",
        )),
        (CampaignKind::DiagnosticsHealth, "I63") => Some((
            zeppelin_embed_adversarial_oracle::diagnostics_health::I63_CHECKER_ID,
            "health",
        )),
        (CampaignKind::DiagnosticsHealth, "I64") => Some((
            zeppelin_embed_adversarial_oracle::diagnostics_health::I64_CHECKER_ID,
            "self-check",
        )),
        (CampaignKind::DiagnosticsHealth, "I65") => Some((
            zeppelin_embed_adversarial_oracle::diagnostics_health::I65_CHECKER_ID,
            "recovery",
        )),
        (CampaignKind::FfiBindings, "I66") => Some((
            zeppelin_embed_adversarial_oracle::ffi_bindings::I66_CHECKER_ID,
            "validation",
        )),
        (CampaignKind::FfiBindings, "I67") => Some((
            zeppelin_embed_adversarial_oracle::ffi_bindings::I67_CHECKER_ID,
            "ownership",
        )),
        (CampaignKind::FfiBindings, "I68") => Some((
            zeppelin_embed_adversarial_oracle::ffi_bindings::I68_CHECKER_ID,
            "containment",
        )),
        (CampaignKind::FfiBindings, "I69") => Some((
            zeppelin_embed_adversarial_oracle::ffi_bindings::I69_CHECKER_ID,
            "deadline",
        )),
        (CampaignKind::FfiBindings, "I70") => Some((
            zeppelin_embed_adversarial_oracle::ffi_bindings::I70_CHECKER_ID,
            "parity",
        )),
        (CampaignKind::MetadataFilterPlanner, "I36") => Some((
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I36_CHECKER_ID,
            "metadata_columns_roundtrip",
        )),
        (CampaignKind::MetadataFilterPlanner, "I37") => Some((
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I37_CHECKER_ID,
            "metadata_bitmap_algebra",
        )),
        (CampaignKind::MetadataFilterPlanner, "I38") => Some((
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I38_CHECKER_ID,
            "metadata_pruning_soundness",
        )),
        (CampaignKind::MetadataFilterPlanner, "I39") => Some((
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::I39_CHECKER_ID,
            "metadata_execution_truth",
        )),
        _ => None,
    }
    .ok_or_else(|| {
        format!(
            "merged oracle row used unbound invariant {invariant} for campaign {}",
            campaign.key()
        )
    })?;
    let checker_id = record["checker_id"]
        .as_str()
        .ok_or_else(|| format!("merged {invariant} oracle row omitted checker_id"))?;
    if checker_id != expected_binding.0 {
        return Err(format!(
            "merged {invariant} oracle row checker_id mismatch: expected={} observed={checker_id}",
            expected_binding.0
        ));
    }
    let operation = record["operation"]
        .as_str()
        .ok_or_else(|| format!("merged {invariant} oracle row omitted operation"))?;
    let operation_matches = operation == expected_binding.1
        || (campaign == CampaignKind::StorageDurability
            && invariant == "I18"
            && operation == "wal-prefix");
    if !operation_matches {
        return Err(format!(
            "merged {invariant} oracle row operation mismatch: expected={}{} observed={operation}",
            expected_binding.1,
            if campaign == CampaignKind::StorageDurability && invariant == "I18" {
                " or wal-prefix"
            } else {
                ""
            }
        ));
    }
    for (field, digest_field) in [
        ("expected", "input_digest"),
        ("observed", "observed_digest"),
    ] {
        let canonical = zeppelin_embed_bench::harness_json::to_vec(&record[field])
            .map_err(|error| format!("canonicalize merged {invariant} {field}: {error}"))?;
        let expected_digest = adversarial::artifacts::evidence_digest(&[&canonical]);
        if record[digest_field].as_str() != Some(expected_digest.as_str()) {
            return Err(format!(
                "merged {invariant} oracle row {digest_field} does not bind canonical {field}"
            ));
        }
    }
    let canonical_version = record["canonical_version"]
        .as_u64()
        .ok_or_else(|| format!("merged {invariant} oracle row omitted family canonical version"))?;
    let oracle_input_digest = record["oracle_input_digest"]
        .as_str()
        .ok_or_else(|| format!("merged {invariant} oracle row omitted family input digest"))?;
    let oracle_observed_digest = record["oracle_observed_digest"]
        .as_str()
        .ok_or_else(|| format!("merged {invariant} oracle row omitted family observed digest"))?;
    if campaign == CampaignKind::MetadataFilterPlanner
        && (canonical_version
            != u64::from(
                zeppelin_embed_adversarial_oracle::metadata_filter_planner::METADATA_CANONICAL_VERSION,
            )
            || !oracle_input_digest.starts_with("metadata-v1:")
            || oracle_input_digest.len() != "metadata-v1:".len() + 16
            || !oracle_observed_digest.starts_with("metadata-v1:")
            || oracle_observed_digest.len() != "metadata-v1:".len() + 16)
    {
        return Err(format!(
            "merged {invariant} oracle row family canonical attestation is stale"
        ));
    }
    if campaign == CampaignKind::MetadataFilterPlanner {
        let validate_metadata_digest =
            |digest: &str, bytes_field: &str, role: &str| -> Result<Vec<u8>, String> {
                let bytes_hex = record[bytes_field].as_str().ok_or_else(|| {
                    format!("merged {invariant} oracle row omitted {bytes_field}")
                })?;
                let bytes = decode_storage_hex(bytes_hex).map_err(|error| {
                    format!("merged {invariant} oracle row invalid {bytes_field}: {error}")
                })?;
                let domain = format!("metadata/{invariant}/{role}/v1");
                let Some(length_bytes) = bytes.get(..8) else {
                    return Err(format!(
                        "merged {invariant} oracle row canonical {role} is truncated"
                    ));
                };
                let length = usize::try_from(u64::from_le_bytes(
                    length_bytes
                        .try_into()
                        .expect("metadata canonical length is exactly eight bytes"),
                ))
                .map_err(|_| {
                    format!("merged {invariant} oracle row canonical {role} length overflowed")
                })?;
                if bytes.get(8..8_usize.saturating_add(length)) != Some(domain.as_bytes()) {
                    return Err(format!(
                        "merged {invariant} oracle row canonical {role} domain is stale"
                    ));
                }
                let recomputed =
                    zeppelin_embed_adversarial_oracle::metadata_filter_planner::canonical_digest(
                        &bytes,
                    );
                let expected = format!("metadata-v{canonical_version}:{recomputed:016x}");
                if digest != expected {
                    return Err(format!(
                        "merged {invariant} oracle row family canonical {role} digest differs"
                    ));
                }
                Ok(bytes)
            };
        let input_bytes =
            validate_metadata_digest(oracle_input_digest, "oracle_input_bytes", "input")?;
        let observed_bytes =
            validate_metadata_digest(oracle_observed_digest, "oracle_observed_bytes", "observed")?;
        let replay = zeppelin_embed_adversarial_oracle::metadata_filter_planner::replay_canonical_comparison(
            checker_id,
            &input_bytes,
            &observed_bytes,
        )
        .map_err(|error| {
            format!("merged {invariant} retained metadata checker replay failed: {error}")
        })?;
        if replay.checker_id != checker_id
            || replay.input_digest
                != zeppelin_embed_adversarial_oracle::metadata_filter_planner::canonical_digest(
                    &input_bytes,
                )
            || replay.observed_digest
                != zeppelin_embed_adversarial_oracle::metadata_filter_planner::canonical_digest(
                    &observed_bytes,
                )
        {
            return Err(format!(
                "merged {invariant} retained metadata checker replay identity diverged"
            ));
        }
        let recorded_passed = record["passed"]
            .as_bool()
            .ok_or_else(|| format!("merged {invariant} oracle row omitted passed"))?;
        if recorded_passed != replay.first_difference.is_none() {
            return Err(format!(
                "merged {invariant} retained metadata checker replay disagreed with passed"
            ));
        }
        let difference_kind = |kind| {
            match kind {
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::ExtraRow => {
                "extra-row"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::MissingRow => {
                "missing-row"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::DuplicateRow => {
                "duplicate-row"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::DeadRow => {
                "dead-row"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::PrimitiveMismatch => {
                "primitive-mismatch"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::UnsoundPrune => {
                "unsound-prune"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::ReportReceiptMismatch => {
                "report-receipt-mismatch"
            }
            zeppelin_embed_adversarial_oracle::metadata_filter_planner::DifferenceKind::ContractMismatch => {
                "contract-mismatch"
            }
        }
        };
        match (&replay.first_difference, &record["first_difference"]) {
            (None, difference) if difference.is_null() => {}
            (Some(replayed), difference) => {
                if difference["checker_id"].as_str() != Some(replayed.checker_id)
                    || difference["path"].as_str() != Some(replayed.path.as_str())
                    || difference["kind"].as_str() != Some(difference_kind(replayed.kind))
                    || difference["row"].as_u64() != replayed.row.map(u64::from)
                    || difference["expected"].as_str() != Some(replayed.expected.as_str())
                    || difference["observed"].as_str() != Some(replayed.observed.as_str())
                {
                    return Err(format!(
                        "merged {invariant} retained metadata checker replay first difference diverged"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "merged {invariant} retained metadata checker replay first difference diverged"
                ));
            }
        }
    }
    if campaign == CampaignKind::IngestRetention {
        if canonical_version
            != u64::from(
                zeppelin_embed_adversarial_oracle::ingest_retention::INGEST_CANONICAL_VERSION,
            )
            || !oracle_input_digest.starts_with("ingest-v1:")
            || oracle_input_digest.len() != "ingest-v1:".len() + 16
            || !oracle_observed_digest.starts_with("ingest-v1:")
            || oracle_observed_digest.len() != "ingest-v1:".len() + 16
        {
            return Err(format!(
                "merged {invariant} oracle row family canonical attestation is stale"
            ));
        }
        let validate_ingest_digest = |digest: &str, bytes_field: &str| -> Result<Vec<u8>, String> {
            let bytes =
                decode_storage_hex(record[bytes_field].as_str().ok_or_else(|| {
                    format!("merged {invariant} oracle row omitted {bytes_field}")
                })?)
                .map_err(|error| {
                    format!("merged {invariant} oracle row invalid {bytes_field}: {error}")
                })?;
            if bytes.is_empty() {
                return Err(format!(
                    "merged {invariant} oracle row omitted retained canonical bytes"
                ));
            }
            let recomputed =
                zeppelin_embed_adversarial_oracle::ingest_retention::canonical_digest(&bytes);
            let expected = format!("ingest-v{canonical_version}:{recomputed:016x}");
            if digest != expected {
                return Err(format!(
                    "merged {invariant} oracle row family canonical digest differs"
                ));
            }
            Ok(bytes)
        };
        let input_bytes = validate_ingest_digest(oracle_input_digest, "oracle_input_bytes")?;
        let observed_bytes =
            validate_ingest_digest(oracle_observed_digest, "oracle_observed_bytes")?;
        let replay =
            zeppelin_embed_adversarial_oracle::ingest_retention::replay_canonical_comparison(
                checker_id,
                &input_bytes,
                &observed_bytes,
            )
            .map_err(|error| {
                format!("merged {invariant} retained ingest checker replay failed: {error}")
            })?;
        if replay.checker_id != checker_id
            || replay.input_digest
                != zeppelin_embed_adversarial_oracle::ingest_retention::canonical_digest(
                    &input_bytes,
                )
            || replay.observed_digest
                != zeppelin_embed_adversarial_oracle::ingest_retention::canonical_digest(
                    &observed_bytes,
                )
        {
            return Err(format!(
                "merged {invariant} retained ingest checker replay identity diverged"
            ));
        }
        let recorded_passed = record["passed"]
            .as_bool()
            .ok_or_else(|| format!("merged {invariant} oracle row omitted passed"))?;
        if recorded_passed != replay.first_difference.is_none() {
            return Err(format!(
                "merged {invariant} retained ingest checker replay disagreed with passed"
            ));
        }
        match (&replay.first_difference, &record["first_difference"]) {
            (None, difference) if difference.is_null() => {}
            (Some(replayed), difference) => {
                if difference["checker_id"].as_str() != Some(replayed.checker_id)
                    || difference["path"].as_str() != Some(replayed.path.as_str())
                    || difference["kind"].as_str() != Some("contract-mismatch")
                    || !difference["row"].is_null()
                    || difference["expected"].as_str() != Some(replayed.expected.as_str())
                    || difference["observed"].as_str() != Some(replayed.observed.as_str())
                {
                    return Err(format!(
                        "merged {invariant} retained ingest checker replay first difference diverged"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "merged {invariant} retained ingest checker replay first difference diverged"
                ));
            }
        }
    }
    if campaign == CampaignKind::VectorExecution {
        let validate_vector_digest = |digest: &str, bytes_field: &str| -> Result<Vec<u8>, String> {
            let bytes_hex = record[bytes_field]
                .as_str()
                .ok_or_else(|| format!("merged {invariant} oracle row omitted {bytes_field}"))?;
            let bytes = decode_storage_hex(bytes_hex).map_err(|error| {
                format!("merged {invariant} oracle row invalid {bytes_field}: {error}")
            })?;
            if !bytes.starts_with(
                zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_CANONICAL_VERSION
                    .as_bytes(),
            ) {
                return Err(format!(
                    "merged {invariant} oracle row family canonical attestation is stale"
                ));
            }
            let recomputed =
                zeppelin_embed_adversarial_oracle::vector_execution::canonical_sha256(&bytes)
                    .into_iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
            let expected = format!(
                "{}:{recomputed}",
                zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_CANONICAL_VERSION
            );
            if digest != expected {
                return Err(format!(
                    "merged {invariant} oracle row family canonical attestation is stale"
                ));
            }
            Ok(bytes)
        };
        if canonical_version != 1 {
            return Err(format!(
                "merged {invariant} oracle row family canonical attestation is stale"
            ));
        }
        let input_bytes = validate_vector_digest(oracle_input_digest, "oracle_input_bytes")?;
        let observed_bytes =
            validate_vector_digest(oracle_observed_digest, "oracle_observed_bytes")?;
        let replay =
            zeppelin_embed_adversarial_oracle::vector_execution::replay_canonical_comparison(
                &input_bytes,
                &observed_bytes,
            )
            .map_err(|error| {
                format!("merged {invariant} retained vector checker replay failed: {error}")
            })?;
        if replay.checker_id != checker_id {
            return Err(format!(
                "merged {invariant} retained vector checker replay used {} instead of {checker_id}",
                replay.checker_id
            ));
        }
        let recorded_passed = record["passed"]
            .as_bool()
            .ok_or_else(|| format!("merged {invariant} oracle row omitted passed"))?;
        if recorded_passed != replay.first_difference.is_none() {
            return Err(format!(
                "merged {invariant} retained vector checker replay disagreed with passed"
            ));
        }
        match (&replay.first_difference, &record["first_difference"]) {
            (None, difference) if difference.is_null() => {}
            (Some(replayed), difference) => {
                if difference["checker_id"].as_str() != Some(replayed.checker_id)
                    || difference["path"].as_str() != Some(replayed.path)
                    || difference["kind"].as_str() != Some("primitive-mismatch")
                    || !difference["row"].is_null()
                    || difference["expected"].as_str() != Some(replayed.expected.as_str())
                    || difference["observed"].as_str() != Some(replayed.observed.as_str())
                {
                    return Err(format!(
                        "merged {invariant} retained vector checker replay first difference diverged"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "merged {invariant} retained vector checker replay first difference diverged"
                ));
            }
        }
    }
    let passed = record["passed"]
        .as_bool()
        .ok_or_else(|| format!("merged {invariant} oracle row omitted passed"))?;
    if passed {
        if !record["first_difference"].is_null() {
            return Err(format!(
                "merged {invariant} passing oracle row carried a first_difference"
            ));
        }
    } else {
        let difference = record["first_difference"].as_object().ok_or_else(|| {
            format!("merged {invariant} failing oracle row omitted structured first_difference")
        })?;
        let required = ["checker_id", "path", "kind", "row", "expected", "observed"]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let observed = difference.keys().cloned().collect::<BTreeSet<_>>();
        if observed != required
            || difference["checker_id"].as_str() != Some(checker_id)
            || difference["path"].as_str().is_none()
            || difference["kind"].as_str().is_none()
            || !(difference["row"].is_null() || difference["row"].as_u64().is_some())
            || difference["expected"].as_str().is_none()
            || difference["observed"].as_str().is_none()
        {
            return Err(format!(
                "merged {invariant} oracle row first_difference schema is invalid"
            ));
        }
    }
    Ok(invariant.to_owned())
}

fn validate_feature_replay_ledger(
    root: &Path,
    campaign: CampaignKind,
    start_seed: u64,
    episodes: u64,
) -> Result<u64, String> {
    let path = root.join("replayed-seeds.jsonl");
    let bytes =
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let expected_keys = BTreeSet::from([
        "artifact_count",
        "campaign",
        "evidence_digest",
        "expected_digest",
        "observed_digest",
        "profile",
        "schema",
        "seed",
        "version",
    ]);
    let valid_digest = |value: &str| {
        value
            .strip_prefix("fnv1a64:")
            .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    let valid_profiles = CampaignSpec::for_kind(campaign)
        .fault_profiles
        .iter()
        .map(|profile| profile.key())
        .collect::<BTreeSet<_>>();
    let mut seeds = BTreeSet::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let row: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse feature replay ledger row: {error}"))?;
        let keys = row
            .as_object()
            .ok_or_else(|| "feature replay ledger row is not an object".to_owned())?
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if keys != expected_keys {
            return Err(format!(
                "feature replay ledger schema mismatch expected={expected_keys:?} observed={keys:?}"
            ));
        }
        if row["schema"] != "zeppelin-embed-adversarial-replay"
            || row["version"] != 1
            || row["campaign"] != campaign.key()
        {
            return Err("feature replay ledger identity mismatch".to_owned());
        }
        if row["artifact_count"].as_u64()
            != Some(adversarial::artifacts::replay_artifacts_for(campaign).len() as u64)
        {
            return Err("feature replay ledger artifact count mismatch".to_owned());
        }
        let profile = row["profile"]
            .as_str()
            .ok_or_else(|| "feature replay ledger profile is not a string".to_owned())?;
        if !valid_profiles.contains(profile) {
            return Err(format!(
                "feature replay ledger profile is unknown: {profile}"
            ));
        }
        let expected_digest = row["expected_digest"]
            .as_str()
            .ok_or_else(|| "feature replay ledger expected digest is not a string".to_owned())?;
        let observed_digest = row["observed_digest"]
            .as_str()
            .ok_or_else(|| "feature replay ledger observed digest is not a string".to_owned())?;
        let evidence_digest = row["evidence_digest"]
            .as_str()
            .ok_or_else(|| "feature replay ledger evidence digest is not a string".to_owned())?;
        if !valid_digest(expected_digest)
            || expected_digest != observed_digest
            || evidence_digest != expected_digest
        {
            return Err(format!(
                "feature replay ledger digest mismatch expected={expected_digest} observed={observed_digest} evidence={evidence_digest}"
            ));
        }
        let seed = row["seed"]
            .as_u64()
            .ok_or_else(|| "feature replay ledger seed is not u64".to_owned())?;
        if !seeds.insert(seed) {
            return Err(format!("feature replay ledger duplicated seed {seed}"));
        }
    }
    let end_seed = start_seed
        .checked_add(episodes)
        .ok_or_else(|| "feature replay seed range overflow".to_owned())?;
    let expected_seeds = (start_seed..end_seed).collect::<BTreeSet<_>>();
    if seeds != expected_seeds {
        return Err(format!(
            "feature replay seed ledger mismatch expected={expected_seeds:?} observed={seeds:?}"
        ));
    }
    Ok(seeds.len() as u64)
}

fn verify_feature_summary_attestation(
    root: &Path,
    campaign: CampaignKind,
    episodes: u64,
    summary: &zeppelin_embed_bench::harness_json::Value,
) -> Result<bool, String> {
    if campaign == CampaignKind::Overall {
        return Ok(true);
    }
    let attestation = &summary["attestation"];
    if campaign == CampaignKind::MetadataFilterPlanner
        && attestation["metadata_oracle_attestation"]["version"].as_u64() != Some(1)
    {
        return Err("missing metadata oracle attestation version 1".to_owned());
    }
    if campaign == CampaignKind::MetadataFilterPlanner {
        validate_metadata_oracle_attestation_shape(&attestation["metadata_oracle_attestation"])?;
    }
    if campaign == CampaignKind::StorageDurability {
        validate_storage_oracle_attestation_shape(&attestation["storage_oracle_attestation"])?;
    }
    if campaign == CampaignKind::VectorExecution {
        validate_vector_oracle_attestation_shape(&attestation["vector_oracle_attestation"])?;
    }
    if campaign == CampaignKind::IngestRetention {
        validate_ingest_retention_oracle_attestation_shape(
            &attestation["ingest_retention_oracle_attestation"],
        )?;
    }
    if attestation["oracle_contract_version"].as_u64()
        != Some(u64::from(
            zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
        ))
    {
        return Err("feature summary attestation missing: oracle_contract_version".to_owned());
    }
    if attestation["oracle_contract"].as_str()
        != Some(adversarial::artifacts::oracle_contract(campaign))
    {
        return Err("feature summary attestation missing or stale: oracle_contract".to_owned());
    }
    let expected_family_contract = match campaign {
        CampaignKind::StorageDurability => {
            zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION
        }
        CampaignKind::VectorExecution => {
            zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_ORACLE_CONTRACT
        }
        CampaignKind::MetadataFilterPlanner => "metadata-filter-planner-oracle-v2",
        _ => adversarial::artifacts::oracle_contract(campaign),
    };
    let contract_versions = attestation["oracle_contract_versions"]
        .as_object()
        .ok_or_else(|| {
            "feature summary attestation missing: oracle_contract_versions".to_owned()
        })?;
    if contract_versions.len() != 1
        || contract_versions[campaign.key()].as_str() != Some(expected_family_contract)
    {
        return Err(format!(
            "feature summary {} family oracle contract version is missing or stale",
            campaign.key()
        ));
    }
    match attestation["harness_git_revision"].as_str() {
        Some(revision) if revision == adversarial::artifacts::harness_git_revision() => {}
        Some(_) => return Err("feature summary harness_git_revision is stale".to_owned()),
        None => return Err("feature summary attestation missing: harness_git_revision".to_owned()),
    }
    let start_seed = summary["start_seed"]
        .as_u64()
        .ok_or_else(|| "feature summary omitted start_seed".to_owned())?;
    let replayed_seeds = validate_feature_replay_ledger(root, campaign, start_seed, episodes)?;
    if campaign == CampaignKind::MetadataFilterPlanner
        && attestation["metadata_oracle_attestation"]["replayed_seeds"].as_u64()
            != Some(replayed_seeds)
    {
        return Err(
            "metadata oracle attestation replayed_seeds disagrees with durable replay ledger"
                .to_owned(),
        );
    }
    if campaign == CampaignKind::StorageDurability
        && attestation["storage_oracle_attestation"]["replayed_seeds"].as_u64()
            != Some(replayed_seeds)
    {
        return Err(
            "storage oracle attestation replayed_seeds disagrees with durable replay ledger"
                .to_owned(),
        );
    }
    if campaign == CampaignKind::VectorExecution
        && attestation["vector_oracle_attestation"]["replayed_seeds"].as_u64()
            != Some(replayed_seeds)
    {
        return Err(
            "vector oracle attestation replayed_seeds disagrees with durable replay ledger"
                .to_owned(),
        );
    }
    if campaign == CampaignKind::IngestRetention
        && attestation["ingest_retention_oracle_attestation"]["replayed_seeds"].as_u64()
            != Some(replayed_seeds)
    {
        return Err(
            "ingest-retention oracle attestation replayed_seeds disagrees with durable replay ledger"
                .to_owned(),
        );
    }
    let reported_comparisons = attestation["comparison_counts"]
        .as_object()
        .ok_or_else(|| "feature summary attestation missing: comparison_counts".to_owned())?
        .iter()
        .map(|(invariant, count)| {
            count
                .as_u64()
                .map(|count| (invariant.clone(), count))
                .ok_or_else(|| format!("comparison count for {invariant} is not u64"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let required_count = |field: &str| {
        attestation[field]
            .as_u64()
            .ok_or_else(|| format!("feature summary attestation missing: {field}"))
    };
    let clean_controls = required_count("same_seed_clean_controls")?;
    let integrated_receipts = required_count("integrated_feature_fault_receipts")?;
    let expected_receipts = required_count("expected_feature_fault_receipts")?;
    let selected_faults = required_count("selected_feature_fault_events")?;
    let merged = &attestation["merged_evidence"];
    let merged_episodes = merged["episodes"].as_u64().ok_or_else(|| {
        "feature summary attestation missing: merged_evidence.episodes".to_owned()
    })?;
    let index_bytes = std::fs::read(root.join("merged-index.jsonl"))
        .map_err(|error| format!("read merged-index.jsonl: {error}"))?;
    let index_records = index_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count() as u64;
    if index_records != merged_episodes {
        return Err(format!(
            "merged index count mismatch: summary={merged_episodes} file={index_records}"
        ));
    }

    let mut observed_comparisons = BTreeMap::<String, u64>::new();
    let mut observed_passes = BTreeMap::<String, u64>::new();
    let mut observed_i37_case_counts = BTreeMap::<String, u64>::new();
    let mut every_comparison_passed = true;
    let mut observed_stream_records = BTreeMap::<String, u64>::new();
    let reported_streams = merged["streams"]
        .as_object()
        .ok_or_else(|| "feature summary attestation missing merged streams".to_owned())?;
    let expected_family = adversarial::artifacts::replay_artifacts_for(campaign)
        .into_iter()
        .filter(|name| !adversarial::artifacts::REPLAY_ARTIFACTS.contains(name))
        .map(|name| format!("family/{name}"))
        .collect::<BTreeSet<_>>();
    let reported_family = reported_streams
        .keys()
        .filter(|name| name.starts_with("family/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    if reported_family != expected_family {
        return Err(format!(
            "merged family stream set mismatch expected={expected_family:?} observed={reported_family:?}"
        ));
    }
    for name in reported_streams.keys() {
        let path = name.strip_prefix("family/").map_or_else(
            || root.join(format!("merged-{name}.jsonl")),
            |artifact| root.join(format!("merged-family-{artifact}.jsonl")),
        );
        let bytes =
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count() as u64;
        observed_stream_records.insert(name.to_owned(), records);
        let reported = &merged["streams"][name];
        if reported["records"].as_u64() != Some(records)
            || reported["bytes"].as_u64() != Some(bytes.len() as u64)
            || reported["digest"].as_str()
                != Some(adversarial::artifacts::evidence_digest(&[&bytes]).as_str())
        {
            return Err(format!("merged {name} count/bytes/digest mismatch"));
        }
        if name == "oracle" {
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let envelope: zeppelin_embed_bench::harness_json::Value =
                    zeppelin_embed_bench::harness_json::from_slice(line)
                        .map_err(|error| format!("parse merged oracle row: {error}"))?;
                let invariant = validate_feature_oracle_record(campaign, &envelope["record"])?;
                if campaign == CampaignKind::MetadataFilterPlanner && invariant == "I37" {
                    let case = envelope["record"]["case_identity"]
                        .as_str()
                        .ok_or_else(|| {
                            "merged metadata I37 oracle row omitted its stable case identity"
                                .to_owned()
                        })?;
                    let case_key =
                        adversarial::metadata_filter_planner::i37_case_key_from_identity(case)
                            .ok_or_else(|| {
                                format!(
                                    "merged metadata I37 oracle row has unknown case identity {case}"
                                )
                            })?;
                    let count = observed_i37_case_counts
                        .entry(case_key.to_owned())
                        .or_default();
                    *count = count.checked_add(1).ok_or_else(|| {
                        format!("merged metadata I37 oracle case {case} count overflowed")
                    })?;
                }
                let count = observed_comparisons.entry(invariant).or_default();
                *count = count.saturating_add(1);
                let passed = envelope["record"]["passed"].as_bool() == Some(true);
                if passed {
                    let pass_count = observed_passes
                        .entry(
                            envelope["record"]["invariant"]
                                .as_str()
                                .expect("validated oracle invariant")
                                .to_owned(),
                        )
                        .or_default();
                    *pass_count = pass_count.saturating_add(1);
                }
                every_comparison_passed &= passed;
            }
        }
    }
    if observed_comparisons != reported_comparisons {
        return Err("comparison counts disagree with merged oracle rows".to_owned());
    }
    if campaign == CampaignKind::StorageDurability {
        let coverage_counts = read_merged_coverage_counts(root)?;
        validate_storage_attested_coverage(
            &attestation["storage_oracle_attestation"],
            &coverage_counts,
        )?;
        let ledgers = read_storage_merged_ledgers(root)?;
        validate_storage_observed_ledgers(
            &attestation["storage_oracle_attestation"],
            &observed_comparisons,
            &ledgers,
            merged_episodes,
        )?;
    }
    if campaign == CampaignKind::VectorExecution {
        let coverage_counts = read_merged_coverage_counts(root)?;
        validate_vector_attested_comparisons(
            &attestation["vector_oracle_attestation"],
            &observed_comparisons,
        )?;
        validate_vector_attested_coverage(
            &attestation["vector_oracle_attestation"],
            &coverage_counts,
        )?;
        let generic_pairs = read_vector_merged_generic_pairs(root)?;
        validate_vector_attested_generic_pairs(
            &attestation["vector_oracle_attestation"],
            &generic_pairs,
        )?;
    }
    if campaign == CampaignKind::MetadataFilterPlanner {
        let coverage_counts = read_merged_coverage_counts(root)?;
        let ledgers = read_metadata_merged_ledgers(root)?;
        validate_metadata_observed_ledgers(
            &attestation["metadata_oracle_attestation"],
            &observed_comparisons,
            &observed_passes,
            &ledgers.fault_pairs,
            &ledgers.production_receipts,
            &ledgers.branches,
            &ledgers.fallbacks,
            &coverage_counts,
            &observed_i37_case_counts,
            &ledgers.i37_fault_case_counts,
            merged_episodes,
        )?;
    }
    if campaign == CampaignKind::IngestRetention {
        let ledgers = read_ingest_retention_merged_ledgers(root)?;
        validate_ingest_retention_observed_ledgers(
            &attestation["ingest_retention_oracle_attestation"],
            &observed_comparisons,
            &observed_passes,
            &ledgers,
            merged_episodes,
        )?;
    }
    let evidence_digests = &attestation["evidence_digests"];
    for (field, stream) in [
        ("program", "program"),
        ("faults", "faults"),
        ("violations", "violations"),
        ("coverage", "coverage"),
        ("checker", "oracle"),
        ("control", "controls"),
        ("receipt", "receipts"),
        ("mutation", "mutations"),
    ] {
        if evidence_digests[field] != merged["streams"][stream]["digest"] {
            return Err(format!(
                "{field} evidence digest disagrees with merged {stream}"
            ));
        }
    }
    for family in &expected_family {
        if evidence_digests[family] != merged["streams"][family]["digest"] {
            return Err(format!(
                "{family} evidence digest disagrees with its merged stream"
            ));
        }
    }
    let profile_override = match std::env::var("ZE_ADV_PROFILE") {
        Ok(value) => Some(FaultProfile::from_key(&value)?),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(format!("read ZE_ADV_PROFILE: {error}")),
    };
    let exact_comparisons = observed_comparisons
        == expected_campaign_comparison_counts_with_profile(
            campaign,
            start_seed,
            episodes,
            profile_override,
        );
    let valid = merged_episodes == episodes
        && exact_comparisons
        && every_comparison_passed
        && clean_controls == selected_faults
        && integrated_receipts == expected_receipts
        && observed_stream_records
            .get("controls")
            .copied()
            .is_some_and(|records| records >= clean_controls)
        && observed_stream_records
            .get("receipts")
            .copied()
            .is_some_and(|records| records >= integrated_receipts);
    if merged["complete"].as_bool() != Some(merged_episodes == episodes) {
        return Err("merged evidence completeness flag disagrees with index".to_owned());
    }
    if attestation["valid"].as_bool() != Some(valid) {
        return Err("feature attestation validity flag disagrees with evidence".to_owned());
    }
    Ok(valid)
}

fn campaign_invariant_checked(
    campaign: CampaignKind,
    invariant: InvariantId,
    coverage: &CoverageRegistry,
) -> bool {
    if coverage.count(&invariant.checked_coverage_key()) > 0 {
        return true;
    }
    if campaign != CampaignKind::Overall {
        return false;
    }
    let evidence = match invariant.number() {
        1 | 2 | 3 | 11 => "op.search",
        4 => "op.crash",
        5 => "op.filtered_search",
        6 => "op.stats",
        7 => "fault.profile.content",
        8 => "op.reopen",
        9 => "op.ingest",
        10 => "op.purge",
        12 => "op.epoch_mismatch_probe",
        13 => "op.predicate_search",
        14 => "op.switch_alias_to_b",
        _ => return false,
    };
    coverage.count(evidence) > 0
}
