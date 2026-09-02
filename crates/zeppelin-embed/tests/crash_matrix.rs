#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochIdentity, Normalization,
    StoreEpoch,
};
use zeppelin_embed::format::FormatFamily;
use zeppelin_embed::format::frame::{FormatCheck, encode_artifact};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::refine::validate_refinement_checkpoint;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::manifest::decode_manifest;
use zeppelin_embed::manifest::io::{
    DurableLog, MANIFEST_FILE, MANIFEST_TEMP_FILE, commit_manifest, load_manifest, open_manifest,
};
use zeppelin_embed::manifest::{EpochMeta, Manifest, ManifestError, encode_manifest};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::segment::reader::validate_segment_bytes;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, encode_segment, write_segment,
};
use zeppelin_embed::segment::{SegmentError, SegmentId, SegmentMeta};
#[cfg(not(feature = "test-support"))]
use zeppelin_embed::vfs::VfsFile;
use zeppelin_embed::vfs::{SyncKind, Vfs};
use zeppelin_embed::wal::header::encode_header as encode_wal_header;
use zeppelin_embed::wal::record::{WalRecord, encode_record};
use zeppelin_embed::wal::{LogSeq, WalReader};
#[cfg(not(feature = "test-support"))]
#[path = "../src/vfs/crash.rs"]
mod crash_support;
#[cfg(not(feature = "test-support"))]
use crash_support::{
    CrashOperation, CrashState, CrashStateClass, CrashStateKind, CrashStates, CrashVfs,
    MAX_TEAR_POINTS_PER_OPERATION, MemoryVfs, TornWriteEdge,
};
#[cfg(feature = "test-support")]
use zeppelin_embed::vfs::crash::{
    CrashOperation, CrashState, CrashStateClass, CrashStateKind, CrashStates, CrashVfs,
    MAX_TEAR_POINTS_PER_OPERATION, MemoryVfs, TornWriteEdge,
};

const DIRECTORY: &str = "/crash-matrix";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    ManifestPublish,
    SegmentPublish,
    SegmentThenManifest,
    PhysicalPurge,
    RefinementCheckpointPublish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedOutcome {
    PreviousOrNewManifest,
    PreviousManifestAndAbsentOrValidSegment,
    NoManifestMayReferenceAMissingSegment,
    PurgeCompletesOrRestarts,
    RefinementCheckpointAbsentOrValid,
}

#[derive(Clone, Copy, Debug)]
struct CrashCase {
    name: &'static str,
    scenario: Scenario,
    expected: ExpectedOutcome,
    expected_operations: usize,
}

const CASES: &[CrashCase] = &[
    CrashCase {
        name: "commit_manifest_previous_or_new",
        scenario: Scenario::ManifestPublish,
        expected: ExpectedOutcome::PreviousOrNewManifest,
        expected_operations: 4,
    },
    CrashCase {
        name: "write_segment_absent_or_complete",
        scenario: Scenario::SegmentPublish,
        expected: ExpectedOutcome::PreviousManifestAndAbsentOrValidSegment,
        expected_operations: 4,
    },
    CrashCase {
        name: "segment_before_referencing_manifest",
        scenario: Scenario::SegmentThenManifest,
        expected: ExpectedOutcome::NoManifestMayReferenceAMissingSegment,
        expected_operations: 8,
    },
    CrashCase {
        name: "physical_purge_manifest_unlink_wal_intent",
        scenario: Scenario::PhysicalPurge,
        expected: ExpectedOutcome::PurgeCompletesOrRestarts,
        expected_operations: 16,
    },
    CrashCase {
        name: "refinement_checkpoint_absent_or_valid",
        scenario: Scenario::RefinementCheckpointPublish,
        expected: ExpectedOutcome::RefinementCheckpointAbsentOrValid,
        expected_operations: 4,
    },
];

struct Log;

impl DurableLog for Log {
    fn durable_end(&self) -> u64 {
        u64::MAX
    }
}

fn directory() -> &'static Path {
    Path::new(DIRECTORY)
}

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered)
        .expect("ordered durability policy")
}

fn schema() -> Schema {
    Schema::new(Vec::new()).expect("schema")
}

fn manifest(generation: u64, mut segments: Vec<SegmentMeta>) -> Manifest {
    let marker = if generation == 1 { 'a' } else { 'b' };
    let epochs = (0_u64..8)
        .map(|index| {
            let document = EmbeddingTower {
                model_id: format!(
                    "{}-{index}",
                    std::iter::repeat_n(marker, 48).collect::<String>()
                ),
                model_version: generation.to_string(),
                weights_digest: vec![marker as u8, index as u8],
                dims: 3,
                normalization: Normalization::L2,
                prompt_prefix: "search_document: ".to_owned(),
                max_tokens: 512,
                runtime: EmbeddingRuntime::CpuReference,
                compute_units: ComputeUnits::Cpu,
                os_build: None,
            };
            let mut query = document.clone();
            query.prompt_prefix = "search_query: ".to_owned();
            EpochMeta::from(StoreEpoch {
                embedding: EmbeddingEpoch {
                    document,
                    query,
                    alignment_digest: Vec::new(),
                },
                tokenizer: TokenizerConfig::text_default().epoch(),
            })
        })
        .collect::<Vec<_>>();
    let epoch_alias = epochs
        .first()
        .map(EpochIdentity::from_meta)
        .transpose()
        .expect("generated epoch metadata");
    for segment in &mut segments {
        segment.epoch_id = epoch_alias.map(|identity| identity.embedding);
    }
    Manifest {
        generation,
        log_seq: generation,
        segments,
        epochs,
        epoch_alias,
        schema: schema(),
    }
}

fn segment_bytes(id: SegmentId) -> (Vec<u8>, SegmentMeta) {
    let columns = ColumnStoreBuilder::new(schema()).finish().expect("columns");
    let alive = AliveSet::new(0);
    let bytes = encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 3,
        codes: &[],
        factors: SegmentFactors::Bit4(&[]),
        rescore: &[],
        columns: &columns,
        alive: &alive,
    })
    .expect("segment bytes");
    let meta = validate_segment_bytes(&bytes).expect("segment validates");
    (bytes, meta)
}

fn publish_segment(recorder: &CrashVfs, id: SegmentId) -> SegmentMeta {
    let columns = ColumnStoreBuilder::new(schema()).finish().expect("columns");
    let alive = AliveSet::new(0);
    write_segment(
        recorder,
        directory(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 3,
            codes: &[],
            factors: SegmentFactors::Bit4(&[]),
            rescore: &[],
            columns: &columns,
            alive: &alive,
        },
        ordered_policy(),
    )
    .expect("segment publish")
}

fn seed_previous_store() -> (MemoryVfs, Manifest, SegmentMeta) {
    let store = MemoryVfs::new();
    let old_id = SegmentId::new(1, [1; 10]);
    let (old_bytes, old_meta) = segment_bytes(old_id);
    store
        .insert(directory().join(old_id.file_name()), old_bytes)
        .expect("old segment");
    let old_manifest = manifest(1, vec![old_meta.clone()]);
    store
        .insert(
            directory().join(MANIFEST_FILE),
            encode_manifest(&old_manifest).expect("old manifest bytes"),
        )
        .expect("old manifest");
    (store, old_manifest, old_meta)
}

fn assert_manifest_is_exactly_old_or_new(
    case: CrashCase,
    state: &CrashState,
    operation_count: usize,
    old: &Manifest,
    new: &Manifest,
) -> Manifest {
    let path = directory().join(MANIFEST_FILE);
    match load_manifest(state.vfs(), &path, u64::MAX) {
        Ok(actual) => {
            assert!(
                actual == *old || actual == *new,
                "{} {:?}: manifest generation {} is neither previous {} nor new {}",
                case.name,
                state.kind(),
                actual.generation,
                old.generation,
                new.generation
            );
            assert!(
                !state.includes_complete_operation_sequence(operation_count) || actual == *new,
                "{} {:?}: successfully returned manifest commit did not publish generation {}",
                case.name,
                state.kind(),
                new.generation
            );
            actual
        }
        Err(error) => panic!(
            "{} {:?}: committed manifest did not leave the previous manifest usable: {}",
            case.name,
            state.kind(),
            typed_manifest_error(&error)
        ),
    }
}

fn typed_manifest_error(error: &ManifestError) -> String {
    match error {
        ManifestError::Io { source, .. } => format!("Io({:?})", source.kind()),
        ManifestError::Format(format) => format!("Format({:?})", format.check()),
        ManifestError::Decode(detail) => format!("Decode({detail})"),
        ManifestError::AheadOfLog { snapshot, durable } => {
            format!("AheadOfLog({snapshot}, {durable})")
        }
        ManifestError::Segment(error) => format!("Segment({error})"),
        ManifestError::UnknownEpochAlias { alias } => {
            format!(
                "UnknownEpochAlias({}, {})",
                alias.embedding, alias.tokenizer
            )
        }
        ManifestError::UnknownSegmentEpoch { segment, epoch } => {
            format!("UnknownSegmentEpoch({segment}, {epoch})")
        }
    }
}

fn assert_segment_absent_or_fully_valid(
    case: CrashCase,
    state: &CrashState,
    operation_count: usize,
    meta: &SegmentMeta,
) {
    let path = directory().join(meta.id.file_name());
    match state.vfs().read(&path) {
        Ok(bytes) => {
            let actual = validate_segment_bytes(&bytes).unwrap_or_else(|error| {
                panic!(
                    "{} {:?}: published segment {} is corrupt: {error}",
                    case.name,
                    state.kind(),
                    meta.id
                )
            });
            assert_eq!(
                actual,
                *meta,
                "{} {:?}: segment metadata",
                case.name,
                state.kind()
            );
        }
        Err(error) => {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::NotFound,
                "{} {:?}: segment read error {error}",
                case.name,
                state.kind()
            );
            assert!(
                !state.includes_complete_operation_sequence(operation_count),
                "{} {:?}: successfully returned segment publish did not publish {}",
                case.name,
                state.kind(),
                meta.id
            );
        }
    }
}

fn assert_temps_are_never_committed_names(case: CrashCase, state: &CrashState) {
    for path in state.vfs().list(directory()).expect("list crash state") {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 fixture path");
        if name.starts_with('.') {
            assert!(
                name == MANIFEST_TEMP_FILE
                    || name.ends_with(".zseg.tmp")
                    || name == ".wal.ze.purge.tmp"
                    || name == ".purge.ze.tmp",
                "{} {:?}: unexpected temp name {name}",
                case.name,
                state.kind()
            );
            assert_ne!(
                name,
                MANIFEST_FILE,
                "{} {:?}: temp became committed manifest",
                case.name,
                state.kind()
            );
            assert!(
                !name.starts_with("segment-") || !name.ends_with(".zseg"),
                "{} {:?}: temp became a committed segment name",
                case.name,
                state.kind()
            );
        }
    }
}

fn class_counts(states: &CrashStates) -> BTreeMap<CrashStateClass, usize> {
    let mut counts = BTreeMap::new();
    for state in states.iter() {
        *counts.entry(state.kind().class()).or_insert(0) += 1;
    }
    counts
}

fn assert_full_uncapped_coverage(case: CrashCase, recorder: &CrashVfs, states: &CrashStates) {
    let operations = recorder.operations().expect("operations");
    assert!(!states.is_empty(), "{} enumerated no states", case.name);
    assert_eq!(
        operations.len(),
        case.expected_operations,
        "{} recorded operation count",
        case.name
    );
    assert!(!states.was_capped(), "{} hit crash-state cap", case.name);
    let counts = class_counts(states);
    let prefixes = counts.get(&CrashStateClass::Prefix).copied().unwrap_or(0);
    assert_eq!(
        prefixes,
        operations.len() + 1,
        "{} exact-prefix count",
        case.name
    );
    let byte_operations = operations
        .iter()
        .filter(|operation| {
            matches!(
                operation,
                CrashOperation::Write { bytes, .. } | CrashOperation::Append { bytes, .. }
                    if bytes.len() > 1
            )
        })
        .count();
    let torn = counts
        .get(&CrashStateClass::TornWrite)
        .copied()
        .unwrap_or(0);
    let torn_prefix = states
        .iter()
        .filter(|state| {
            matches!(
                state.kind(),
                CrashStateKind::TornWrite {
                    edge: TornWriteEdge::Prefix,
                    ..
                }
            )
        })
        .count();
    let torn_suffix = torn.saturating_sub(torn_prefix);
    let garbage = counts
        .get(&CrashStateClass::ExtendedWithGarbage)
        .copied()
        .unwrap_or(0);
    let zeros = counts
        .get(&CrashStateClass::ExtendedWithZeros)
        .copied()
        .unwrap_or(0);
    let interior = counts
        .get(&CrashStateClass::InteriorDamage)
        .copied()
        .unwrap_or(0);
    assert_eq!(torn_prefix, torn_suffix, "{} torn edge balance", case.name);
    assert_eq!(torn, garbage.saturating_mul(2), "{} torn count", case.name);
    assert_eq!(garbage, zeros, "{} garbage/zero point count", case.name);
    assert_eq!(
        garbage, interior,
        "{} garbage/interior point count",
        case.name
    );
    assert!(
        garbage > 0,
        "{} exercised no full-length corruption",
        case.name
    );
    assert!(
        garbage <= byte_operations.saturating_mul(MAX_TEAR_POINTS_PER_OPERATION),
        "{} exceeded semantic-point bound: {} points for {} byte operations",
        case.name,
        garbage,
        byte_operations
    );
    let reordered = counts
        .get(&CrashStateClass::ReorderedWrites)
        .copied()
        .unwrap_or(0);
    let rename_old = counts
        .get(&CrashStateClass::RenameWithOldContent)
        .copied()
        .unwrap_or(0);
    assert_eq!(reordered, 0, "{} unexpected reorder state", case.name);
    assert_eq!(rename_old, 0, "{} unsafe rename/content state", case.name);
    assert_eq!(
        states.len(),
        prefixes + torn + garbage + zeros + interior + reordered + rename_old,
        "{} classified state count",
        case.name
    );
    eprintln!(
        "crash_matrix protocol={} states={} prefixes={} torn_prefix={} torn_suffix={} garbage={} zeros={} interior={} reordered={} rename_old={} cap=4096 capped={}",
        case.name,
        states.len(),
        prefixes,
        torn_prefix,
        torn_suffix,
        garbage,
        zeros,
        interior,
        reordered,
        rename_old,
        states.was_capped()
    );
}

fn run_manifest_case(case: CrashCase) {
    assert_eq!(case.scenario, Scenario::ManifestPublish);
    assert_eq!(case.expected, ExpectedOutcome::PreviousOrNewManifest);
    let (store, old_manifest, old_meta) = seed_previous_store();
    let recorder = CrashVfs::new(store).expect("recorder");
    let new_manifest = manifest(2, vec![old_meta]);
    commit_manifest(&recorder, directory(), &new_manifest, ordered_policy())
        .expect("manifest commit");
    let states = recorder.crash_states().expect("crash states");
    let operation_count = recorder.operations().expect("operations").len();
    assert_full_uncapped_coverage(case, &recorder, &states);
    for state in states.iter() {
        assert_temps_are_never_committed_names(case, state);
        let _ = assert_manifest_is_exactly_old_or_new(
            case,
            state,
            operation_count,
            &old_manifest,
            &new_manifest,
        );
        open_manifest(state.vfs(), directory(), &Log, &HashSet::new()).unwrap_or_else(|error| {
            panic!(
                "{} {:?}: reopen required manual intervention: {}",
                case.name,
                state.kind(),
                typed_manifest_error(&error)
            )
        });
    }
}

fn run_segment_case(case: CrashCase) {
    assert_eq!(case.scenario, Scenario::SegmentPublish);
    assert_eq!(
        case.expected,
        ExpectedOutcome::PreviousManifestAndAbsentOrValidSegment
    );
    let (store, old_manifest, _) = seed_previous_store();
    let recorder = CrashVfs::new(store).expect("recorder");
    let new_id = SegmentId::new(2, [2; 10]);
    let new_meta = publish_segment(&recorder, new_id);
    let states = recorder.crash_states().expect("crash states");
    let operation_count = recorder.operations().expect("operations").len();
    assert_full_uncapped_coverage(case, &recorder, &states);
    for state in states.iter() {
        assert_temps_are_never_committed_names(case, state);
        let loaded = load_manifest(state.vfs(), &directory().join(MANIFEST_FILE), u64::MAX)
            .unwrap_or_else(|error| {
                panic!(
                    "{} {:?}: previous manifest unusable: {}",
                    case.name,
                    state.kind(),
                    typed_manifest_error(&error)
                )
            });
        assert_eq!(
            loaded,
            old_manifest,
            "{} {:?}: manifest changed",
            case.name,
            state.kind()
        );
        assert_segment_absent_or_fully_valid(case, state, operation_count, &new_meta);
        open_manifest(state.vfs(), directory(), &Log, &HashSet::new()).unwrap_or_else(|error| {
            panic!(
                "{} {:?}: reopen required manual intervention: {}",
                case.name,
                state.kind(),
                typed_manifest_error(&error)
            )
        });
    }
}

fn run_combined_case(case: CrashCase) {
    assert_eq!(case.scenario, Scenario::SegmentThenManifest);
    assert_eq!(
        case.expected,
        ExpectedOutcome::NoManifestMayReferenceAMissingSegment
    );
    let (store, old_manifest, old_meta) = seed_previous_store();
    let recorder = CrashVfs::new(store).expect("recorder");
    let new_id = SegmentId::new(3, [3; 10]);
    let new_meta = publish_segment(&recorder, new_id);
    let new_manifest = manifest(2, vec![old_meta, new_meta.clone()]);
    commit_manifest(&recorder, directory(), &new_manifest, ordered_policy())
        .expect("manifest commit");
    let states = recorder.crash_states().expect("crash states");
    let operation_count = recorder.operations().expect("operations").len();
    assert_full_uncapped_coverage(case, &recorder, &states);
    for state in states.iter() {
        assert_temps_are_never_committed_names(case, state);
        let loaded = assert_manifest_is_exactly_old_or_new(
            case,
            state,
            operation_count,
            &old_manifest,
            &new_manifest,
        );
        if loaded == new_manifest {
            let bytes = state
                .vfs()
                .read(&directory().join(new_id.file_name()))
                .unwrap_or_else(|error| {
                    panic!(
                        "{} {:?}: new manifest references missing segment {}: {error}",
                        case.name,
                        state.kind(),
                        new_id
                    )
                });
            assert_eq!(
                validate_segment_bytes(&bytes).expect("referenced segment validates"),
                new_meta,
                "{} {:?}: referenced segment metadata",
                case.name,
                state.kind()
            );
        }
        open_manifest(state.vfs(), directory(), &Log, &HashSet::<PathBuf>::new()).unwrap_or_else(
            |error| {
                panic!(
                    "{} {:?}: reopen required manual intervention: {}",
                    case.name,
                    state.kind(),
                    typed_manifest_error(&error)
                )
            },
        );
    }
}

#[test]
fn commit_manifest_crash_matrix() {
    run_manifest_case(CASES[0]);
}

#[test]
fn write_segment_crash_matrix() {
    run_segment_case(CASES[1]);
}

#[test]
fn segment_then_manifest_crash_matrix() {
    run_combined_case(CASES[2]);
}

#[test]
fn refinement_checkpoint_crash_matrix() {
    let case = CASES[4];
    assert_eq!(case.scenario, Scenario::RefinementCheckpointPublish);
    assert_eq!(
        case.expected,
        ExpectedOutcome::RefinementCheckpointAbsentOrValid
    );
    let store = MemoryVfs::new();
    let recorder = CrashVfs::new(store).expect("refinement checkpoint recorder");
    let committed = directory().join(".tier-refinement.renumber.refine.checkpoint");
    let temporary = directory().join(".tier-refinement.renumber.refine.checkpoint.tmp");
    let bytes = valid_refinement_checkpoint_bytes();
    validate_refinement_checkpoint(&bytes).expect("fixture refinement checkpoint validates");
    recorder
        .write(&temporary, &bytes)
        .expect("refinement checkpoint temp write");
    recorder
        .sync(&temporary, SyncKind::Barrier)
        .expect("refinement checkpoint temp sync");
    recorder
        .rename(&temporary, &committed)
        .expect("refinement checkpoint rename");
    recorder
        .sync(directory(), SyncKind::Barrier)
        .expect("refinement checkpoint directory sync");
    let states = recorder
        .crash_states()
        .expect("refinement checkpoint crash states");
    assert_full_uncapped_coverage(case, &recorder, &states);
    for state in states.iter() {
        match state.vfs().read(&committed) {
            Ok(committed_bytes) => {
                validate_refinement_checkpoint(&committed_bytes).unwrap_or_else(|error| {
                    panic!(
                        "{} {:?}: committed refinement checkpoint is invalid: {error}",
                        case.name,
                        state.kind()
                    )
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!(
                "{} {:?}: committed checkpoint read failed: {error}",
                case.name,
                state.kind()
            ),
        }
    }
}

fn valid_refinement_checkpoint_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"ZEREFCP1");
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.push(0);
    bytes.push(1);
    bytes.push(1);
    bytes.push(1);
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&19_u64.to_le_bytes());
    bytes.extend_from_slice(&1.2_f32.to_bits().to_le_bytes());
    bytes.extend_from_slice(&[0x19; 16]);
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&7_u64.to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes.extend_from_slice(&6_u64.to_le_bytes());
    bytes.extend_from_slice(&33_u64.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.push(1);
    bytes.push(0);
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(&0_f64.to_bits().to_le_bytes());
    bytes.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&bytes).to_le_bytes());
    bytes
}

#[test]
fn a_crash_mid_purge_reopens_and_completes_or_restarts_cleanly() {
    let case = CASES[3];
    assert_eq!(case.scenario, Scenario::PhysicalPurge);
    assert_eq!(case.expected, ExpectedOutcome::PurgeCompletesOrRestarts);
    let old_id = SegmentId::new(11, [0x11; 10]);
    let new_id = SegmentId::new(12, [0x12; 10]);
    let sentinel = 0x3f12_34ab_u32.to_le_bytes();
    let (old_bytes, old_meta) = one_row_segment_bytes(old_id, f32::from_bits(0x3f12_34ab));
    let (new_bytes, new_meta) = segment_bytes(new_id);
    let old_manifest = Manifest {
        generation: 1,
        log_seq: 1,
        segments: vec![old_meta.clone()],
        epochs: Vec::new(),
        epoch_alias: None,
        schema: schema(),
    };
    let new_manifest = Manifest {
        generation: 2,
        log_seq: 1,
        segments: vec![new_meta.clone()],
        epochs: Vec::new(),
        epoch_alias: None,
        schema: schema(),
    };
    let old_wal = purge_wal_with_sentinel(&sentinel);
    let scrubbed_wal = encode_wal_header(LogSeq::new(2)).expect("scrubbed WAL header");
    let store = MemoryVfs::new();
    store
        .insert(directory().join(old_id.file_name()), old_bytes)
        .expect("old purge segment");
    store
        .insert(
            directory().join(MANIFEST_FILE),
            encode_manifest(&old_manifest).expect("old purge manifest"),
        )
        .expect("old purge manifest");
    store
        .insert(directory().join("wal.ze"), old_wal)
        .expect("old purge WAL");
    store
        .insert(
            directory().join("purge.ze"),
            purge_intent_bytes(0x1020_3040_5060_7080),
        )
        .expect("committed purge intent");
    let recorder = CrashVfs::new(store).expect("purge recorder");

    publish_encoded_segment(&recorder, new_id, &new_bytes);
    commit_manifest(&recorder, directory(), &new_manifest, ordered_policy())
        .expect("purge manifest commit");
    recorder
        .delete(&directory().join(old_id.file_name()))
        .expect("purge old segment unlink");
    recorder
        .sync(directory(), SyncKind::Barrier)
        .expect("purge unlink directory sync");
    let wal_temp = directory().join(".wal.ze.purge.tmp");
    recorder
        .write(&wal_temp, &scrubbed_wal)
        .expect("purge WAL temp");
    recorder
        .sync(&wal_temp, SyncKind::Barrier)
        .expect("purge WAL temp sync");
    recorder
        .rename(&wal_temp, &directory().join("wal.ze"))
        .expect("purge WAL commit");
    recorder
        .sync(directory(), SyncKind::Barrier)
        .expect("purge WAL directory sync");
    recorder
        .delete(&directory().join("purge.ze"))
        .expect("purge intent removal");
    recorder
        .sync(directory(), SyncKind::Barrier)
        .expect("purge resolution directory sync");

    let states = recorder.crash_states().expect("purge crash states");
    assert_full_uncapped_coverage(case, &recorder, &states);
    for state in states.iter() {
        recover_purge_state(state, &new_bytes, &new_manifest, old_id, &scrubbed_wal);
        let opened = open_manifest(state.vfs(), directory(), &Log, &HashSet::new()).unwrap_or_else(
            |error| {
                panic!(
                    "{} {:?}: recovered purge is not openable: {}",
                    case.name,
                    state.kind(),
                    typed_manifest_error(&error)
                )
            },
        );
        assert_eq!(opened.manifest, new_manifest);
        let wal = WalReader::open(state.vfs(), &directory().join("wal.ze"))
            .expect("recovered purge WAL opens");
        assert_eq!(wal.durable_end(), 1);
        assert!(
            state
                .vfs()
                .list(directory())
                .expect("list recovered purge")
                .into_iter()
                .all(|path| state.vfs().read(&path).map_or(true, |bytes| !bytes
                    .windows(sentinel.len())
                    .any(|value| value == sentinel))),
            "{} {:?}: sentinel survived recovered purge",
            case.name,
            state.kind()
        );
    }

    let real_directory = tempdir().expect("real recovery directory");
    let real_store = Store::open(real_directory.path(), OpenOptions::default())
        .expect("open real recovery store");
    let real_id = DocId::new(0xfeed);
    real_store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(real_id, Revision::new(1)),
            vec![f32::from_bits(0x3f12_34ab), 0.0, 0.0],
        )]))
        .expect("ingest real recovery sentinel");
    real_store.seal().expect("seal real recovery sentinel");
    let _pending = real_store
        .purge(&[real_id])
        .expect("schedule real recovery purge");
    drop(real_store);

    let reopened = Store::open(real_directory.path(), OpenOptions::default())
        .expect("reopen and complete real pending purge");
    for entry in std::fs::read_dir(real_directory.path()).expect("list real recovered store") {
        let path = entry.expect("real recovered entry").path();
        if path.is_file() {
            let bytes = std::fs::read(&path).expect("read real recovered artifact");
            assert!(
                !bytes.windows(sentinel.len()).any(|value| value == sentinel),
                "real Store::open recovery left sentinel in {}",
                path.display()
            );
        }
    }
    drop(reopened);
}

fn one_row_segment_bytes(id: SegmentId, value: f32) -> (Vec<u8>, SegmentMeta) {
    let mut columns = ColumnStoreBuilder::new(schema());
    columns.push_row(7, &[]).expect("one purge row");
    let columns = columns.finish().expect("purge columns");
    let alive = AliveSet::new(1);
    let vector = [value, 0.0, 0.0];
    let mut codes = [0_u8; 2];
    let factor = zeppelin_embed::quant::quantize_bit4(&vector, &mut codes)
        .expect("purge vector quantization");
    let bytes = encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 3,
        codes: &codes,
        factors: SegmentFactors::Bit4(&[factor]),
        rescore: &vector,
        columns: &columns,
        alive: &alive,
    })
    .expect("one-row purge segment");
    let meta = validate_segment_bytes(&bytes).expect("one-row purge segment validates");
    (bytes, meta)
}

fn purge_wal_with_sentinel(sentinel: &[u8; 4]) -> Vec<u8> {
    let mut bytes = encode_wal_header(LogSeq::new(1)).expect("old WAL header");
    bytes.extend_from_slice(
        &encode_record(WalRecord {
            seq: LogSeq::new(1),
            op: 99,
            payload: sentinel,
        })
        .expect("old sentinel WAL record"),
    );
    bytes
}

fn purge_intent_bytes(token_id: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&token_id.to_le_bytes());
    payload.extend_from_slice(&1_u32.to_le_bytes());
    payload.extend_from_slice(&0_u32.to_le_bytes());
    payload.extend_from_slice(&0xfeed_u128.to_le_bytes());
    encode_artifact(FormatFamily::PurgeIntent, 0, &payload)
}

fn publish_encoded_segment(vfs: &dyn Vfs, id: SegmentId, bytes: &[u8]) {
    let temporary = directory().join(format!(".{}.tmp", id.file_name()));
    let committed = directory().join(id.file_name());
    vfs.write(&temporary, bytes).expect("purge segment temp");
    vfs.sync(&temporary, SyncKind::Barrier)
        .expect("purge segment temp sync");
    vfs.rename(&temporary, &committed)
        .expect("purge segment commit");
    vfs.sync(directory(), SyncKind::Barrier)
        .expect("purge segment directory sync");
}

fn recover_purge_state(
    state: &CrashState,
    new_segment_bytes: &[u8],
    new_manifest: &Manifest,
    old_id: SegmentId,
    scrubbed_wal: &[u8],
) {
    let vfs = state.vfs();
    let manifest_path = directory().join(MANIFEST_FILE);
    let loaded = load_manifest(vfs, &manifest_path, u64::MAX).expect("crash manifest prefix");
    if loaded != *new_manifest {
        let new_id = new_manifest.segments[0].id;
        publish_encoded_segment(vfs, new_id, new_segment_bytes);
        commit_manifest(vfs, directory(), new_manifest, ordered_policy())
            .expect("restart purge manifest");
    }
    delete_if_exists(vfs, &directory().join(old_id.file_name()));
    let wal_temp = directory().join(".wal.ze.purge.tmp");
    vfs.write(&wal_temp, scrubbed_wal)
        .expect("restart purge WAL temp");
    vfs.sync(&wal_temp, SyncKind::Barrier)
        .expect("restart purge WAL sync");
    vfs.rename(&wal_temp, &directory().join("wal.ze"))
        .expect("restart purge WAL commit");
    vfs.sync(directory(), SyncKind::Barrier)
        .expect("restart purge WAL directory sync");
    delete_if_exists(vfs, &directory().join("purge.ze"));
    delete_if_exists(vfs, &directory().join(MANIFEST_TEMP_FILE));
    vfs.sync(directory(), SyncKind::Barrier)
        .expect("restart purge resolution sync");
}

fn delete_if_exists(vfs: &dyn Vfs, path: &Path) {
    match vfs.open(path) {
        Ok(_) => vfs.delete(path).expect("delete recovery artifact"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("open recovery artifact {}: {error}", path.display()),
    }
}

#[test]
fn new_corruption_classes_reach_checksum_checks_for_both_protocols() {
    let (manifest_store, _, old_meta) = seed_previous_store();
    let manifest_recorder = CrashVfs::new(manifest_store).expect("manifest recorder");
    commit_manifest(
        &manifest_recorder,
        directory(),
        &manifest(2, vec![old_meta]),
        ordered_policy(),
    )
    .expect("manifest commit");
    let manifest_states = manifest_recorder.crash_states().expect("manifest states");

    let (segment_store, _, _) = seed_previous_store();
    let segment_recorder = CrashVfs::new(segment_store).expect("segment recorder");
    let segment_id = SegmentId::new(4, [4; 10]);
    let _ = publish_segment(&segment_recorder, segment_id);
    let segment_states = segment_recorder.crash_states().expect("segment states");

    let mut reached = Vec::new();
    for state in manifest_states.iter() {
        let label = match state.kind() {
            CrashStateKind::ExtendedWithGarbage {
                operation_index: 0,
                intact_prefix_bytes: 40,
            } => Some("manifest ExtendedWithGarbage(op=0,intact=40)"),
            CrashStateKind::ExtendedWithZeros {
                operation_index: 0,
                intact_prefix_bytes: 40,
            } => Some("manifest ExtendedWithZeros(op=0,intact=40)"),
            CrashStateKind::InteriorDamage {
                operation_index: 0,
                damage_start: 40,
                ..
            } => Some("manifest InteriorDamage(op=0,start=40)"),
            _ => None,
        };
        if let Some(label) = label {
            let bytes = state
                .vfs()
                .read(&directory().join(MANIFEST_TEMP_FILE))
                .expect("damaged manifest temp");
            let check: FormatCheck = match decode_manifest("damaged-manifest-temp", &bytes) {
                Err(ManifestError::Format(error)) => error.check(),
                result => panic!("{label}: expected format rejection, got {result:?}"),
            };
            reached.push(format!("{label} -> {check:?}"));
        }
    }
    let segment_temp = directory().join(format!(".{}.tmp", segment_id.file_name()));
    for state in segment_states.iter() {
        let label = match state.kind() {
            CrashStateKind::ExtendedWithGarbage {
                operation_index: 0,
                intact_prefix_bytes: 16_384,
            } => Some("segment ExtendedWithGarbage(op=0,intact=16384)"),
            CrashStateKind::ExtendedWithZeros {
                operation_index: 0,
                intact_prefix_bytes: 16_384,
            } => Some("segment ExtendedWithZeros(op=0,intact=16384)"),
            CrashStateKind::InteriorDamage {
                operation_index: 0,
                damage_start: 16_384,
                ..
            } => Some("segment InteriorDamage(op=0,start=16384)"),
            _ => None,
        };
        if let Some(label) = label {
            let bytes = state
                .vfs()
                .read(&segment_temp)
                .expect("damaged segment temp");
            let check: FormatCheck = match validate_segment_bytes(&bytes) {
                Err(SegmentError::Format(error)) => error.check(),
                result => panic!("{label}: expected format rejection, got {result:?}"),
            };
            reached.push(format!("{label} -> {check:?}"));
        }
    }

    assert_eq!(
        reached,
        vec![
            "manifest ExtendedWithGarbage(op=0,intact=40) -> FileChecksum",
            "manifest ExtendedWithZeros(op=0,intact=40) -> FileChecksum",
            "manifest InteriorDamage(op=0,start=40) -> FileChecksum",
            "segment ExtendedWithGarbage(op=0,intact=16384) -> BlockChecksum",
            "segment ExtendedWithZeros(op=0,intact=16384) -> BlockChecksum",
            "segment InteriorDamage(op=0,start=16384) -> BlockChecksum",
        ],
        "new corruption classes must prove checksum-path reachability"
    );
}
